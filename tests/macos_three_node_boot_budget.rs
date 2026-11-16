#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kalisto::backend::BackendKind;
use kalisto::cluster::manager;
use kalisto::cluster::spec::ClusterSpec;

struct OwnedCluster(String, PathBuf);

impl Drop for OwnedCluster {
    fn drop(&mut self) {
        // Preserve pull/scheduling/container timings before deleting owned VMs,
        // including when the budget or readiness assertion fails.
        if let Ok(kubeconfig) = manager::kubeconfig_path(&self.0) {
            for (file, args) in [
                ("pods.json", vec!["get", "pods", "-A", "-o", "json"]),
                ("events.json", vec!["get", "events", "-A", "-o", "json"]),
            ] {
                if let Ok(output) = Command::new(managed_kubectl())
                    .arg("--kubeconfig")
                    .arg(&kubeconfig)
                    .arg("--request-timeout=5s")
                    .args(args)
                    .output()
                {
                    let _ = fs::write(self.1.join(file), output.stdout);
                    let _ = fs::write(self.1.join(format!("{file}.stderr")), output.stderr);
                }
            }
        }
        if let Ok(data) = kalisto::paths::data_root() {
            let runtime = data.join("clusters").join(&self.0).join("runtime");
            for suffix in ["cp", "w0", "w1"] {
                let directory = if suffix == "cp" {
                    runtime.join("control-plane")
                } else {
                    runtime.join(format!("kalisto-{}-{suffix}", self.0))
                };
                if let Ok(log) = fs::read_to_string(directory.join("k3s.stderr.log")) {
                    // Keep import timings, never full logs that might expose credentials.
                    let timing = log
                        .lines()
                        .filter(|line| {
                            [
                                "Importing",
                                "Imported",
                                "Unpacking",
                                "Starting k3s",
                                "Running containerd",
                            ]
                            .iter()
                            .any(|stage| line.contains(stage))
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let _ = fs::write(self.1.join(format!("{suffix}-image-timings.log")), timing);
                }
            }
        }
        let lima_home = std::env::var_os("LIMA_HOME").map_or_else(
            || PathBuf::from(std::env::var_os("HOME").expect("HOME is set")).join(".lima"),
            PathBuf::from,
        );
        for suffix in ["cp", "w0", "w1"] {
            let name = format!("kalisto-{}-{suffix}", self.0);
            for log in [
                "ha.stderr.log",
                "ha.stdout.log",
                "serial.log",
                "serialv.log",
            ] {
                let source = lima_home.join(&name).join(log);
                if source.is_file() {
                    let _ = fs::copy(source, self.1.join(format!("{suffix}-{log}")));
                }
            }
        }
        if let Err(error) = manager::destroy(&self.0) {
            eprintln!("failed to clean up benchmark cluster {}: {error}", self.0);
        }
        if let Err(error) = kalisto::paths::clear_default_cluster_name_if_matches(&self.0) {
            eprintln!("failed to clear benchmark default selection: {error}");
        }
    }
}

fn kubectl(kubeconfig: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(managed_kubectl())
        .arg("--kubeconfig")
        .arg(kubeconfig)
        .arg("--request-timeout=5s")
        .args(args)
        .output()
        .expect("host kubectl should run");
    assert!(
        output.status.success(),
        "host kubectl {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn managed_kubectl() -> PathBuf {
    kalisto::kubectl::binary(
        kalisto::cluster::spec::DEFAULT_K3S_VERSION,
        &kalisto::process::SystemRunner,
    )
    .expect("matching managed kubectl should be prepared")
}

struct BorrowedAppleRuntime {
    executable: PathBuf,
    identity: serde_json::Value,
    config: Option<Vec<u8>>,
    private_files: [Option<(u64, SystemTime)>; 3],
}

impl BorrowedAppleRuntime {
    fn capture(evidence: &Path, stage: &str) -> Self {
        let executable = std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
            .map(|directory| directory.join("container"))
            .find(|candidate| candidate.is_file())
            .expect("native CI requires an installed Homebrew container on PATH")
            .canonicalize()
            .unwrap();
        assert!(
            executable.to_string_lossy().contains("/Cellar/container/"),
            "native CI must prove reuse of Homebrew, not a private runtime"
        );
        let version = runtime_output(&executable, &["--version"]);
        assert!(
            String::from_utf8_lossy(&version)
                .starts_with("container CLI version 1.7.1 (build: release,"),
            "native CI requires the supported Homebrew release"
        );
        let status: serde_json::Value = serde_json::from_slice(&runtime_output(
            &executable,
            &["system", "status", "--format", "json"],
        ))
        .unwrap();
        assert_eq!(status["status"], "running", "do not start a user's service");
        assert_eq!(status["server"]["version"], "1.7.1");
        assert_eq!(status["server"]["build"], "release");
        let app = PathBuf::from(status["paths"]["appRoot"].as_str().unwrap());
        let install = PathBuf::from(status["paths"]["installRoot"].as_str().unwrap());
        assert!(executable.starts_with(install.canonicalize().unwrap()));
        let services = launchd_services(&app, &install);
        let properties: serde_json::Value = serde_json::from_slice(&runtime_output(
            &executable,
            &["system", "property", "list", "--format", "json"],
        ))
        .unwrap();
        // Homebrew's PATH entry is a shell wrapper; Kalisto selects the native
        // CLI beside the already-running API daemon, retaining this installation.
        let path_executable = executable;
        let executable = Path::new(
            services["com.apple.container.apiserver"]["program"]
                .as_str()
                .unwrap(),
        )
        .parent()
        .unwrap()
        .join("container")
        .canonicalize()
        .unwrap();
        assert!(executable.starts_with(install.canonicalize().unwrap()));
        let identity = serde_json::json!({
            "executable": executable,
            "path_executable": path_executable,
            "paths": status["paths"],
            "server": status["server"],
            "services": services,
            "properties": properties
        });
        fs::write(
            evidence.join(format!("runtime-{stage}.json")),
            serde_json::to_vec_pretty(&identity).unwrap(),
        )
        .unwrap();
        let private_root = kalisto::paths::artifacts_root()
            .unwrap()
            .join("runtime-v1/apple-container-1.7.1-arm64");
        let private_files = [
            private_root.join("install"),
            private_root.join("downloads/container-1.7.1-installer-signed.pkg"),
            private_root.join("downloads/container-1.7.1-installer-signed.download"),
        ]
        .map(|path| match fs::metadata(&path) {
            Ok(metadata) => Some((metadata.len(), metadata.modified().unwrap())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("cannot inspect private runtime {}: {error}", path.display()),
        });
        let config = match fs::read(app.join("config.toml")) {
            Ok(config) => Some(config),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("cannot inspect borrowed service config: {error}"),
        };
        Self {
            executable,
            identity,
            config,
            private_files,
        }
    }

    fn assert_unchanged(&self, evidence: &Path, stage: &str) {
        let after = Self::capture(evidence, stage);
        assert_eq!(
            after.identity, self.identity,
            "borrowed service was mutated"
        );
        assert!(
            after.config == self.config,
            "borrowed service configuration must not change"
        );
        assert_eq!(
            after.private_files, self.private_files,
            "reuse must not download or install a private Apple runtime"
        );
        assert_eq!(
            kalisto::runtime::program("container").unwrap(),
            self.executable,
            "Kalisto must select the compatible executable from PATH"
        );
        let selection: serde_json::Value = serde_json::from_slice(
            &fs::read(
                kalisto::paths::data_root()
                    .unwrap()
                    .join("apple-container/selection.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(selection["managed"], false, "the service is user-owned");
        assert_eq!(selection["executable"], self.identity["executable"]);
        for key in ["app_root", "install_root"] {
            let status_key = if key == "app_root" {
                "appRoot"
            } else {
                "installRoot"
            };
            assert_eq!(
                Path::new(selection[key].as_str().unwrap()),
                Path::new(self.identity["paths"][status_key].as_str().unwrap())
            );
        }
        assert_eq!(
            selection["api_program"],
            self.identity["services"]["com.apple.container.apiserver"]["program"]
        );
        let log = fs::read_to_string(evidence.join("pull.log")).unwrap();
        assert!(
            !log.contains("container-1.7.1-installer-signed")
                && !log.contains("signed Apple container runtime"),
            "pull must not fetch a second Apple runtime"
        );
    }
}

fn launchd_services(app: &Path, install: &Path) -> serde_json::Map<String, serde_json::Value> {
    let uid = runtime_output(Path::new("/usr/bin/id"), &["-u"]);
    let session = runtime_output(Path::new("/bin/launchctl"), &["managername"]);
    let domain = match String::from_utf8_lossy(&session).trim() {
        "Aqua" => "gui",
        "Background" => "user",
        other => panic!("unsupported native CI launchd session: {other}"),
    };
    let mut services = serde_json::Map::new();
    for label in [
        "com.apple.container.apiserver",
        "com.apple.container.container-core-images",
    ] {
        let target = format!("{domain}/{}/{label}", String::from_utf8_lossy(&uid).trim());
        let output = runtime_output(Path::new("/bin/launchctl"), &["print", &target]);
        let output = String::from_utf8(output).unwrap();
        let mut identity = serde_json::Map::new();
        for (key, separator) in [
            ("program", "="),
            ("CONTAINER_APP_ROOT", "=>"),
            ("CONTAINER_INSTALL_ROOT", "=>"),
            ("TMPDIR", "=>"),
        ] {
            if let Some(value) = launchd_value(&output, key, separator) {
                identity.insert(key.into(), value.into());
            }
        }
        assert_eq!(
            identity["CONTAINER_APP_ROOT"].as_str().map(Path::new),
            Some(app),
            "the running service must keep its original app root"
        );
        assert_eq!(
            identity["CONTAINER_INSTALL_ROOT"].as_str().map(Path::new),
            Some(install),
            "the running service must keep its original install root"
        );
        if label == "com.apple.container.apiserver" {
            for key in ["pid", "runs"] {
                let value = launchd_value(&output, key, "=")
                    .expect("the already-running API daemon must retain its process");
                identity.insert(key.into(), value.into());
            }
        }
        services.insert(label.into(), identity.into());
    }
    services
}

fn runtime_output(program: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(program)
        .args(args)
        .output()
        .expect("runtime identity inspection should run");
    assert!(
        output.status.success(),
        "{} {args:?} failed: {}",
        program.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn launchd_value<'a>(output: &'a str, key: &str, separator: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        let (name, value) = line.trim().split_once(separator)?;
        (name.trim() == key).then_some(value.trim())
    })
}

#[test]
fn cold_cli_boot_has_three_ready_nodes_working_cni_and_host_api_under_one_minute() {
    if std::env::var("KALISTO_LIVE_THREE_NODE_BOOT_BUDGET").as_deref() != Ok("1") {
        eprintln!("skipping opt-in Lima cold boot budget benchmark");
        return;
    }
    cold_cli_boot(BackendKind::Lima);
}

#[test]
fn apple_container_k3s_has_functional_three_node_cilium_under_one_minute() {
    if std::env::var("KALISTO_LIVE_APPLE_CONTAINER_BOOT_BUDGET").as_deref() != Ok("1") {
        eprintln!("skipping opt-in Apple container cold boot budget benchmark");
        return;
    }
    cold_cli_boot(BackendKind::AppleContainer);
}

#[expect(
    clippy::too_many_lines,
    reason = "Cold startup and network proofs share one clock."
)]
fn cold_cli_boot(backend: BackendKind) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("budget-{}-{nonce}", std::process::id());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let evidence = root.join("target/kalisto/boot-budget").join(&name);
    let workspace = evidence.join("empty-cwd");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let mut spec = ClusterSpec::new(name.clone(), workspace.clone());
    spec.backend = backend;
    assert!(
        !spec.state_root().exists(),
        "cold benchmark must not reuse any persisted cluster state"
    );
    let prefix = format!("kalisto-{name}-");
    let lima_home = std::env::var_os("LIMA_HOME").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").expect("HOME is set")).join(".lima"),
        PathBuf::from,
    );
    if backend == BackendKind::Lima {
        for suffix in ["cp", "w0", "w1"] {
            assert!(
                !lima_home.join(format!("{prefix}{suffix}")).exists(),
                "cold benchmark must not reuse any provisioned VM"
            );
        }
    }
    let cleanup = OwnedCluster(name.clone(), evidence.clone());
    let borrowed_runtime = (backend == BackendKind::AppleContainer)
        .then(|| BorrowedAppleRuntime::capture(&evidence, "before-pull"));
    let up_log = fs::File::create(evidence.join("up.log")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_kalisto"));
    command
        .env("RUST_LOG", "info")
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .current_dir(&workspace)
        .args([
            "up",
            "--workers",
            "2",
            "--addons",
            "cni/cilium",
            "--name",
            &name,
        ])
        .arg("--project-root")
        .arg(&workspace)
        .stdout(Stdio::from(up_log.try_clone().unwrap()))
        .stderr(Stdio::from(up_log));
    if backend != BackendKind::AppleContainer {
        command.args(["--backend", backend.as_str()]);
    }
    if let Ok(port) = std::env::var("KALISTO_LIMA_K3S_API_PORT") {
        command.args(["--api-port", &port]);
    }

    // Preparation may build a sanitized OS-only image, never a Kubernetes cluster.
    // Both commands run outside the checkout with a system-only PATH.
    let mut pull = Command::new(command.get_program());
    let pull_log = fs::File::create(evidence.join("pull.log")).unwrap();
    pull.env("RUST_LOG", "info,kalisto::apple_container::registry=debug");
    pull.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    let pull_status = pull
        .current_dir(&workspace)
        .arg("pull")
        .args(command.get_args().skip(1))
        .stdout(Stdio::from(pull_log.try_clone().unwrap()))
        .stderr(Stdio::from(pull_log))
        .status()
        .expect("asset preparation should run");
    assert!(
        pull_status.success(),
        "asset preparation failed; see pull.log"
    );
    assert!(
        !spec.state_root().exists(),
        "pull must not create cluster state"
    );
    if let Some(runtime) = &borrowed_runtime {
        runtime.assert_unchanged(&evidence, "after-pull");
    }
    if backend == BackendKind::Lima {
        let after_pull =
            Command::new(kalisto::runtime::program("limactl").expect("pull installs Lima"))
                .args(["list", "--format", "{{ .Name }}"])
                .output()
                .expect("Lima should list instances after pull");
        assert!(after_pull.status.success());
        assert!(
            String::from_utf8_lossy(&after_pull.stdout)
                .lines()
                .all(|instance| !instance.starts_with(&prefix)),
            "pull must not pre-create benchmark VMs"
        );
    } else {
        use kalisto::process::CommandRunner;
        let after_pull = kalisto::process::SystemRunner
            .run(
                "container",
                &[
                    "list".into(),
                    "--all".into(),
                    "--format".into(),
                    "json".into(),
                ],
            )
            .expect("Apple runtime should list containers after pull");
        assert!(
            !after_pull.stdout.contains(&prefix),
            "pull must not pre-create benchmark nodes"
        );
    }
    managed_kubectl();
    let mut prepared_spec = spec.clone();
    prepared_spec.backend = backend;
    prepared_spec.cni_plugin = kalisto::cluster::addon::CniPlugin::Cilium;
    let prepared = kalisto::pull::load(&prepared_spec).unwrap();

    // Start before the real CLI process: image resolution/downloads performed
    // by up, VM creation, guest boot and k3s/CNI startup are all on the clock.
    // No manually bootstrapped guests or pre-created VM definitions qualify.
    let observation_seconds = std::env::var("KALISTO_COLD_OBSERVATION_SECONDS")
        .map_or(60, |value| {
            value
                .parse::<u64>()
                .expect("observation timeout should be seconds")
        })
        .max(60);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(observation_seconds);
    let mut child = command.spawn().expect("product CLI should start");
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "cold CLI startup failed; see up.log");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cold CLI startup did not finish in {observation_seconds}s; see up.log");
        }
        thread::sleep(Duration::from_millis(50));
    }

