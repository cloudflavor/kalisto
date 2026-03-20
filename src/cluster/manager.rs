use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use tracing::{info, info_span, warn};

use crate::backend::lima::{
    Config as LimaConfig, InstanceConfig, canonicalize_path, internal_hostname,
    query_instance_status, shell_escape, start_instance, stop_and_delete_instance,
};
use crate::cluster::spec::ClusterSpec;
use crate::cluster::state::{ClusterState, NodeState};
use crate::error::{Error, Result};
use crate::k3s::NodeRole;
use crate::lima_k3s::node::{Instance, bootstrap_server, bootstrap_worker, start_worker_instances};
use crate::lima_k3s::util::{format_duration, initial_readiness_command};
use crate::lima_k3s::wait::{
    LiveLogTail, wait_for_host_http_with_tail, wait_for_node_ready, wait_for_path_with_tail,
    wait_for_stable_guest_success_with_tail, wait_for_worker_process,
};
use crate::process::{CommandRunner, SystemRunner, ensure_path_exists};

#[derive(Debug, Clone)]
pub struct NodeStatus {
    pub name: String,
    pub role: NodeRole,
    pub lima_status: String,
}

#[derive(Debug, Clone)]
pub struct ClusterStatus {
    pub name: String,
    pub state_root: std::path::PathBuf,
    pub kubeconfig_path: std::path::PathBuf,
    pub nodes: Vec<NodeStatus>,
}

#[derive(Debug, Clone)]
pub struct UpOutcome {
    pub state: ClusterState,
    pub control_plane_ready: String,
    pub cluster_ready: String,
}

#[derive(Debug, Clone)]
pub struct BenchmarkSample {
    pub run: usize,
    pub cluster_name: String,
    pub control_plane_ready: String,
    pub cluster_ready: String,
}

pub fn up(spec: &ClusterSpec) -> Result<UpOutcome> {
    let runner = SystemRunner;
    up_with_runner(spec, &runner)
}

