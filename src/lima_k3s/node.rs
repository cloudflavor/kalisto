use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::error::Result;
use crate::k3s::{BootstrapSpec, NodeRole, ServerMode};
use crate::probe::macos_lima::{
    ProbeConfig as LimaBaseConfig, guest_shell, guest_sudo_shell, shell_escape, start_instance,
};
use crate::process::{CommandRunner, Output};

use super::config::Config;
use super::report::Result as ProbeResult;
use super::util::{first_line, truncate};

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

pub fn build_worker_names(worker_count: usize, cluster_suffix: &str) -> Vec<String> {
    (0..worker_count)
        .map(|index| format!("kalisto-w{index}-{cluster_suffix}"))
        .collect()
}

pub fn build_worker_instances(
    worker_count: usize,
    cluster_suffix: &str,
    artifact_root: &Path,
) -> Vec<Instance> {
    build_worker_names(worker_count, cluster_suffix)
        .into_iter()
        .map(|worker_name| {
            Instance::new(
                worker_name.clone(),
                NodeRole::Worker,
                artifact_root.join(worker_name),
            )
        })
        .collect()
}

pub fn start_node_instance(
    runner: &dyn CommandRunner,
    base_config: &LimaBaseConfig,
    node: &Instance,
    workspace_root: &Path,
    host_api_port: Option<u16>,
    cpus: u8,
    memory_gib: u8,
) -> Result<Output> {
    start_instance(
        runner,
        base_config,
        &node.name,
        workspace_root,
        host_api_port,
        cpus,
        memory_gib,
    )
}

pub fn bootstrap_server(
    runner: &dyn CommandRunner,
    config: &Config,
    workspace_root: &Path,
    helper_script: &Path,
    control_plane: &Instance,
) -> Result<Output> {
    let command = build_bootstrap_command(
        config,
        workspace_root,
        helper_script,
        control_plane,
        None,
        None,
    );
    guest_sudo_shell(runner, &control_plane.name, &command)
}

pub fn bootstrap_worker(
    runner: &dyn CommandRunner,
    config: &Config,
    workspace_root: &Path,
    helper_script: &Path,
    worker: &Instance,
    server_url: &str,
    token: &str,
) -> Result<Output> {
    let command = build_bootstrap_command(
        config,
        workspace_root,
        helper_script,
        worker,
        Some(server_url),
        Some(token),
    );
    guest_sudo_shell(runner, &worker.name, &command)
}

pub fn start_worker_instances(
    runner: &dyn CommandRunner,
    base_config: &LimaBaseConfig,
    workspace_root: &Path,
    workers: &[Instance],
    instances: &mut Vec<String>,
    results: &mut Vec<ProbeResult>,
) -> std::result::Result<Vec<StartedWorker>, super::report::Report> {
    let mut started_workers = Vec::with_capacity(workers.len());

    for worker in workers {
        if let Err(error) = fs::create_dir_all(&worker.artifacts.root) {
            results.push(ProbeResult::fail("artifact.worker", error.to_string()));
            return Err(super::report::Report {
                results: results.clone(),
            });
        }

        let boot_started = Instant::now();
        match start_node_instance(runner, base_config, worker, workspace_root, None, 2, 4) {
            Ok(output) => {
                results.push(ProbeResult::pass(
                    "worker.instance.start",
                    format!(
                        "{} ({})",
                        worker.name,
                        first_line(&output.stdout, "lima worker instance started")
                    ),
                ));
                instances.push(worker.name.clone());
                started_workers.push(StartedWorker {
                    node: worker.clone(),
                    boot_started,
                });
            }
            Err(error) => {
                results.push(ProbeResult::fail(
                    "worker.instance.start",
                    format!("{}: {error}", worker.name),
                ));
                return Err(super::report::Report {
                    results: results.clone(),
                });
            }
        }
    }

    Ok(started_workers)
}