    let kubeconfig = manager::kubeconfig_path(&name).unwrap();
    let readyz = kubectl(&kubeconfig, &["get", "--raw=/readyz"]);
    assert_eq!(String::from_utf8_lossy(&readyz).trim(), "ok");
    let nodes: serde_json::Value =
        serde_json::from_slice(&kubectl(&kubeconfig, &["get", "nodes", "-o", "json"])).unwrap();
    let nodes = nodes["items"].as_array().unwrap();
    assert_eq!(nodes.len(), 3);
    assert!(nodes.iter().all(|node| {
        node["status"]["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|condition| condition["type"] == "Ready" && condition["status"] == "True")
    }));

    for (namespace, resource) in prepared.rollouts() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "boot budget exhausted before Cilium readiness"
        );
        let timeout = format!("--timeout={}s", remaining.as_secs().max(1));
        kubectl(
            &kubeconfig,
            &["-n", namespace, "rollout", "status", resource, &timeout],
        );
    }

    // DNS requests from both workers must reach CoreDNS through the pod/service
    // network. Ready nodes or an applied CNI manifest alone are not enough.
    let control_plane = spec.control_plane_name();
    let workers = nodes
        .iter()
        .filter_map(|node| node["metadata"]["name"].as_str())
        .filter(|node| *node != control_plane.as_str())
        .collect::<Vec<_>>();
    assert_eq!(workers.len(), 2);
    let mut pods = workers
        .iter()
        .enumerate()
        .map(|(index, worker)| {
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": format!("boot-budget-dns-{index}"),
                    "labels": {"app": "kalisto-boot-budget"}
                },
                "spec": {
                    "nodeName": worker,
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "dns",
                        "image": prepared.probe_image(),
                        "imagePullPolicy": "Never",
                        "command": ["sh", "-c", "for i in $(seq 1 20); do if nslookup kubernetes.default.svc.cluster.local && wget -q -T 1 -O - http://boot-budget-http.default.svc.cluster.local:8080/ | grep -qx ready; then exit 0; fi; sleep 1; done; exit 1"]
                    }]
                }
            })
        })
        .collect::<Vec<_>>();
    pods.push(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "boot-budget-http", "labels": {"app": "boot-budget-http"}},
        "spec": {
            "nodeName": control_plane,
            "containers": [{
                "name": "http", "image": prepared.probe_image(), "imagePullPolicy": "Never",
                "command": ["sh", "-c", "mkdir -p /www; printf 'ready\\n' >/www/index.html; exec httpd -f -p 8080 -h /www"],
                "readinessProbe": {"httpGet": {"path": "/", "port": 8080}, "periodSeconds": 1}
            }]
        }
    }));
    pods.push(serde_json::json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"name": "boot-budget-http"},
        "spec": {"selector": {"app": "boot-budget-http"}, "ports": [{"port": 8080, "targetPort": 8080}]}
    }));
    let manifest = evidence.join("cni-probes.json");
    fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({
            "apiVersion": "v1", "kind": "List", "items": pods
        }))
        .unwrap(),
    )
    .unwrap();
    kubectl(&kubeconfig, &["create", "-f", manifest.to_str().unwrap()]);
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
        !remaining.is_zero(),
        "cold boot budget exhausted before CNI probes"
    );
    let timeout = format!("--timeout={}s", remaining.as_secs().max(1));
    kubectl(
        &kubeconfig,
        &[
            "wait",
            "--for=jsonpath={.status.phase}=Succeeded",
            "pod",
            "-l",
            "app=kalisto-boot-budget",
            &timeout,
        ],
    );
    fs::write(
        evidence.join("cni-probes.log"),
        kubectl(
            &kubeconfig,
            &["logs", "-l", "app=kalisto-boot-budget", "--prefix=true"],
        ),
    )
    .unwrap();
    let elapsed = started.elapsed();
    fs::write(
        evidence.join("result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "seconds": elapsed.as_secs_f64(),
            "fresh_vm_definitions": true,
            "backend": backend.as_str(),
            "ready_nodes": 3,
            "cni": "cilium",
            "cni_rollouts_ready": true,
            "authenticated_host_api": true,
            "worker_dns_probes": 2,
            "worker_cross_node_http_probes": 2,
            "probe_images_prepared": true,
            "empty_working_directory": true,
            "system_only_path": true,
            "borrowed_homebrew_runtime": borrowed_runtime.is_some(),
            "under_60_seconds": elapsed < Duration::from_secs(60)
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(
        elapsed < Duration::from_secs(60),
        "cold CLI boot, API and functional CNI exceeded 60 seconds: {elapsed:?}"
    );
    if backend == BackendKind::AppleContainer {
        let pull_output = fs::read_to_string(evidence.join("pull.log")).unwrap();
        let up_output = fs::read_to_string(evidence.join("up.log")).unwrap();
        assert!(
            pull_output
                .lines()
                .any(|line| line == "backend: apple-container")
        );
        assert!(
            !up_output.contains("template="),
            "Apple startup must not advertise an unused Lima template"
        );
        fs::write(
            evidence.join("backend-isolation.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "backend": "apple-container",
                "pull_reports_selected_backend": true,
                "startup_logs_lima_template": false
            }))
            .unwrap(),
        )
        .unwrap();
        prove_persistent_restart_and_worker_scale(&name, &prepared_spec, &kubeconfig);
        borrowed_runtime
            .as_ref()
            .unwrap()
            .assert_unchanged(&evidence, "after-restart-scale");
        drop(cleanup);
        borrowed_runtime
            .as_ref()
            .unwrap()
            .assert_unchanged(&evidence, "after-destroy");
    }
}