pub fn up_with_runner(spec: &ClusterSpec, runner: &dyn CommandRunner) -> Result<UpOutcome> {
    if !cfg!(target_os = "macos") {
        return Err(Error::InvalidArgs(
            "kalisto up currently supports only macOS".to_owned(),
        ));
    }

    let up_span = info_span!(
        "cluster_up",
        cluster = %spec.name,
        workers = spec.worker_count,
        api_port = spec.api_port,
        template = %spec.template
    );
    let _up_guard = up_span.enter();
    info!("starting cluster bring-up");

    ensure_path_exists(&spec.workspace_root)?;
    let workspace_root = canonicalize_path(&spec.workspace_root)?;
    let helper_script = ClusterSpec::helper_script_for(&workspace_root);
    ensure_path_exists(&helper_script)?;

    let state_root = ClusterSpec::state_root_for(&spec.name)?;
    let runtime_root = ClusterSpec::runtime_root_for(&workspace_root, &spec.name);
    let state_file = ClusterState::state_file(&state_root);
    if state_file.exists() {
        return Err(Error::InvalidArgs(format!(
            "cluster `{}` already exists at {}",
            spec.name,
            state_root.display()
        )));
    }

    fs::create_dir_all(&state_root).map_err(io_error("create_dir_all"))?;
    fs::create_dir_all(&runtime_root).map_err(io_error("create_dir_all"))?;
    info!(
        workspace_root = %workspace_root.display(),
        helper_script = %helper_script.display(),
        state_root = %state_root.display(),
        runtime_root = %runtime_root.display(),
        "resolved cluster paths"
    );

    runner.run("limactl", &["--version".into()])?;

    let base_config = LimaConfig {
        template: spec.template.clone(),
        workspace_root: workspace_root.clone(),
        enable_rosetta: spec.enable_rosetta,
        network: spec.network.clone(),
    };

    let control_plane = Instance::new(
        spec.control_plane_name(),
        NodeRole::ControlPlane,
        runtime_root.join("control-plane"),
    );
    fs::create_dir_all(&control_plane.artifacts.root).map_err(io_error("create_dir_all"))?;

    let mut started_instances = Vec::new();
    let cluster_started = Instant::now();

    info!(
        node = %control_plane.name,
        cpus = 4,
        memory_gib = 6,
        "starting control-plane VM"
    );
    let control_plane_start = start_instance(
        runner,
        &base_config,
        &InstanceConfig {
            name: control_plane.name.clone(),
            host_port_forward: Some(spec.api_port),
            cpus: 4,
            memory_gib: 6,
        },
    );
    if let Err(error) = control_plane_start {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            error,
        ));
    }
    started_instances.push(control_plane.name.clone());
    info!(node = %control_plane.name, "control-plane VM started");

    let k3s_config = k3s_config_from_spec(spec, &workspace_root);
    info!(
        node = %control_plane.name,
        mode = ?spec.server_mode,
        version = %spec.k3s_version,
        "bootstrapping control-plane"
    );
    if let Err(error) = bootstrap_server(
        runner,
        &k3s_config,
        &workspace_root,
        &helper_script,
        &control_plane,
    ) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            error,
        ));
    }

    let mut control_plane_tail = LiveLogTail::new(
        &control_plane.artifacts.stderr_log,
        format!("{}:k3s", control_plane.name),
    );
    info!(node = %control_plane.name, "waiting for control-plane kubeconfig and join token");
    if let Err(error) = wait_for_path_with_tail(
        &control_plane.artifacts.kubeconfig,
        Duration::from_secs(180),
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            error,
        ));
    }
    if let Err(error) = wait_for_path_with_tail(
        &control_plane.artifacts.token,
        Duration::from_secs(180),
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            error,
        ));
    }
    info!(
        kubeconfig = %control_plane.artifacts.kubeconfig.display(),
        token_path = %control_plane.artifacts.token.display(),
        "control-plane artifacts ready"
    );

    let initial_ready =
        initial_readiness_command(spec.server_mode, &control_plane.artifacts.kubeconfig);
    info!(node = %control_plane.name, "waiting for control-plane API in guest");
    if let Err(error) = wait_for_stable_guest_success_with_tail(
        runner,
        &control_plane.name,
        &initial_ready,
        Duration::from_secs(90),
        1,
        Duration::from_secs(2),
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            error,
        ));
    }

    let stable_ready_check = format!(
        "ps -ef | grep -F '[k]3s server' >/dev/null && KUBECONFIG={} /usr/local/bin/k3s kubectl get --raw=/readyz",
        shell_escape(
            control_plane
                .artifacts
                .kubeconfig
                .as_os_str()
                .to_string_lossy()
                .as_ref()
        )
    );
    if let Err(error) = wait_for_stable_guest_success_with_tail(
        runner,
        &control_plane.name,
        &stable_ready_check,
        Duration::from_secs(30),
        3,
        Duration::from_secs(5),
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            error,
        ));
    }

    let api_check = format!("https://127.0.0.1:{}/readyz", spec.api_port);
    info!(url = %api_check, "waiting for control-plane API on the host");
    if let Err(error) = wait_for_host_http_with_tail(
        runner,
        &api_check,
        Duration::from_secs(90),
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            error,
        ));
    }
    let control_plane_ready = format_duration(cluster_started.elapsed());
    info!(
        elapsed = %control_plane_ready,
        node = %control_plane.name,
        "control-plane is ready"
    );

    let server_token = fs::read_to_string(&control_plane.artifacts.token)
        .map_err(io_error("read_to_string"))?
        .trim()
        .to_owned();
    let server_url = format!("https://{}:6443", internal_hostname(&control_plane.name));

    let worker_instances: Vec<Instance> = (0..spec.worker_count)
        .map(|index| {
            let name = format!("kalisto-{}-w{index}", spec.name);
            Instance::new(name.clone(), NodeRole::Worker, runtime_root.join(&name))
        })
        .collect();

    if !worker_instances.is_empty() {
        info!(workers = worker_instances.len(), "starting worker VMs");
    }
    let mut start_results = Vec::new();
    let started_workers = start_worker_instances(
        runner,
        &base_config,
        &workspace_root,
        &worker_instances,
        &mut started_instances,
        &mut start_results,
    )
    .map_err(report_error)?;

    for started_worker in started_workers {
        let worker = started_worker.node;
        info!(node = %worker.name, "bootstrapping worker");
        if let Err(error) = bootstrap_worker(
            runner,
            &k3s_config,
            &workspace_root,
            &helper_script,
            &worker,
            &server_url,
            &server_token,
        ) {
            return Err(rollback_failed_up(
                runner,
                &started_instances,
                &state_root,
                &runtime_root,
                error,
            ));
        }
        info!(node = %worker.name, "waiting for worker process");
        if let Err(error) = wait_for_worker_process(runner, &worker.name) {
            return Err(rollback_failed_up(
                runner,
                &started_instances,
                &state_root,
                &runtime_root,
                error,
            ));
        }
        info!(node = %worker.name, "waiting for worker readiness");
        if let Err(error) = wait_for_node_ready(runner, &control_plane, &worker.name) {
            return Err(rollback_failed_up(
                runner,
                &started_instances,
                &state_root,
                &runtime_root,
                error,
            ));
        }
        info!(node = %worker.name, "worker is ready");
    }

    let persisted_kubeconfig = state_root.join("kubeconfig.yaml");
    let persisted_token = state_root.join("node-token");
    persist_kubeconfig(
        &control_plane.artifacts.kubeconfig,
        &persisted_kubeconfig,
        spec.api_port,
    )?;
    copy_artifact(
        &control_plane.artifacts.token,
        &persisted_token,
        "node token",
    )?;

    let state = ClusterState {
        name: spec.name.clone(),
        workspace_root: workspace_root.clone(),
        template: spec.template.clone(),
        api_port: spec.api_port,
        worker_count: spec.worker_count,
        enable_rosetta: spec.enable_rosetta,
        network: spec.network.clone(),
        k3s_version: spec.k3s_version.clone(),
        server_mode: spec.server_mode,
        kubeconfig_path: persisted_kubeconfig,
        token_path: persisted_token,
        nodes: std::iter::once(NodeState {
            name: control_plane.name.clone(),
            role: NodeRole::ControlPlane,
            artifacts_root: control_plane.artifacts.root.clone(),
        })
        .chain(worker_instances.iter().map(|worker| NodeState {
            name: worker.name.clone(),
            role: NodeRole::Worker,
            artifacts_root: worker.artifacts.root.clone(),
        }))
        .collect(),
    };
    info!(
        state_file = %ClusterState::state_file(&state_root).display(),
        "persisting cluster state"
    );
    state.save(&state_root)?;
    let cluster_ready = format_duration(cluster_started.elapsed());
    info!(
        cluster = %state.name,
        kubeconfig = %state.kubeconfig_path.display(),
        control_plane_ready = %control_plane_ready,
        cluster_ready = %cluster_ready,
        "cluster is ready"
    );

    Ok(UpOutcome {
        state,
        control_plane_ready,
        cluster_ready,
    })
}

