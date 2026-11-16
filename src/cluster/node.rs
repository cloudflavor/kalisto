use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tracing::info;

use crate::backend::{BackendConfig, ClusterBackend, InstanceConfig};
use crate::cluster::state::NodeState;
use crate::error::{Error, Result};
use crate::k3s::{
    ComponentToggles, KUBELET_RESOLV_CONF_PATH, NodeRole, ServerMode, kubelet_resolv_conf_arg,
    kubelet_resolv_conf_contents,
};
use crate::process::{CommandRunner, Output};
use crate::shell::shell_escape;

#[derive(Debug, Clone)]
pub struct Artifacts {
    pub root: PathBuf,
    pub kubeconfig: PathBuf,
    pub token: PathBuf,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
}

impl Artifacts {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            kubeconfig: root.join("kubeconfig.yaml"),
            token: root.join("node-token"),
            stdout_log: root.join("k3s.stdout.log"),
            stderr_log: root.join("k3s.stderr.log"),
            root,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub role: NodeRole,
    pub artifacts: Artifacts,
}

impl Instance {
    #[must_use]
    pub fn new(name: String, role: NodeRole, root: PathBuf) -> Self {
        Self {
            name,
            role,
            artifacts: Artifacts::new(root),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartedWorker {
    pub node: Instance,
    pub boot_started: Instant,
}

#[derive(Debug, Clone)]
pub struct K3sBootstrapConfig {
    pub k3s_version: String,
    pub server_mode: ServerMode,
    pub component_toggles: ComponentToggles,
}

struct K3sCommandSpec<'a> {
    config: &'a K3sBootstrapConfig,
    artifact_dir: &'a Path,
    node: &'a Instance,
    tls_sans: &'a [String],
    join: Option<JoinConfig<'a>>,
    dynamic_guest_addresses: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct JoinConfig<'a> {
    pub server_url: &'a str,
    pub token: &'a str,
}

#[derive(Debug, Clone)]
pub struct WorkerStartError {
    pub detail: String,
}

impl Display for WorkerStartError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn bootstrap_server(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    config: &K3sBootstrapConfig,
    control_plane: &Instance,
    tls_sans: &[String],
) -> Result<Output> {
    let artifact_dir =
        backend.guest_artifact_root(&control_plane.name, &control_plane.artifacts.root)?;
    ensure_guest_bootstrap_prereqs(backend, runner, control_plane, &artifact_dir)?;
    if k3s_process_running(backend, runner, control_plane, &artifact_dir)? {
        sync_control_plane_runtime_artifacts(backend, runner, control_plane, &artifact_dir)?;
        return Ok(Output {
            stdout: "k3s server already running\n".to_owned(),
            stderr: String::new(),
        });
    }
    let command = build_k3s_command(&K3sCommandSpec {
        config,
        artifact_dir: &artifact_dir,
        node: control_plane,
        tls_sans,
        join: None,
        dynamic_guest_addresses: backend.kind() == crate::backend::BackendKind::AppleContainer,
    })?;
    launch_k3s_process(backend, runner, control_plane, &artifact_dir, &command)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn bootstrap_worker(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    config: &K3sBootstrapConfig,
    worker: &Instance,
    join: JoinConfig<'_>,
) -> Result<Output> {
    let artifact_dir = backend.guest_artifact_root(&worker.name, &worker.artifacts.root)?;
    ensure_guest_bootstrap_prereqs(backend, runner, worker, &artifact_dir)?;
    if k3s_process_running(backend, runner, worker, &artifact_dir)? {
        return Ok(Output {
            stdout: "k3s agent already running\n".to_owned(),
            stderr: String::new(),
        });
    }
    let command = build_k3s_command(&K3sCommandSpec {
        config,
        artifact_dir: &artifact_dir,
        node: worker,
        tls_sans: &[],
        join: Some(join),
        dynamic_guest_addresses: backend.kind() == crate::backend::BackendKind::AppleContainer,
    })?;
    launch_k3s_process(backend, runner, worker, &artifact_dir, &command)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn start_node_instance(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    base_config: &BackendConfig,
    node: &Instance,
    host_api_port: Option<u16>,
    cpus: u8,
    memory_gib: u8,
) -> Result<Output> {
    let output = backend.start_instance(
        runner,
        base_config,
        &InstanceConfig {
            name: node.name.clone(),
            runtime_root: node.artifacts.root.clone(),
            host_port_forward: host_api_port,
            cpus,
            memory_gib,
        },
    )?;
    if let Some(assets) = &base_config.boot_assets_root {
        assets.stage_node(backend, runner, node)?;
    }
    Ok(output)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn start_worker_instances(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    base_config: &BackendConfig,
    workers: &[Instance],
    worker_cpus: u8,
    worker_memory_gib: u8,
    instances: &mut Vec<NodeState>,
) -> std::result::Result<Vec<StartedWorker>, WorkerStartError> {
    let mut started_workers = Vec::with_capacity(workers.len());

    for worker in workers {
        if let Err(error) = fs::create_dir_all(&worker.artifacts.root) {
            return Err(WorkerStartError {
                detail: error.to_string(),
            });
        }

        let boot_started = Instant::now();
        info!(
            node = %worker.name,
            cpus = worker_cpus,
            memory_gib = worker_memory_gib,
            "starting worker VM"
        );
        match start_node_instance(
            backend,
            runner,
            base_config,
            worker,
            None,
            worker_cpus,
            worker_memory_gib,
        ) {
            Ok(_) => {
                instances.push(NodeState {
                    name: worker.name.clone(),
                    role: worker.role,
                    artifacts_root: worker.artifacts.root.clone(),
                });
                info!(node = %worker.name, "worker VM started");
                started_workers.push(StartedWorker {
                    node: worker.clone(),
                    boot_started,
                });
            }
            Err(error) => {
                return Err(WorkerStartError {
                    detail: format!("{}: {error}", worker.name),
                });
            }
        }
    }

    Ok(started_workers)
}

fn ensure_guest_bootstrap_prereqs(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    node: &Instance,
    artifact_dir: &Path,
) -> Result<Output> {
    let artifact_dir = shell_escape(artifact_dir.as_os_str().to_string_lossy().as_ref());
    let kubelet_resolv_conf = shell_escape(KUBELET_RESOLV_CONF_PATH);
    backend.guest_sudo_shell(
        runner,
        &node.name,
        &node.artifacts.root,
        &format!(
            concat!(
                "set -eu\n",
                "mkdir -p {artifact_dir} /etc/kalisto /etc/rancher/k3s /var/lib/rancher/k3s\n",
                "cat <<'EOF' > {kubelet_resolv_conf}\n",
                "{kubelet_resolv_contents}",
                "EOF\n",
                "if [ ! -L /etc/resolv.conf ] && ",
                "cmp -s /etc/resolv.conf {kubelet_resolv_conf} && ",
                "[ -s /run/systemd/resolve/stub-resolv.conf ]; then\n",
                "ln -sfn /run/systemd/resolve/stub-resolv.conf /etc/resolv.conf\n",
                "fi"
            ),
            artifact_dir = artifact_dir,
            kubelet_resolv_conf = kubelet_resolv_conf,
            kubelet_resolv_contents = kubelet_resolv_conf_contents(),
        ),
    )
}

fn k3s_process_running(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    node: &Instance,
    artifact_dir: &Path,
) -> Result<bool> {
    match backend.guest_sudo_shell(
        runner,
        &node.name,
        &node.artifacts.root,
        &running_check_command(artifact_dir, node.role),
    ) {
        Ok(_) => Ok(true),
        Err(Error::CommandFailed { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

fn launch_k3s_process(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    node: &Instance,
    artifact_dir: &Path,
    k3s_command: &str,
) -> Result<Output> {
    backend.guest_sudo_shell(
        runner,
        &node.name,
        &node.artifacts.root,
        &launch_k3s_process_command(artifact_dir, k3s_command),
    )
}

fn launch_k3s_process_command(artifact_dir: &Path, k3s_command: &str) -> String {
    let stdout_log_path = artifact_dir.join("k3s.stdout.log");
    let stderr_log_path = artifact_dir.join("k3s.stderr.log");
    let pid_file_path = pid_file_path(artifact_dir);
    format!(
        "rm -f {pid_file}; nohup /bin/sh -lc {command} >{stdout_log} 2>{stderr_log} </dev/null & printf '%s\\n' $! > {pid_file}",
        pid_file = shell_escape(pid_file_path.as_os_str().to_string_lossy().as_ref()),
        command = shell_escape(k3s_command),
        stdout_log = shell_escape(stdout_log_path.as_os_str().to_string_lossy().as_ref()),
        stderr_log = shell_escape(stderr_log_path.as_os_str().to_string_lossy().as_ref()),
    )
}

fn pid_file_path(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join("k3s.pid")
}

fn running_check_command(artifact_dir: &Path, role: NodeRole) -> String {
    let pid_file = shell_escape(
        pid_file_path(artifact_dir)
            .as_os_str()
            .to_string_lossy()
            .as_ref(),
    );
    let process_match = match role {
        NodeRole::ControlPlane => {
            "$2 == \"k3s-server\" || ($2 == \"k3s\" && $0 ~ /(^|[[:space:]])server([[:space:]]|$)/)"
        }
        NodeRole::Worker => {
            "$2 == \"k3s-agent\" || ($2 == \"k3s\" && $0 ~ /(^|[[:space:]])agent([[:space:]]|$)/)"
        }
    };
    // Do not trust a stale PID or signal a root-owned process as an unprivileged
    // user. Discover the actual role process and repair its runtime PID file.
    format!(
        "pid=$(ps -eo pid=,comm=,args= | awk '{process_match} {{ print $1; exit }}'); test -n \"$pid\" && printf '%s\\n' \"$pid\" > {pid_file}",
    )
}

fn sync_control_plane_runtime_artifacts(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    artifact_dir: &Path,
) -> Result<Output> {
    backend.guest_sudo_shell(
        runner,
        &control_plane.name,
        &control_plane.artifacts.root,
        &control_plane_runtime_artifact_sync_command(artifact_dir),
    )
}

fn control_plane_runtime_artifact_sync_command(artifact_dir: &Path) -> String {
    let kubeconfig_path = shell_escape(
        artifact_dir
            .join("kubeconfig.yaml")
            .as_os_str()
            .to_string_lossy()
            .as_ref(),
    );
    let token_path = shell_escape(
        artifact_dir
            .join("node-token")
            .as_os_str()
            .to_string_lossy()
            .as_ref(),
    );
    format!(
        concat!(
            "cp /etc/rancher/k3s/k3s.yaml {kubeconfig_path}; ",
            "if [ -s /var/lib/rancher/k3s/server/node-token ]; then ",
            "cp /var/lib/rancher/k3s/server/node-token {token_path}; ",
            "fi"
        ),
        kubeconfig_path = kubeconfig_path,
        token_path = token_path,
    )
}

fn build_k3s_command(spec: &K3sCommandSpec<'_>) -> Result<String> {
    let server_mode = match spec.node.role {
        NodeRole::ControlPlane => spec.config.server_mode,
        NodeRole::Worker => ServerMode::Standard,
    };

    let mut args = vec!["/usr/local/bin/k3s".to_owned()];
    match spec.node.role {
        NodeRole::ControlPlane => {
            args.push("server".to_owned());
            if spec.dynamic_guest_addresses {
                // The default CCM interval leaves resumed Apple nodes advertising old IPs.
                args.push(
                    "--kube-cloud-controller-manager-arg=node-status-update-frequency=5s".to_owned(),
                );
            }
            if !spec.node.name.is_empty() {
                args.push("--node-name".to_owned());
                args.push(spec.node.name.clone());
            }
            if server_mode.disable_agent() {
                args.push("--disable-agent".to_owned());
            }
            args.push("--write-kubeconfig".to_owned());
            args.push(
                spec.artifact_dir
                    .join("kubeconfig.yaml")
                    .display()
                    .to_string(),
            );
            args.push("--write-kubeconfig-mode".to_owned());
            args.push("0644".to_owned());
            for san in spec.tls_sans {
                if !san.is_empty() {
                    args.push("--tls-san".to_owned());
                    args.push(san.clone());
                }
            }
            if spec.config.component_toggles.disable_traefik {
                args.push("--disable=traefik".to_owned());
            }
            if spec.config.component_toggles.disable_servicelb {
                args.push("--disable=servicelb".to_owned());
            }
            if spec.config.component_toggles.disable_metrics_server {
                args.push("--disable=metrics-server".to_owned());
            }
            if spec.config.component_toggles.disable_local_storage {
                args.push("--disable=local-storage".to_owned());
            }
            if spec.config.component_toggles.disable_coredns {
                args.push("--disable=coredns".to_owned());
            }
            if spec.config.component_toggles.disable_network_policy {
                args.push("--disable-network-policy".to_owned());
            }
            if spec.config.component_toggles.disable_kube_proxy {
                args.push("--disable-kube-proxy".to_owned());
            }
            if spec.config.component_toggles.flannel_backend_none {
                args.push("--flannel-backend=none".to_owned());
            }
        }
        NodeRole::Worker => {
            let join = spec.join.ok_or_else(|| {
                Error::InvalidArgs("worker bootstrap requires server URL and token".to_owned())
            })?;
            args.push("agent".to_owned());
            if !spec.node.name.is_empty() {
                args.push("--node-name".to_owned());
                args.push(spec.node.name.clone());
            }
            args.push("--server".to_owned());
            args.push(join.server_url.to_owned());
            args.push("--token".to_owned());
            args.push(join.token.to_owned());
        }
    }
    args.push("--prefer-bundled-bin".to_owned());
    args.push("--kubelet-arg".to_owned());
    args.push(kubelet_resolv_conf_arg());
    Ok(args
        .into_iter()
        .map(|arg| shell_escape(&arg))
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::k3s::{ComponentToggles, NodeRole, ServerMode};
    use crate::process::{CommandRunner, Output};

    use super::{
        Artifacts, Instance, K3sBootstrapConfig, K3sCommandSpec, build_k3s_command,
        control_plane_runtime_artifact_sync_command, ensure_guest_bootstrap_prereqs,
        launch_k3s_process_command, start_worker_instances,
    };

    #[derive(Default)]
    struct RecordingRunner(std::sync::Mutex<Vec<String>>);

    impl CommandRunner for RecordingRunner {
        fn run(&self, _program: &str, args: &[String]) -> crate::error::Result<Output> {
            self.0.lock().unwrap().extend_from_slice(args);
            Ok(Output {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn bootstrap_preserves_guest_dns_for_internal_worker_join_names() {
        let runner = RecordingRunner::default();
        let worker = Instance::new(
            "worker".to_owned(),
            NodeRole::Worker,
            PathBuf::from("/runtime/worker"),
        );
        ensure_guest_bootstrap_prereqs(
            &crate::backend::lima::BACKEND,
            &runner,
            &worker,
            &worker.artifacts.root,
        )
        .unwrap();
        let commands = runner.0.lock().unwrap();
        let command = commands.last().unwrap();
        assert!(command.contains("/etc/kalisto/kubelet-resolv.conf"));
        assert!(!command.contains("rm -f /etc/resolv.conf"));
        assert!(command.contains("cmp -s /etc/resolv.conf"));
    }

    #[test]
    fn worker_launch_command_runs_k3s_directly() {
        let config = K3sBootstrapConfig {
            k3s_version: "v1.33.5+k3s1".to_owned(),
            server_mode: ServerMode::Standard,
            component_toggles: ComponentToggles::default(),
        };
        let worker = Instance {
            name: "kalisto-w0-test".to_owned(),
            role: NodeRole::Worker,
            artifacts: Artifacts::new(PathBuf::from("/tmp/kalisto-w0-test")),
        };

        let command = build_k3s_command(&K3sCommandSpec {
            config: &config,
            artifact_dir: Path::new("/tmp/kalisto-w0-test"),
            node: &worker,
            tls_sans: &[],
            dynamic_guest_addresses: false,
            join: Some(super::JoinConfig {
                server_url: "https://cp.example.test:6443",
                token: "secret",
            }),
        })
        .unwrap();

        assert!(command.contains("/usr/local/bin/k3s"));
        assert!(command.contains("https://cp.example.test:6443"));
        assert!(command.contains("secret"));
        assert!(command.contains("--kubelet-arg"));
        assert!(command.contains("resolv-conf=/etc/kalisto/kubelet-resolv.conf"));
    }

    #[test]
    fn control_plane_k3s_command_is_schedulable_by_default() {
        let config = K3sBootstrapConfig {
            k3s_version: "v1.33.5+k3s1".to_owned(),
            server_mode: ServerMode::Standard,
            component_toggles: ComponentToggles::default(),
        };
        let control_plane = Instance {
            name: "kalisto-cp-test".to_owned(),
            role: NodeRole::ControlPlane,
            artifacts: Artifacts::new(PathBuf::from("/tmp/kalisto-cp-test")),
        };

        let command = build_k3s_command(&K3sCommandSpec {
            config: &config,
            artifact_dir: Path::new("/tmp/kalisto-cp-test"),
            node: &control_plane,
            tls_sans: &[],
            join: None,
            dynamic_guest_addresses: false,
        })
        .unwrap();

        assert!(!command.contains("--node-taint"));
        assert!(command.contains("--kubelet-arg"));
        assert!(command.contains("resolv-conf=/etc/kalisto/kubelet-resolv.conf"));
    }

    #[test]
    fn start_worker_instances_succeeds_for_empty_input() {
        let mut started = Vec::new();
        let output = start_worker_instances(
            &crate::backend::lima::BACKEND,
            &crate::process::SystemRunner,
            &crate::backend::BackendConfig {
                template: "template://default".to_owned(),
                workspace_root: PathBuf::from("."),
                enable_rosetta: true,
                network: "lima:user-v2".to_owned(),
                firecracker_kernel: None,
                firecracker_rootfs: None,
                boot_assets_root: None,
                stream_vm_logs: false,
            },
            &[],
            2,
            4,
            &mut started,
        )
        .unwrap();

        assert!(output.is_empty());
        assert!(started.is_empty());
    }

    #[test]
    fn control_plane_bootstrap_command_mirrors_node_token_into_artifacts() {
        let command = launch_k3s_process_command(
            Path::new("/tmp/kalisto-cp-test"),
            "/usr/local/bin/k3s server --write-kubeconfig /tmp/kalisto-cp-test/kubeconfig.yaml",
        );

        assert!(command.contains("nohup /bin/sh -lc"));
        assert!(command.contains("/tmp/kalisto-cp-test/k3s.stdout.log"));
        assert!(command.contains("/tmp/kalisto-cp-test/k3s.stderr.log"));
        assert!(command.contains("/tmp/kalisto-cp-test/k3s.pid"));
        assert!(command.contains("write-kubeconfig"));
        assert!(!command.contains("&;"));
    }

    #[test]
    fn sync_control_plane_runtime_artifacts_command_targets_runtime_files() {
        let command =
            control_plane_runtime_artifact_sync_command(Path::new("/tmp/kalisto-cp-test"));

        assert!(command.contains("/etc/rancher/k3s/k3s.yaml"));
        assert!(command.contains("/tmp/kalisto-cp-test/kubeconfig.yaml"));
        assert!(command.contains("/var/lib/rancher/k3s/server/node-token"));
        assert!(command.contains("/tmp/kalisto-cp-test/node-token"));
    }
}
