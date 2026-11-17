#![cfg(target_os = "linux")]

use std::fs;
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

struct LiveCluster {
    name: String,
    evidence: PathBuf,
    data: PathBuf,
    workspace: PathBuf,
    path: String,
}

// Kill the complete CLI process group on timeout or unwind, including any SSH
// children. Only processes launched by this test belong to this group.
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", self.0.id())])
                .status();
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl LiveCluster {
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kalisto"));
        command
            .current_dir(&self.workspace)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", self.evidence.join("config"))
            .env("PATH", &self.path)
            .env("RUST_LOG", "info")
            .process_group(0);
        command
    }

    fn run(&self, mut command: Command, label: &str, deadline: Instant) -> Vec<u8> {
        assert!(
            Instant::now() < deadline,
            "deadline exhausted before {label}"
        );
        let stdout = self.evidence.join(format!("{label}.stdout"));
        let stderr = self.evidence.join(format!("{label}.stderr"));
        command
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
        let mut child = Running(command.spawn().expect("spawn product CLI"));
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{label} failed ({status}); evidence: {}",
                    self.evidence.display()
                );
                assert!(Instant::now() < deadline, "{label} exceeded deadline");
                return fs::read(stdout).unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "{label} exceeded deadline; evidence: {}",
                self.evidence.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn cli(&self, args: &[&str], label: &str, deadline: Instant) -> Vec<u8> {
        let mut command = self.command();
        command.args(args).args(["--name", &self.name]);
        self.run(command, label, deadline)
    }

    fn kubectl(&self, args: &[&str], label: &str, deadline: Instant) -> Vec<u8> {
        let mut command = self.command();
        command
            .args([
                "kubectl",
                "--name",
                &self.name,
                "--",
                "--request-timeout=5s",
            ])
            .args(args);
        self.run(command, label, deadline)
    }

    fn guest_diagnostics(&self, runtime: &Path, index: usize) {
        let rootless = fs::read_to_string(runtime.join("network-mode"))
            .is_ok_and(|mode| matches!(mode.trim(), "slirp" | "slirp4netns"));
        let host = if rootless {
            "127.0.0.1".to_owned()
        } else {
            fs::read_to_string(runtime.join("guest-ip"))
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        if host.is_empty() {
            return;
        }
        let mut ssh = Command::new("ssh");
        ssh.env("PATH", &self.path)
            .process_group(0)
            .args([
                "-F",
                "/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=3",
                "-i",
            ])
            .arg(runtime.join("id_ed25519"));
        if rootless {
            let Ok(port) = fs::read_to_string(runtime.join("ssh-port")) else {
                return;
            };
            ssh.args(["-p", port.trim()]);
        }
        ssh.arg(format!("ubuntu@{host}")).arg(
            "sudo sh -c 'echo ===K3S===; journalctl --no-pager -n 300 -u k3s -u k3s-agent 2>/dev/null; \
             for f in /var/log/k3s*.log /var/lib/rancher/k3s/agent/containerd/containerd.log; do \
             if [ -f \"$f\" ]; then echo ===$f===; tail -n 300 \"$f\"; fi; done; \
             echo ===CONTAINER_LOGS===; find /var/log/pods -type f -name \"*.log\" \
             \\( -path \"*cilium*\" -o -path \"*coredns*\" \\) -exec tail -n 200 {} \\; 2>/dev/null; \
             echo ===CONTAINERD_TASKS===; /usr/local/bin/k3s crictl ps -a 2>/dev/null; \
             echo ===NETWORK===; ip addr; ip route'"
        );
        self.run(
            ssh,
            &format!("guest-{index}-diagnostics"),
            Instant::now() + Duration::from_secs(12),
        );
    }

    fn nodes(&self, count: usize, label: &str, deadline: Instant) -> Value {
        let nodes: Value =
            serde_json::from_slice(&self.kubectl(&["get", "nodes", "-o", "json"], label, deadline))
                .unwrap();
        let items = nodes["items"].as_array().unwrap();
        assert_eq!(items.len(), count);
        assert!(items.iter().all(|node| {
            node["status"]["conditions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|condition| condition["type"] == "Ready" && condition["status"] == "True")
        }));
        nodes
    }
}

impl Drop for LiveCluster {
    fn drop(&mut self) {
        // Preserve evidence even on assertion failure, before destroying only the
        // named cluster in this test's private XDG roots. Never touch user state.
        let cluster = self.data.join("kalisto/clusters").join(&self.name);
        if !cluster.exists() {
            return;
        }
        let mut guests = Vec::new();
        guest_runtime_directories(&cluster, &mut guests);
        for (index, runtime) in guests.iter().enumerate() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.guest_diagnostics(runtime, index);
            }));
        }
        for (label, args) in [
            ("final-pods", vec!["get", "pods", "-A", "-o", "json"]),
            ("final-events", vec!["get", "events", "-A", "-o", "json"]),
            (
                "final-probe-logs",
                vec!["logs", "-l", "app=kalisto-budget-probe", "--prefix=true"],
            ),
        ] {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.kubectl(&args, label, Instant::now() + Duration::from_secs(8))
            }));
        }
        let _ = fs::write(
            self.evidence.join("completion.json"),
            json!({
                "panicking": thread::panicking(), "cluster": self.name,
            })
            .to_string(),
        );
        preserve_logs(&cluster, &self.evidence.join("guest-logs"));
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.cli(
                &["destroy"],
                "destroy",
                Instant::now() + Duration::from_secs(120),
            )
        }));
        if cleanup.is_err() {
            eprintln!(
                "owned cluster cleanup failed; inspect {}",
                self.evidence.display()
            );
        }
    }
}