pub fn down(name: &str) -> Result<()> {
    let runner = SystemRunner;
    down_with_runner(name, &runner)
}

pub fn down_with_runner(name: &str, runner: &dyn CommandRunner) -> Result<()> {
    info!(cluster = %name, "starting cluster teardown");
    let state_root = require_cluster_state_root(name)?;
    down_at_root(&state_root, name, runner)
}

fn down_at_root(state_root: &Path, name: &str, runner: &dyn CommandRunner) -> Result<()> {
    let state = ClusterState::load(state_root)?;
    let runtime_root = ClusterSpec::runtime_root_for(&state.workspace_root, name);
    let instance_names: Vec<String> = state.nodes.iter().map(|node| node.name.clone()).collect();
    cleanup_instances(runner, &instance_names);
    fs::remove_dir_all(state_root).map_err(io_error("remove_dir_all"))?;
    if runtime_root.exists() {
        fs::remove_dir_all(&runtime_root).map_err(io_error("remove_dir_all"))?;
    }
    info!(cluster = %name, state_root = %state_root.display(), "cluster removed");
    Ok(())
}

pub fn status(name: &str) -> Result<ClusterStatus> {
    let runner = SystemRunner;
    status_with_runner(name, &runner)
}

pub fn list() -> Result<Vec<ClusterStatus>> {
    let runner = SystemRunner;
    list_with_runner(&runner)
}

pub fn status_with_runner(name: &str, runner: &dyn CommandRunner) -> Result<ClusterStatus> {
    let state_root = require_cluster_state_root(name)?;
    status_at_root(&state_root, name, runner)
}

fn status_at_root(
    state_root: &Path,
    name: &str,
    runner: &dyn CommandRunner,
) -> Result<ClusterStatus> {
    let state = ClusterState::load(state_root)?;
    let mut nodes = Vec::with_capacity(state.nodes.len());
    for node in &state.nodes {
        let lima_status = query_instance_status(runner, &node.name)?;
        nodes.push(NodeStatus {
            name: node.name.clone(),
            role: node.role,
            lima_status,
        });
    }
    info!(cluster = %name, nodes = nodes.len(), "loaded cluster status");
    Ok(ClusterStatus {
        name: state.name,
        state_root: state_root.to_path_buf(),
        kubeconfig_path: state.kubeconfig_path,
        nodes,
    })
}