fn prove_persistent_restart_and_worker_scale(name: &str, spec: &ClusterSpec, kubeconfig: &Path) {
    kubectl(
        kubeconfig,
        &[
            "create",
            "configmap",
            "boot-budget-state",
            "--from-literal=value=persisted",
        ],
    );
    manager::down(name, false).expect("Apple nodes should stop without deleting disks");
    let stopped = manager::status(name).expect("stopped cluster should remain defined");
    assert!(
        stopped.nodes.iter().all(|node| node
            .backend_status
            .split_whitespace()
            .any(|word| word == "Stopped")),
        "down must stop every Apple node"
    );
    manager::up(spec).expect("Apple cluster should resume its persisted node disks");
    assert_eq!(
        kubectl(
            kubeconfig,
            &[
                "get",
                "configmap",
                "boot-budget-state",
                "-o",
                "jsonpath={.data.value}"
            ]
        ),
        b"persisted",
        "resume must retain Kubernetes datastore contents"
    );
    manager::scale(name, 3).expect("Apple cluster should add a worker on its existing network");
    let nodes: serde_json::Value =
        serde_json::from_slice(&kubectl(kubeconfig, &["get", "nodes", "-o", "json"])).unwrap();
    let nodes = nodes["items"].as_array().unwrap();
    assert_eq!(nodes.len(), 4);
    assert!(
        nodes.iter().all(|node| {
            node["status"]["conditions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|condition| condition["type"] == "Ready" && condition["status"] == "True")
        }),
        "the added worker must join with a functional CNI"
    );
}