fn manifests(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            manifests(&path, found);
        } else if path.file_name().is_some_and(|name| name == "manifest.json") {
            let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if value.get("probe_image").is_some() && value.get("rollouts").is_some() {
                found.push(path);
            }
        }
    }
}

fn guest_runtime_directories(source: &Path, found: &mut Vec<PathBuf>) {
    if source.join("id_ed25519").is_file() {
        found.push(source.to_owned());
        return;
    }
    let Ok(entries) = fs::read_dir(source) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            guest_runtime_directories(&entry.path(), found);
        }
    }
}

fn preserve_logs(source: &Path, destination: &Path) {
    let Ok(entries) = fs::read_dir(source) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let target = destination.join(entry.file_name());
        if path.is_dir() {
            preserve_logs(&path, &target);
        } else if path.extension().is_some_and(|extension| extension == "log") {
            let _ = fs::create_dir_all(destination);
            let _ = fs::copy(path, target);
        }
    }
}

fn prepare_multi_endpoint_scale_service(
    live: &LiveCluster,
    workers: &[&str],
    image: &str,
    deadline: Instant,
) {
    assert_eq!(workers.len(), 2);
    let mut items = workers
        .iter()
        .enumerate()
        .map(|(index, worker)| {
            json!({
                "apiVersion": "v1", "kind": "Pod",
                "metadata": {
                    "name": format!("budget-scale-http-{index}"),
                    "labels": {"app": "budget-scale-http"},
                },
                "spec": {
                    "nodeName": worker,
                    "containers": [{
                        "name": "http", "image": image, "imagePullPolicy": "Never",
                        "command": ["sh", "-c", "mkdir -p /www; printf 'scale-ready\\n' >/www/index.html; exec httpd -f -p 8080 -h /www"],
                        "readinessProbe": {
                            "httpGet": {"path": "/", "port": 8080}, "periodSeconds": 1,
                        },
                    }],
                },
            })
        })
        .collect::<Vec<_>>();
    items.push(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"name": "budget-scale-http"},
        "spec": {
            "selector": {"app": "budget-scale-http"},
            "ports": [{"port": 8080, "targetPort": 8080}],
        },
    }));
    let path = live.evidence.join("scale-service.json");
    fs::write(
        &path,
        json!({"apiVersion": "v1", "kind": "List", "items": items}).to_string(),
    )
    .unwrap();
    live.cli(
        &["apply", "--manifest", path.to_str().unwrap()],
        "apply-scale-service",
        deadline,
    );
    live.kubectl(
        &[
            "wait",
            "--for=condition=Ready",
            "pod",
            "-l",
            "app=budget-scale-http",
            "--timeout=60s",
        ],
        "scale-service-pods-ready",
        deadline,
    );
    // A single endpoint never exercises kube-proxy's statistic match. The
    // additional worker must boot with a multi-endpoint Service already present.
    live.kubectl(
        &[
            "wait",
            "--for=jsonpath={.subsets[0].addresses[1].ip}",
            "endpoints/budget-scale-http",
            "--timeout=60s",
        ],
        "scale-service-two-ready-endpoints",
        deadline,
    );
}