pub fn list_with_runner(runner: &dyn CommandRunner) -> Result<Vec<ClusterStatus>> {
    let clusters_root = ClusterSpec::clusters_root_for()?;
    list_at_root(&clusters_root, runner)
}

fn list_at_root(clusters_root: &Path, runner: &dyn CommandRunner) -> Result<Vec<ClusterStatus>> {
    if !clusters_root.exists() {
        info!(clusters_root = %clusters_root.display(), "no cluster state directory found");
        return Ok(Vec::new());
    }

    let mut cluster_roots = Vec::new();
    for entry in fs::read_dir(clusters_root).map_err(io_error("read_dir"))? {
        let entry = entry.map_err(io_error("read_dir"))?;
        let path = entry.path();
        if path.is_dir() && ClusterState::state_file(&path).is_file() {
            cluster_roots.push(path);
        }
    }
    cluster_roots.sort();

    let mut clusters = Vec::with_capacity(cluster_roots.len());
    for state_root in cluster_roots {
        let state = ClusterState::load(&state_root)?;
        let mut nodes = Vec::with_capacity(state.nodes.len());
        for node in &state.nodes {
            let lima_status = query_instance_status(runner, &node.name)?;
            nodes.push(NodeStatus {
                name: node.name.clone(),
                role: node.role,
                lima_status,
            });
        }
        clusters.push(ClusterStatus {
            name: state.name.clone(),
            state_root: state_root.clone(),
            kubeconfig_path: state.kubeconfig_path.clone(),
            nodes,
        });
    }

    info!(clusters = clusters.len(), "listed known clusters");
    Ok(clusters)
}

pub fn kubeconfig_path(name: &str) -> Result<std::path::PathBuf> {
    let state_root = require_cluster_state_root(name)?;
    let kubeconfig_path = ClusterState::load(state_root.as_path())?.kubeconfig_path;
    info!(
        cluster = %name,
        kubeconfig = %kubeconfig_path.display(),
        "resolved kubeconfig path"
    );
    Ok(kubeconfig_path)
}

pub fn benchmark(spec: &ClusterSpec, runs: usize) -> Result<Vec<BenchmarkSample>> {
    let runner = SystemRunner;
    benchmark_with_runner(spec, runs, &runner)
}

pub fn benchmark_with_runner(
    spec: &ClusterSpec,
    runs: usize,
    runner: &dyn CommandRunner,
) -> Result<Vec<BenchmarkSample>> {
    if runs == 0 {
        return Err(Error::InvalidArgs(
            "benchmark runs must be greater than zero".to_owned(),
        ));
    }

    let mut samples = Vec::with_capacity(runs);
    for run in 0..runs {
        let run_name = format!("{}-bench-{}", spec.name, run + 1);
        let mut run_spec = spec.clone();
        run_spec.name = run_name.clone();
        info!(
            benchmark_run = run + 1,
            benchmark_total = runs,
            cluster = %run_name,
            "starting benchmark run"
        );

        let outcome = up_with_runner(&run_spec, runner)?;
        samples.push(BenchmarkSample {
            run: run + 1,
            cluster_name: run_name.clone(),
            control_plane_ready: outcome.control_plane_ready,
            cluster_ready: outcome.cluster_ready,
        });
        down_with_runner(&run_name, runner)?;
        if let Some(sample) = samples.last() {
            info!(
                benchmark_run = sample.run,
                cluster = %sample.cluster_name,
                control_plane_ready = %sample.control_plane_ready,
                cluster_ready = %sample.cluster_ready,
                "completed benchmark run"
            );
        }
    }

    Ok(samples)
}

fn k3s_config_from_spec(
    spec: &ClusterSpec,
    workspace_root: &std::path::Path,
) -> crate::lima_k3s::config::Config {
    crate::lima_k3s::config::Config {
        template: spec.template.clone(),
        workspace_root: workspace_root.to_path_buf(),
        api_port: spec.api_port,
        keep_resources: false,
        k3s_version: spec.k3s_version.clone(),
        server_mode: spec.server_mode,
        component_toggles: spec.component_toggles,
        enable_rosetta: spec.enable_rosetta,
        worker_count: spec.worker_count,
        network: Some(spec.network.clone()),
    }
}

fn cleanup_instances(runner: &dyn CommandRunner, instance_names: &[String]) {
    for instance_name in instance_names.iter().rev() {
        info!(instance = %instance_name, "cleaning up lima instance");
        let _ = stop_and_delete_instance(runner, instance_name);
    }
}

