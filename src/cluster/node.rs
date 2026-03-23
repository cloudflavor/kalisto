use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tracing::info;

use crate::backend::{BackendConfig, ClusterBackend, InstanceConfig};
use crate::cluster::state::NodeState;
use crate::error::{Error, Result};
use crate::k3s::{ComponentToggles, NodeRole, ServerMode};
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

struct BootstrapCommandSpec<'a> {
    config: &'a K3sBootstrapConfig,
    artifact_dir: &'a Path,
    node: &'a Instance,
    tls_sans: &'a [String],
    join: Option<JoinConfig<'a>>,
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

pub fn bootstrap_server(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    config: &K3sBootstrapConfig,
    control_plane: &Instance,
    tls_sans: &[String],
) -> Result<Output> {
    let artifact_dir =
        backend.guest_artifact_root(&control_plane.name, &control_plane.artifacts.root)?;
    let command = build_bootstrap_command(BootstrapCommandSpec {
        config,
        artifact_dir: &artifact_dir,
        node: control_plane,
        tls_sans,
        join: None,
    })?;
    backend.guest_sudo_shell(
        runner,
        &control_plane.name,
        &control_plane.artifacts.root,
        &command,
    )
}

pub fn bootstrap_worker(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    config: &K3sBootstrapConfig,
    worker: &Instance,
    join: JoinConfig<'_>,
) -> Result<Output> {
    let artifact_dir = backend.guest_artifact_root(&worker.name, &worker.artifacts.root)?;
    let command = build_bootstrap_command(BootstrapCommandSpec {
        config,
        artifact_dir: &artifact_dir,
        node: worker,
        tls_sans: &[],
        join: Some(join),
    })?;
    backend.guest_sudo_shell(runner, &worker.name, &worker.artifacts.root, &command)
}

pub fn start_node_instance(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    base_config: &BackendConfig,
    node: &Instance,
    host_api_port: Option<u16>,
    cpus: u8,
    memory_gib: u8,
) -> Result<Output> {
    backend.start_instance(
        runner,
        base_config,
        &InstanceConfig {
            name: node.name.clone(),
            runtime_root: node.artifacts.root.clone(),
            host_port_forward: host_api_port,
            cpus,
            memory_gib,
        },
    )
}

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

fn build_bootstrap_command(spec: BootstrapCommandSpec<'_>) -> Result<String> {
    build_direct_bootstrap_command(spec)
}

fn build_direct_bootstrap_command(spec: BootstrapCommandSpec<'_>) -> Result<String> {
    let artifact_dir = shell_escape(spec.artifact_dir.as_os_str().to_string_lossy().as_ref());
    let stdout_log_path = spec.artifact_dir.join("k3s.stdout.log");
    let stderr_log_path = spec.artifact_dir.join("k3s.stderr.log");
    let stdout_log = shell_escape(stdout_log_path.as_os_str().to_string_lossy().as_ref());
    let stderr_log = shell_escape(stderr_log_path.as_os_str().to_string_lossy().as_ref());
    let k3s_command = build_k3s_command(&spec)?;
    let running_check = running_check(spec.node.role);

    Ok(format!(
        concat!(
            "mkdir -p {artifact_dir} /etc/rancher/k3s /var/lib/rancher/k3s; ",
            "test -f /etc/kalisto/node-image.env; ",
            "test -x /usr/local/bin/k3s; ",
            "if {running_check}; then exit 0; fi; ",
            "nohup /bin/sh -lc {k3s_command} >{stdout_log} 2>{stderr_log} </dev/null &"
        ),
        artifact_dir = artifact_dir,
        running_check = running_check,
        k3s_command = shell_escape(&k3s_command),
        stdout_log = stdout_log,
        stderr_log = stderr_log,
    ))
}

fn build_k3s_command(spec: &BootstrapCommandSpec<'_>) -> Result<String> {
    let server_mode = match spec.node.role {
        NodeRole::ControlPlane => spec.config.server_mode,
        NodeRole::Worker => ServerMode::Standard,
    };

    let mut args = vec!["/usr/local/bin/k3s".to_owned()];
    match spec.node.role {
        NodeRole::ControlPlane => {
            args.push("server".to_owned());
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
    Ok(args
        .into_iter()
        .map(|arg| shell_escape(&arg))
        .collect::<Vec<_>>()
        .join(" "))
}

fn running_check(role: NodeRole) -> &'static str {
    match role {
        NodeRole::ControlPlane => {
            "ps -eo comm=,args= | awk '$1 == \"k3s\" && $0 ~ /(^|[[:space:]])server([[:space:]]|$)/ { found = 1 } END { exit found ? 0 : 1 }'"
        }
        NodeRole::Worker => {
            "ps -eo comm=,args= | awk '$1 == \"k3s\" && $0 ~ /(^|[[:space:]])agent([[:space:]]|$)/ { found = 1 } END { exit found ? 0 : 1 }'"
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::k3s::{ComponentToggles, NodeRole, ServerMode};

    use super::{
        Artifacts, BootstrapCommandSpec, Instance, K3sBootstrapConfig, build_bootstrap_command,
        build_k3s_command, start_worker_instances,
    };

    #[test]
    fn worker_bootstrap_command_runs_k3s_directly() {
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

        let command = build_bootstrap_command(BootstrapCommandSpec {
            config: &config,
            artifact_dir: Path::new("/tmp/kalisto-w0-test"),
            node: &worker,
            tls_sans: &[],
            join: Some(super::JoinConfig {
                server_url: "https://cp.example.test:6443",
                token: "secret",
            }),
        })
        .unwrap();

        assert!(command.contains("nohup /bin/sh -lc"));
        assert!(command.contains("/usr/local/bin/k3s"));
        assert!(command.contains("https://cp.example.test:6443"));
        assert!(command.contains("secret"));
        assert!(command.contains("mkdir -p '/tmp/kalisto-w0-test'"));
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

        let command = build_k3s_command(&BootstrapCommandSpec {
            config: &config,
            artifact_dir: Path::new("/tmp/kalisto-cp-test"),
            node: &control_plane,
            tls_sans: &[],
            join: None,
        })
        .unwrap();

        assert!(!command.contains("--node-taint"));
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
}