#[expect(
    clippy::too_many_lines,
    reason = "one cold-start deadline covers the complete CLI lifecycle scenario"
)]
#[test]
fn fresh_cli_three_node_cilium_host_api_dns_and_service_under_sixty_seconds() {
    if std::env::var("KALISTO_LIVE_FIRECRACKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping live Firecracker test; set KALISTO_LIVE_FIRECRACKER_TESTS=1");
        return;
    }
    for variable in ["KALISTO_FIRECRACKER_KERNEL", "KALISTO_FIRECRACKER_ROOTFS"] {
        let path = PathBuf::from(
            std::env::var_os(variable)
                .unwrap_or_else(|| panic!("{variable} must name a local artifact")),
        );
        assert!(
            path.is_absolute() && path.is_file(),
            "{variable} must be an absolute local file path"
        );
    }
    let kernel = std::env::var("KALISTO_FIRECRACKER_KERNEL").unwrap();
    let rootfs = std::env::var("KALISTO_FIRECRACKER_ROOTFS").unwrap();
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    );
    let target = if target.is_absolute() {
        target
    } else {
        std::env::current_dir().unwrap().join(target)
    };
    fs::create_dir_all(&target).unwrap();
    let target = target.canonicalize().unwrap();
    let data = target.join("kalisto/firecracker-live-data");
    fs::create_dir_all(&data).unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("fc-budget-{}-{nonce}", std::process::id());
    let evidence = target.join("kalisto/firecracker-budget").join(&name);
    let workspace = evidence.join("empty-cwd");
    fs::create_dir_all(&workspace).unwrap();
    let evidence = evidence.canonicalize().unwrap();
    let live = LiveCluster {
        name,
        workspace: workspace.canonicalize().unwrap(),
        evidence,
        data,
        path: format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            home.join(".local/bin").display()
        ),
    };
    eprintln!("Firecracker evidence: {}", live.evidence.display());
    let mut version = Command::new("/bin/sh");
    version
        .env("PATH", &live.path)
        .process_group(0)
        .args(["-c", "command -v firecracker && exec firecracker --version"]);
    live.run(
        version,
        "firecracker-version",
        Instant::now() + Duration::from_secs(10),
    );
    assert!(
        !live.data.join("kalisto/clusters").join(&live.name).exists(),
        "fresh run must not reuse cluster state"
    );
    assert_eq!(fs::read_dir(&live.workspace).unwrap().count(), 0);
    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port().to_string();
    let args = [
        "--backend",
        "firecracker",
        "--workers",
        "2",
        "--addons",
        "cni/cilium",
        "--api-port",
        &port,
        "--firecracker-kernel",
        &kernel,
        "--firecracker-rootfs",
        &rootfs,
    ];
    let prepare_started = Instant::now();
    let mut pull = live.command();
    pull.arg("pull")
        .args(args)
        .args(["--name", &live.name, "--project-root"])
        .arg(&live.workspace);
    live.run(pull, "pull", Instant::now() + Duration::from_secs(1800));
    let preparation_seconds = prepare_started.elapsed().as_secs_f64();
    fs::write(
        live.evidence.join("preparation.json"),
        json!({"seconds": preparation_seconds}).to_string(),
    )
    .unwrap();
    assert!(
        !live.data.join("kalisto/clusters").join(&live.name).exists(),
        "pull must not create cluster or VM state"
    );
    let mut prepared = Vec::new();
    manifests(&live.data.join("kalisto/artifacts"), &mut prepared);
    assert_eq!(prepared.len(), 1, "one prepared Cilium profile is required");
    let assets = prepared[0].parent().unwrap();
    assert!(
        assets.join("firecracker-assets.ext4").is_file(),
        "offline Firecracker asset disk must be prepared before startup"
    );
    let manifest: Value = serde_json::from_slice(&fs::read(&prepared[0]).unwrap()).unwrap();
    let rollouts = manifest["rollouts"].as_array().unwrap();
    assert!(
        !rollouts.is_empty(),
        "Cilium rollout inventory must not be empty"
    );
    let probe_image = manifest["probe_image"].as_str().unwrap();
    assert!(
        probe_image.contains("busybox"),
        "probe must use cached BusyBox"
    );

    let started = Instant::now();
    let deadline = started + Duration::from_secs(60);
    drop(reservation);
    let mut up = live.command();
    up.arg("up")
        .args(args)
        .args(["--name", &live.name, "--project-root"])
        .arg(&live.workspace);
    live.run(up, "up", deadline);
    assert_eq!(
        String::from_utf8_lossy(&live.kubectl(&["get", "--raw=/readyz"], "readyz", deadline))
            .trim(),
        "ok"
    );
    let nodes = live.nodes(3, "nodes", deadline);
    for (index, rollout) in rollouts.iter().enumerate() {
        live.kubectl(
            &[
                "-n",
                rollout["namespace"].as_str().unwrap(),
                "rollout",
                "status",
                rollout["resource"].as_str().unwrap(),
                "--timeout=60s",
            ],
            &format!("cilium-{index}"),
            deadline,
        );
    }
    let control_plane = format!("kalisto-{}-cp", live.name);
    let workers = nodes["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["metadata"]["name"].as_str())
        .filter(|name| *name != control_plane)
        .collect::<Vec<_>>();
    assert_eq!(workers.len(), 2);
    let mut items = workers.iter().enumerate().map(|(index, worker)| json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": format!("budget-probe-{index}"), "labels": {"app": "kalisto-budget-probe"}},
        "spec": {"nodeName": worker, "restartPolicy": "Never", "containers": [{
            "name": "probe", "image": probe_image, "imagePullPolicy": "Never",
            "command": ["sh", "-c", "while :; do if nslookup kubernetes.default.svc.cluster.local && wget -q -T 1 -O - http://budget-http.default.svc.cluster.local:8080/ | grep -qx ready; then echo DNS_AND_CROSS_NODE_HTTP_OK; exit 0; fi; sleep 1; done"]
        }]}
    })).collect::<Vec<_>>();
    items.push(json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "budget-http", "labels": {"app": "budget-http"}},
        "spec": {"nodeName": control_plane, "containers": [{"name": "http", "image": probe_image, "imagePullPolicy": "Never",
            "command": ["sh", "-c", "mkdir -p /www; printf 'ready\\n' >/www/index.html; exec httpd -f -p 8080 -h /www"],
            "readinessProbe": {"httpGet": {"path": "/", "port": 8080}, "periodSeconds": 1}}]}}));
    items.push(json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "budget-http"},
        "spec": {"selector": {"app": "budget-http"}, "ports": [{"port": 8080, "targetPort": 8080}]}}));
    let probes = live.evidence.join("probes.json");
    fs::write(
        &probes,
        json!({"apiVersion": "v1", "kind": "List", "items": items}).to_string(),
    )
    .unwrap();
    live.kubectl(
        &["create", "-f", probes.to_str().unwrap()],
        "create-probes",
        deadline,
    );
    live.kubectl(
        &[
            "wait",
            "--for=jsonpath={.status.phase}=Succeeded",
            "pod",
            "-l",
            "app=kalisto-budget-probe",
            "--timeout=60s",
        ],
        "wait-probes",
        deadline,
    );
    let logs = live.kubectl(
        &["logs", "-l", "app=kalisto-budget-probe", "--prefix=true"],
        "probe-logs",
        deadline,
    );
    assert_eq!(
        String::from_utf8_lossy(&logs)
            .matches("DNS_AND_CROSS_NODE_HTTP_OK")
            .count(),
        2
    );
    let elapsed = started.elapsed();
    fs::write(live.evidence.join("result.json"), serde_json::to_vec_pretty(&json!({
        "preparation_seconds": preparation_seconds, "startup_and_probes_seconds": elapsed.as_secs_f64(),
        "budget_seconds": 60, "under_60_seconds": elapsed < Duration::from_secs(60),
        "ready_nodes": 3, "cni": "cilium", "prepared_rollouts": rollouts,
        "authenticated_host_api": true, "worker_dns_probes": 2, "worker_cross_node_http_probes": 2,
        "fresh_cluster": true, "isolated_xdg": true, "api_port": port,
    })).unwrap()).unwrap();
    eprintln!(
        "preparation={preparation_seconds:.3}s startup+functional-probes={:.3}s",
        elapsed.as_secs_f64()
    );
    assert!(elapsed < Duration::from_secs(60));

    // Persistence and scaling are deliberately outside the fresh-start budget.
    let maintenance = Instant::now() + Duration::from_secs(300);
    live.kubectl(
        &[
            "create",
            "configmap",
            "budget-state",
            "--from-literal=value=persisted",
        ],
        "create-state",
        maintenance,
    );
    live.cli(&["down"], "down", maintenance);
    live.cli(&["up"], "restart", maintenance);
    for node in nodes["items"].as_array().unwrap() {
        let node_name = node["metadata"]["name"].as_str().unwrap();
        let endpoint = format!("/api/v1/nodes/{node_name}/proxy/healthz");
        let health = live.kubectl(
            &["get", "--raw", &endpoint],
            &format!("resumed-{node_name}-kubelet-health"),
            maintenance,
        );
        assert_eq!(String::from_utf8(health).unwrap().trim(), "ok");
    }
    assert_eq!(
        live.kubectl(
            &[
                "get",
                "configmap",
                "budget-state",
                "-o",
                "jsonpath={.data.value}"
            ],
            "persisted-state",
            maintenance
        ),
        b"persisted"
    );
    prepare_multi_endpoint_scale_service(&live, &workers, probe_image, maintenance);
    live.cli(&["scale", "--workers", "3"], "scale", maintenance);
    live.nodes(4, "scaled-nodes", maintenance);
}