fn rollback_failed_up(
    runner: &dyn CommandRunner,
    started_instances: &[String],
    state_root: &Path,
    runtime_root: &Path,
    error: Error,
) -> Error {
    warn!(
        state_root = %state_root.display(),
        runtime_root = %runtime_root.display(),
        error = %error,
        "cluster bring-up failed; cleaning up partial state"
    );
    cleanup_instances(runner, started_instances);
    if let Err(cleanup_error) = fs::remove_dir_all(state_root) {
        warn!(
            state_root = %state_root.display(),
            error = %cleanup_error,
            "failed to remove partial cluster state"
        );
    }
    if let Err(cleanup_error) = fs::remove_dir_all(runtime_root) {
        warn!(
            runtime_root = %runtime_root.display(),
            error = %cleanup_error,
            "failed to remove partial cluster runtime artifacts"
        );
    }
    error
}

fn copy_artifact(source: &Path, destination: &Path, label: &str) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(io_error("create_dir_all"))?;
    }
    fs::copy(source, destination).map_err(io_error("copy"))?;
    info!(
        artifact = label,
        source = %source.display(),
        destination = %destination.display(),
        "copied cluster artifact into persisted state"
    );
    Ok(())
}

fn persist_kubeconfig(source: &Path, destination: &Path, api_port: u16) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(io_error("create_dir_all"))?;
    }

    let kubeconfig = fs::read_to_string(source).map_err(io_error("read_to_string"))?;
    let rewritten = rewrite_kubeconfig_server(&kubeconfig, api_port)?;
    fs::write(destination, rewritten).map_err(io_error("write"))?;
    info!(
        artifact = "kubeconfig",
        source = %source.display(),
        destination = %destination.display(),
        api_port,
        "copied cluster artifact into persisted state"
    );
    Ok(())
}

