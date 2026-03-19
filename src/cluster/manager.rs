use std::fs;
use std::time::{Duration, Instant};

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

    ensure_path_exists(&spec.workspace_root)?;
    let workspace_root = canonicalize_path(&spec.workspace_root)?;
    let helper_script = spec.helper_script();
    ensure_path_exists(&helper_script)?;

    let state_root = spec.state_root();
    let state_file = ClusterState::state_file(&state_root);
    if state_file.exists() {
        return Err(Error::InvalidArgs(format!(
            "cluster `{}` already exists at {}",
            spec.name,
            state_root.display()
        )));
    }

    fs::create_dir_all(&state_root).map_err(io_error("create_dir_all"))?;

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
        let _ = fs::remove_dir_all(&state_root);
        return Err(error);
    }
    started_instances.push(control_plane.name.clone());

    let k3s_config = k3s_config_from_spec(spec, &workspace_root);
    if let Err(error) = bootstrap_server(
        runner,
        &k3s_config,
        &workspace_root,
        &helper_script,
        &control_plane,
    ) {
        cleanup_instances(runner, &started_instances);
        let _ = fs::remove_dir_all(&state_root);
        return Err(error);
    }

    if let Err(error) = wait_for_path(
        &control_plane.artifacts.kubeconfig,
        Duration::from_secs(180),
    ) {
        cleanup_instances(runner, &started_instances);
        let _ = fs::remove_dir_all(&state_root);
        return Err(error);
    }
    if let Err(error) = wait_for_path(&control_plane.artifacts.token, Duration::from_secs(180)) {
        cleanup_instances(runner, &started_instances);
        let _ = fs::remove_dir_all(&state_root);
        return Err(error);
    }

    let initial_ready =
        initial_readiness_command(spec.server_mode, &control_plane.artifacts.kubeconfig);
    if let Err(error) = wait_for_stable_guest_success(
        runner,
        &control_plane.name,
        &initial_ready,
        Duration::from_secs(90),
        1,
        Duration::from_secs(2),
    ) {
        cleanup_instances(runner, &started_instances);
        let _ = fs::remove_dir_all(&state_root);
        return Err(error);
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
        cleanup_instances(runner, &started_instances);
        let _ = fs::remove_dir_all(&state_root);
        return Err(error);
    }

    let api_check = format!("https://127.0.0.1:{}/readyz", spec.api_port);
    if let Err(error) = wait_for_host_http(runner, &api_check, Duration::from_secs(90)) {
        cleanup_instances(runner, &started_instances);
        let _ = fs::remove_dir_all(&state_root);
        return Err(error);
    }
    let control_plane_ready = format_duration(cluster_started.elapsed());

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

    let mut start_results = Vec::new();
    let started_workers = start_worker_instances(
        runner,
        &probe_lima_config(spec, &workspace_root),
        &workspace_root,
        &worker_instances,
        &mut started_instances,
        &mut start_results,
    )
    .map_err(report_error)?;

    for started_worker in started_workers {
        let worker = started_worker.node;
        if let Err(error) = bootstrap_worker(
            runner,
            &k3s_config,
            &workspace_root,
            &helper_script,
            &worker,
            &server_url,
            &server_token,
        ) {
            cleanup_instances(runner, &started_instances);
            let _ = fs::remove_dir_all(&state_root);
            return Err(error);
        }
        if let Err(error) = wait_for_worker_process(runner, &worker.name) {
            cleanup_instances(runner, &started_instances);
            let _ = fs::remove_dir_all(&state_root);
            return Err(error);
        }
        if let Err(error) = wait_for_node_ready(runner, &control_plane, &worker.name) {
            cleanup_instances(runner, &started_instances);
            let _ = fs::remove_dir_all(&state_root);
            return Err(error);
        }
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
    state.save(&state_root)?;

    Ok(UpOutcome {
        state,
        control_plane_ready,
        cluster_ready: format_duration(cluster_started.elapsed()),
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
    let state_root = workspace_root.join(".kalisto").join("clusters").join(name);
    let state = ClusterState::load(&state_root)?;
    let instance_names: Vec<String> = state.nodes.iter().map(|node| node.name.clone()).collect();
    cleanup_instances(runner, &instance_names);
    fs::remove_dir_all(&state_root).map_err(io_error("remove_dir_all"))?;
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
    let state_root = workspace_root.join(".kalisto").join("clusters").join(name);
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
    Ok(ClusterStatus {
        name: state.name,
        state_root,
        kubeconfig_path: state.kubeconfig_path,
        nodes,
    })
}

pub fn kubeconfig_path(workspace_root: &std::path::Path, name: &str) -> Result<std::path::PathBuf> {
    let state_root = workspace_root.join(".kalisto").join("clusters").join(name);
    Ok(ClusterState::load(&state_root)?.kubeconfig_path)
}

fn probe_lima_config(
    spec: &ClusterSpec,
    workspace_root: &std::path::Path,
) -> crate::probe::macos_lima::ProbeConfig {
    crate::probe::macos_lima::ProbeConfig {
        template: spec.template.clone(),
        workspace_root: workspace_root.to_path_buf(),
        keep_resources: false,
        enable_rosetta: spec.enable_rosetta,
        network: spec.network.clone(),
    }
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
        let _ = stop_and_delete_instance(runner, instance_name);
    }
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
