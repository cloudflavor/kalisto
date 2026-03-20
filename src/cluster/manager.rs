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
    wait_for_host_http, wait_for_node_ready, wait_for_path, wait_for_stable_guest_success,
    wait_for_worker_process,
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

    let state_root = ClusterSpec::state_root_for(&workspace_root, &spec.name);
    let state_file = ClusterState::state_file(&state_root);
    if state_file.exists() {
        return Err(Error::InvalidArgs(format!(
            "cluster `{}` already exists at {}",
            spec.name,
            state_root.display()
        )));
    }

    fs::create_dir_all(&state_root).map_err(io_error("create_dir_all"))?;
    info!(
        workspace_root = %workspace_root.display(),
        helper_script = %helper_script.display(),
        state_root = %state_root.display(),
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
        state_root.join("control-plane"),
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
            error,
        ));
    }

    info!(node = %control_plane.name, "waiting for control-plane kubeconfig and join token");
    if let Err(error) = wait_for_path(
        &control_plane.artifacts.kubeconfig,
        Duration::from_secs(180),
    ) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
            error,
        ));
    }
    if let Err(error) = wait_for_path(&control_plane.artifacts.token, Duration::from_secs(180)) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
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
    if let Err(error) = wait_for_stable_guest_success(
        runner,
        &control_plane.name,
        &initial_ready,
        Duration::from_secs(90),
        1,
        Duration::from_secs(2),
    ) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
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
    if let Err(error) = wait_for_stable_guest_success(
        runner,
        &control_plane.name,
        &stable_ready_check,
        Duration::from_secs(30),
        3,
        Duration::from_secs(5),
    ) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
            error,
        ));
    }

    let api_check = format!("https://127.0.0.1:{}/readyz", spec.api_port);
    info!(url = %api_check, "waiting for control-plane API on the host");
    if let Err(error) = wait_for_host_http(runner, &api_check, Duration::from_secs(90)) {
        return Err(rollback_failed_up(
            runner,
            &started_instances,
            &state_root,
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
            Instance::new(name.clone(), NodeRole::Worker, state_root.join(&name))
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
                error,
            ));
        }
        info!(node = %worker.name, "waiting for worker process");
        if let Err(error) = wait_for_worker_process(runner, &worker.name) {
            return Err(rollback_failed_up(
                runner,
                &started_instances,
                &state_root,
                error,
            ));
        }
        info!(node = %worker.name, "waiting for worker readiness");
        if let Err(error) = wait_for_node_ready(runner, &control_plane, &worker.name) {
            return Err(rollback_failed_up(
                runner,
                &started_instances,
                &state_root,
                error,
            ));
        }
        info!(node = %worker.name, "worker is ready");
    }

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
        kubeconfig_path: control_plane.artifacts.kubeconfig.clone(),
        token_path: control_plane.artifacts.token.clone(),
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

pub fn down(workspace_root: &std::path::Path, name: &str) -> Result<()> {
    let runner = SystemRunner;
    down_with_runner(workspace_root, name, &runner)
}

pub fn down_with_runner(
    workspace_root: &std::path::Path,
    name: &str,
    runner: &dyn CommandRunner,
) -> Result<()> {
    info!(cluster = %name, "starting cluster teardown");
    let workspace_root = canonicalize_path(workspace_root)?;
    let state_root = ClusterSpec::state_root_for(&workspace_root, name);
    let state = ClusterState::load(&state_root)?;
    let instance_names: Vec<String> = state.nodes.iter().map(|node| node.name.clone()).collect();
    cleanup_instances(runner, &instance_names);
    fs::remove_dir_all(&state_root).map_err(io_error("remove_dir_all"))?;
    info!(cluster = %name, state_root = %state_root.display(), "cluster removed");
    Ok(())
}

pub fn status(workspace_root: &std::path::Path, name: &str) -> Result<ClusterStatus> {
    let runner = SystemRunner;
    status_with_runner(workspace_root, name, &runner)
}

pub fn status_with_runner(
    workspace_root: &std::path::Path,
    name: &str,
    runner: &dyn CommandRunner,
) -> Result<ClusterStatus> {
    let workspace_root = canonicalize_path(workspace_root)?;
    let state_root = ClusterSpec::state_root_for(&workspace_root, name);
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
    info!(cluster = %name, nodes = nodes.len(), "loaded cluster status");
    Ok(ClusterStatus {
        name: state.name,
        state_root,
        kubeconfig_path: state.kubeconfig_path,
        nodes,
    })
}

pub fn kubeconfig_path(workspace_root: &std::path::Path, name: &str) -> Result<std::path::PathBuf> {
    let workspace_root = canonicalize_path(workspace_root)?;
    let state_root = ClusterSpec::state_root_for(&workspace_root, name);
    let kubeconfig_path = ClusterState::load(&state_root)?.kubeconfig_path;
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
        down_with_runner(&spec.workspace_root, &run_name, runner)?;
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
    error: Error,
) -> Error {
    warn!(
        state_root = %state_root.display(),
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
    error
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