fn rewrite_kubeconfig_server(kubeconfig: &str, api_port: u16) -> Result<String> {
    let mut found_server = false;
    let rewritten = kubeconfig
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("server: ") {
                found_server = true;
                format!("    server: https://127.0.0.1:{api_port}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if !found_server {
        return Err(Error::InvalidArgs(
            "kubeconfig did not contain a server endpoint".to_owned(),
        ));
    }

    Ok(rewritten)
}

fn require_cluster_state_root(name: &str) -> Result<std::path::PathBuf> {
    let state_root = ClusterSpec::state_root_for(name)?;
    let state_file = ClusterState::state_file(&state_root);
    if !state_file.is_file() {
        return Err(Error::InvalidArgs(format!(
            "cluster `{name}` not found at {}",
            state_root.display()
        )));
    }
    Ok(state_root)
}

fn io_error(program: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::CommandIo {
        program: program.to_owned(),
        source,
    }
}

fn report_error(report: crate::lima_k3s::report::Report) -> Error {
    Error::InvalidArgs(
        report
            .results
            .last()
            .map(|result| result.detail.clone())
            .unwrap_or_else(|| "unknown worker startup failure".to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use crate::cluster::state::{ClusterState, NodeState};
    use crate::error::Error;
    use crate::k3s::{NodeRole, ServerMode};
    use crate::process::{CommandRunner, Output};

    use super::{down_at_root, list_at_root};

    #[derive(Default)]
    struct FakeRunner {
        statuses: HashMap<String, String>,
        invocations: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeRunner {
        fn with_statuses(statuses: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
            Self {
                statuses: statuses
                    .into_iter()
                    .map(|(name, status)| (name.to_owned(), status.to_owned()))
                    .collect(),
                invocations: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[String]) -> crate::error::Result<Output> {
            self.invocations
                .lock()
                .unwrap()
                .push((program.to_owned(), args.to_vec()));
            if program == "limactl"
                && args.first().map(String::as_str) == Some("list")
                && args.last().is_some()
            {
                let instance_name = args.last().unwrap();
                let status = self
                    .statuses
                    .get(instance_name)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_owned());
                return Ok(Output {
                    stdout: format!("{instance_name} {status} vz\n"),
                    stderr: String::new(),
                });
            }

            Ok(Output {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn list_returns_sorted_clusters() {
        let root = std::env::temp_dir().join(format!("kalisto-list-{}", std::process::id()));
        let workspace_root = root.join("workspace");
        let clusters_root = workspace_root.join(".kalisto").join("clusters");
        fs::create_dir_all(&clusters_root).unwrap();

        save_state(
            &clusters_root.join("beta"),
            "beta",
            vec![("kalisto-beta-cp", NodeRole::ControlPlane)],
        );
        save_state(
            &clusters_root.join("alpha"),
            "alpha",
            vec![
                ("kalisto-alpha-cp", NodeRole::ControlPlane),
                ("kalisto-alpha-w0", NodeRole::Worker),
            ],
        );

        let runner = FakeRunner::with_statuses([
            ("kalisto-alpha-cp", "Running"),
            ("kalisto-alpha-w0", "Running"),
            ("kalisto-beta-cp", "Stopped"),
        ]);

        let clusters = list_at_root(&clusters_root, &runner).unwrap();
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].name, "alpha");
        assert_eq!(clusters[0].nodes.len(), 2);
        assert_eq!(
            clusters[0].nodes[0].lima_status,
            "kalisto-alpha-cp Running vz"
        );
        assert_eq!(clusters[1].name, "beta");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn down_removes_cluster_state_directory() {
        let root = std::env::temp_dir().join(format!("kalisto-down-{}", std::process::id()));
        let workspace_root = root.join("workspace");
        let state_root = workspace_root.join(".kalisto").join("clusters").join("dev");
        fs::create_dir_all(&state_root).unwrap();
        save_state(
            &state_root,
            "dev",
            vec![
                ("kalisto-dev-cp", NodeRole::ControlPlane),
                ("kalisto-dev-w0", NodeRole::Worker),
            ],
        );

        let runner = FakeRunner::default();
        down_at_root(&state_root, "dev", &runner).unwrap();
        assert!(!state_root.exists());

        let _ = fs::remove_dir_all(root);
    }

    fn save_state(root: &std::path::Path, name: &str, nodes: Vec<(&str, NodeRole)>) {
        let state = ClusterState {
            name: name.to_owned(),
            workspace_root: root.ancestors().nth(3).unwrap().to_path_buf(),
            template: "template://default".to_owned(),
            api_port: 16443,
            worker_count: nodes
                .iter()
                .filter(|(_, role)| *role == NodeRole::Worker)
                .count(),
            enable_rosetta: true,
            network: "lima:user-v2".to_owned(),
            k3s_version: "v1.33.5+k3s1".to_owned(),
            server_mode: ServerMode::Standard,
            kubeconfig_path: root.join("kubeconfig.yaml"),
            token_path: root.join("node-token"),
            nodes: nodes
                .into_iter()
                .map(|(node_name, role)| NodeState {
                    name: node_name.to_owned(),
                    role,
                    artifacts_root: root.join(node_name),
                })
                .collect(),
        };
        state.save(root).unwrap();
    }

    #[test]
    fn list_returns_empty_when_no_clusters_exist() {
        let root = std::env::temp_dir().join(format!("kalisto-empty-{}", std::process::id()));
        let workspace_root = root.join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();

        let runner = FakeRunner::default();
        let clusters_root = workspace_root.join(".kalisto").join("clusters");
        let clusters = list_at_root(&clusters_root, &runner).unwrap();
        assert!(clusters.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn list_propagates_invalid_state() {
        let root = std::env::temp_dir().join(format!("kalisto-bad-state-{}", std::process::id()));
        let workspace_root = root.join("workspace");
        let state_root = workspace_root
            .join(".kalisto")
            .join("clusters")
            .join("broken");
        fs::create_dir_all(&state_root).unwrap();
        fs::write(state_root.join("cluster.state"), "name\tbroken\n").unwrap();

        let runner = FakeRunner::default();
        let clusters_root = workspace_root.join(".kalisto").join("clusters");
        let error = list_at_root(&clusters_root, &runner).unwrap_err();
        match error {
            Error::InvalidArgs(message) => assert!(message.contains("missing workspace_root")),
            other => panic!("unexpected error: {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rewrite_kubeconfig_server_updates_forwarded_host_port() {
        let kubeconfig = "\
apiVersion: v1
clusters:
- cluster:
    certificate-authority-data: test
    server: https://127.0.0.1:6443
  name: default
";

        let rewritten = super::rewrite_kubeconfig_server(kubeconfig, 16443).unwrap();
        assert!(rewritten.contains("server: https://127.0.0.1:16443"));
        assert!(!rewritten.contains("server: https://127.0.0.1:6443"));
    }

    #[test]
    fn rewrite_kubeconfig_server_requires_server_line() {
        let error = super::rewrite_kubeconfig_server("apiVersion: v1\n", 16443).unwrap_err();
        match error {
            Error::InvalidArgs(message) => {
                assert!(message.contains("did not contain a server endpoint"))
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