pub fn collect_logs(runner: &dyn CommandRunner, results: &mut Vec<ProbeResult>, node: &Instance) {
    let command = format!(
        "tail -n 80 {} {} 2>/dev/null || true",
        shell_escape(
            node.artifacts
                .stderr_log
                .as_os_str()
                .to_string_lossy()
                .as_ref()
        ),
        shell_escape(
            node.artifacts
                .stdout_log
                .as_os_str()
                .to_string_lossy()
                .as_ref()
        ),
    );
    if let Ok(output) = guest_shell(runner, &node.name, &command) {
        results.push(ProbeResult::pass(
            "k3s.logs",
            format!("{}:\n{}", node.name, truncate(&output.stdout, 1200)),
        ));
    }
}

fn build_bootstrap_command(
    config: &Config,
    workspace_root: &Path,
    helper_script: &Path,
    node: &Instance,
    server_url: Option<&str>,
    token: Option<&str>,
) -> String {
    let server_mode = match node.role {
        NodeRole::ControlPlane => config.server_mode,
        NodeRole::Worker => ServerMode::Standard,
    };
    let spec = BootstrapSpec {
        workspace_root: workspace_root.to_path_buf(),
        artifact_dir: node.artifacts.root.clone(),
        node_role: node.role,
        node_name: node.name.clone(),
        k3s_version: config.k3s_version.clone(),
        server_mode,
        component_toggles: config.component_toggles,
        use_systemd: false,
        server_url: server_url.map(ToOwned::to_owned),
        token: token.map(ToOwned::to_owned),
    };
    let mut parts = spec.export_commands(shell_escape);
    parts.push(format!(
        "sh {}",
        shell_escape(helper_script.as_os_str().to_string_lossy().as_ref())
    ));
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::k3s::NodeRole;

    use super::{
        Artifacts, Instance, build_bootstrap_command, build_worker_instances, build_worker_names,
    };
    use crate::k3s::ServerMode;
    use crate::lima_k3s::config::Config;

    #[test]
    fn build_worker_names_returns_stable_pattern() {
        assert_eq!(
            build_worker_names(2, "1234"),
            vec!["kalisto-w0-1234".to_owned(), "kalisto-w1-1234".to_owned()]
        );
    }

    #[test]
    fn build_worker_instances_uses_expected_artifact_paths() {
        let workers = build_worker_instances(2, "1234", Path::new("/tmp/lima-probe"));
        assert_eq!(workers.len(), 2);
        assert_eq!(workers[0].name, "kalisto-w0-1234");
        assert_eq!(workers[1].name, "kalisto-w1-1234");
        assert_eq!(
            workers[0].artifacts.root,
            PathBuf::from("/tmp/lima-probe/kalisto-w0-1234")
        );
        assert_eq!(
            workers[1].artifacts.root,
            PathBuf::from("/tmp/lima-probe/kalisto-w1-1234")
        );
    }

    #[test]
    fn worker_bootstrap_command_exports_join_details() {
        let mut config = Config::new(PathBuf::from("."));
        config.server_mode = ServerMode::Standard;

        let node = Instance {
            name: "kalisto-w0-test".to_owned(),
            role: NodeRole::Worker,
            artifacts: Artifacts::new(PathBuf::from("/tmp/worker")),
        };

        let command = build_bootstrap_command(
            &config,
            Path::new("/workspace"),
            Path::new("/workspace/scripts/macos/k3s_server_probe.sh"),
            &node,
            Some("https://lima-kalisto-cp-test.example.test:6443"),
            Some("secret-token"),
        );

        assert!(command.contains("KALISTO_K3S_NODE_ROLE=agent"));
        assert!(
            command.contains(
                  "KALISTO_K3S_SERVER_URL='https://lima-kalisto-cp-test.example.test:6443'"
              )
        );
        assert!(command.contains("KALISTO_K3S_TOKEN='secret-token'"));
        assert!(command.contains("KALISTO_K3S_NODE_NAME='kalisto-w0-test'"));
    }
}
