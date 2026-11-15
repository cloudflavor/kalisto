use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use tokio::runtime::Builder as TokioRuntimeBuilder;
use tracing::{debug, info, info_span, warn};

use crate::artifact::resolve_cluster_spec;
use crate::backend::lima::canonicalize_path;
use crate::backend::{BackendConfig, BackendKind, ClusterBackend, InstanceConfig, backend_for};
use crate::cluster::addon::{
    AddonId, CniPlugin, PostReadyAddonRequest, install_catalog_addon_with_cilium_version,
    install_post_ready_addons,
};
use crate::cluster::node::{
    Instance, JoinConfig, K3sBootstrapConfig, StartedWorker, bootstrap_server, bootstrap_worker,
    start_node_instance, start_worker_instances,
};
use crate::cluster::spec::ClusterSpec;
use crate::cluster::state::{ClusterState, NodeState};
use crate::cluster::util::{format_duration, initial_readiness_command};
use crate::cluster::wait::{
    GuestFileCopy, GuestWait, LiveLogTail, wait_for_guest_text_file_copy,
    wait_for_host_http_with_tail, wait_for_node_ready, wait_for_stable_guest_success_with_tail,
    wait_for_worker_process,
};
use crate::config::load::load_project_config;
use crate::error::{Error, Result};
use crate::k3s::{NodeRole, ServerMode};
use crate::process::{CommandRunner, SystemRunner, ensure_path_exists};
use crate::shell::shell_escape;
use crate::telemetry::raw_streams_enabled;

#[derive(Debug, Clone)]
pub struct NodeStatus {
    pub name: String,
    pub role: NodeRole,
    pub backend_status: String,
}

#[derive(Debug, Clone)]
pub struct ClusterStatus {
    pub name: String,
    pub state_root: std::path::PathBuf,
    pub kubeconfig_path: std::path::PathBuf,
    pub nodes: Vec<NodeStatus>,
}

#[derive(Debug, Clone)]
pub struct NodeStats {
    pub name: String,
    pub role: NodeRole,
    pub backend_status: String,
    pub configured_cpus: u8,
    pub configured_memory_gib: u8,
    pub pid: Option<u32>,
    pub guest_ip: Option<String>,
    pub rss_bytes: Option<u64>,
    pub vsz_bytes: Option<u64>,
    pub rootfs_bytes: Option<u64>,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ClusterStats {
    pub name: String,
    pub state_root: std::path::PathBuf,
    pub kubeconfig_path: std::path::PathBuf,
    pub nodes: Vec<NodeStats>,
}

#[derive(Debug, Clone)]
pub struct NodeLogSection {
    pub label: String,
    pub path: std::path::PathBuf,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct NodeLogs {
    pub cluster: String,
    pub node: String,
    pub sections: Vec<NodeLogSection>,
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

#[derive(Debug, Clone)]
pub struct ScaleOutcome {
    pub name: String,
    pub previous_workers: usize,
    pub current_workers: usize,
    pub added_workers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AddonInstallOutcome {
    pub cluster: String,
    pub addon: String,
}

#[derive(Debug, Clone)]
pub struct CreateOutcome {
    pub state: ClusterState,
}

#[derive(Clone, Copy)]
struct WorkerJoinContext<'a> {
    base_config: &'a BackendConfig,
    k3s_config: &'a K3sBootstrapConfig,
    control_plane: &'a Instance,
    control_plane_guest_kubeconfig: &'a Path,
    worker_cpus: u8,
    worker_memory_gib: u8,
    server_url: &'a str,
    server_token: &'a str,
    prestarted_at: Option<Instant>,
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn up(spec: &ClusterSpec) -> Result<UpOutcome> {
    let runner = SystemRunner;
    up_with_runner(spec, &runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn create(spec: &ClusterSpec) -> Result<CreateOutcome> {
    let runner = SystemRunner;
    create_with_runner(spec, &runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn create_with_runner(spec: &ClusterSpec, runner: &dyn CommandRunner) -> Result<CreateOutcome> {
    debug_feedback(format!(
        "create: resolving cluster spec for `{}` ({})",
        spec.name,
        spec.backend.as_str()
    ));
    let resolved_spec = resolve_cluster_spec(spec, runner)?;
    let state_root = ClusterSpec::state_root_for(&resolved_spec.name)?;
    let _operation = crate::operation::lock(&state_root)?;
    let state_file = ClusterState::state_file(&state_root);
    if state_root.join("archive-import-incomplete").exists() && !state_file.exists() {
        return Err(Error::InvalidArgs("interrupted archive import owns this destination; inspect it before retrying, do not overwrite".into()));
    }
    if state_file.exists() {
        return Err(Error::InvalidArgs(format!(
            "cluster `{}` already exists at {}",
            resolved_spec.name,
            state_root.display()
        )));
    }

    ensure_path_exists(&resolved_spec.workspace_root)?;
    let workspace_root = canonicalize_path(&resolved_spec.workspace_root)?;
    let runtime_root = state_root.join("runtime");

    let backend = backend_for(resolved_spec.backend);
    debug_feedback(format!(
        "create: checking backend availability for `{}`",
        resolved_spec.backend.as_str()
    ));
    backend.check_available(runner)?;

    fs::create_dir_all(&state_root).map_err(io_error("create_dir_all"))?;
    fs::create_dir_all(&runtime_root).map_err(io_error("create_dir_all"))?;
    debug_feedback(format!(
        "create: prepared state root `{}` and runtime root `{}`",
        state_root.display(),
        runtime_root.display()
    ));

    let state = cluster_state_from_spec(&resolved_spec, workspace_root.clone(), &state_root);
    let base_config = BackendConfig {
        template: resolved_spec.template.clone(),
        workspace_root: workspace_root.clone(),
        enable_rosetta: resolved_spec.enable_rosetta,
        network: resolved_spec.network.clone(),
        firecracker_kernel: resolved_spec.firecracker_kernel.clone(),
        firecracker_rootfs: resolved_spec.firecracker_rootfs.clone(),
        boot_assets_root: None,
        stream_vm_logs: resolved_spec.stream_vm_logs,
    };
    let instance_names = state
        .nodes
        .iter()
        .map(|node| node.name.clone())
        .collect::<Vec<_>>();
    debug_feedback(format!(
        "create: preparing {} node network target(s) with network `{}`",
        instance_names.len(),
        resolved_spec.network
    ));
    if let Err(error) = backend.prepare_cluster_network(runner, &base_config, &instance_names) {
        let _ = backend.cleanup_cluster_network(runner, &base_config, &instance_names);
        let _ = fs::remove_dir_all(&state_root);
        let _ = fs::remove_dir_all(&runtime_root);
        return Err(error);
    }

    debug_feedback(format!(
        "create: persisting cluster state to `{}`",
        state_root.display()
    ));
    state.save(&state_root)?;
    if let Err(error) = handoff_cluster_paths_to_invoking_user(runner, &state_root, &runtime_root) {
        warn!(
            error = %error,
            state_root = %state_root.display(),
            runtime_root = %runtime_root.display(),
            "failed to hand off cluster files to invoking user"
        );
    }

    Ok(CreateOutcome { state })
}

#[allow(clippy::too_many_lines)] // orchestrator; split tracked for a follow-up
/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn up_with_runner(spec: &ClusterSpec, runner: &dyn CommandRunner) -> Result<UpOutcome> {
    let state_root = ClusterSpec::state_root_for(&spec.name)?;
    let state_file = ClusterState::state_file(&state_root);
    if !state_file.exists() {
        if spec.backend == crate::backend::BackendKind::Lima {
            crate::pull::load(spec)?;
            let resolved = resolve_cluster_spec(spec, runner)?;
            crate::pull::validate_lima_template(&resolved.template)?;
        }
        debug_feedback(format!(
            "up: cluster `{}` has no persisted definition, creating it first",
            spec.name
        ));
        create_with_runner(spec, runner)?;
    }
    let _operation = crate::operation::lock(&state_root)?;
    let state = ClusterState::load(&state_root)?;
    if cluster_has_running_nodes(runner, &state)? {
        info!(cluster = %state.name, "resuming existing cluster bring-up");
    }
    let resolved_spec = resolve_up_spec(&state, spec);
    let spec = &resolved_spec;
    let prepared = if spec.backend == crate::backend::BackendKind::Lima {
        let assets = crate::pull::load(spec)?;
        crate::pull::validate_lima_template(&spec.template)?;
        Some(assets)
    } else {
        None
    };
    let up_span = info_span!(
        "cluster_up",
        cluster = %spec.name,
        backend = spec.backend.as_str(),
        workers = spec.worker_count,
        api_port = spec.api_port,
        template = %spec.template
    );
    let _up_guard = up_span.enter();
    info!("starting cluster bring-up");
    debug_feedback(format!(
        "up: starting cluster `{}` with backend `{}` and {} worker(s)",
        spec.name,
        spec.backend.as_str(),
        spec.worker_count
    ));

    ensure_path_exists(&spec.workspace_root)?;
    let workspace_root = canonicalize_path(&spec.workspace_root)?;

    let runtime_root = state_root.join("runtime");

    let backend = backend_for(spec.backend);
    backend.check_available(runner)?;

    fs::create_dir_all(&state_root).map_err(io_error("create_dir_all"))?;
    fs::create_dir_all(&runtime_root).map_err(io_error("create_dir_all"))?;
    info!(
        backend = spec.backend.as_str(),
        workspace_root = %workspace_root.display(),
        state_root = %state_root.display(),
        runtime_root = %runtime_root.display(),
        "resolved cluster paths"
    );
    debug_feedback(format!(
        "up: using state root `{}` and runtime root `{}`",
        state_root.display(),
        runtime_root.display()
    ));

    let base_config = BackendConfig {
        template: spec.template.clone(),
        workspace_root: workspace_root.clone(),
        enable_rosetta: spec.enable_rosetta,
        network: spec.network.clone(),
        firecracker_kernel: spec.firecracker_kernel.clone(),
        firecracker_rootfs: spec.firecracker_rootfs.clone(),
        boot_assets_root: prepared.clone(),
        stream_vm_logs: spec.stream_vm_logs,
    };

    let control_plane = Instance::new(
        spec.control_plane_name(),
        NodeRole::ControlPlane,
        runtime_root.join("control-plane"),
    );
    fs::create_dir_all(&control_plane.artifacts.root).map_err(io_error("create_dir_all"))?;

    let worker_instances: Vec<Instance> = (0..spec.worker_count)
        .map(|index| {
            let name = format!("kalisto-{}-w{index}", spec.name);
            Instance::new(name.clone(), NodeRole::Worker, runtime_root.join(&name))
        })
        .collect();
    let nodes = std::iter::once(control_plane.clone())
        .chain(worker_instances.iter().cloned())
        .collect::<Vec<_>>();
    let mut started_instances = Vec::new();
    let cluster_started = Instant::now();

    // VM boot does not depend on the server join token. Start every VM now,
    // then join the already-booted agents once the control-plane API is ready.
    if let Err(error) = start_cluster_instances(
        backend,
        runner,
        &base_config,
        spec,
        &nodes,
        &mut started_instances,
    ) {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }
    info!(node = %control_plane.name, "control-plane VM started");
    debug_feedback(format!(
        "up: control-plane VM `{}` started, bootstrapping k3s",
        control_plane.name
    ));

    let control_plane_host =
        backend.control_plane_host(&control_plane.name, &control_plane.artifacts.root)?;
    let api_endpoint = backend.host_api_endpoint(
        &control_plane.name,
        &control_plane.artifacts.root,
        spec.api_port,
    )?;
    let control_plane_tls_sans = tls_sans_for_control_plane(&control_plane_host, &api_endpoint);
    let control_plane_guest_artifacts =
        backend.guest_artifact_root(&control_plane.name, &control_plane.artifacts.root)?;
    let control_plane_guest_kubeconfig = control_plane_guest_artifacts.join("kubeconfig.yaml");
    let control_plane_guest_token =
        Path::new("/var/lib/rancher/k3s/server/node-token").to_path_buf();

    let k3s_config = K3sBootstrapConfig {
        k3s_version: spec.k3s_version.clone(),
        server_mode: spec.server_mode,
        component_toggles: spec.component_toggles,
    };
    info!(
        node = %control_plane.name,
        mode = ?spec.server_mode,
        version = %spec.k3s_version,
        "bootstrapping control-plane"
    );
    if let Err(error) = bootstrap_server(
        backend,
        runner,
        &k3s_config,
        &control_plane,
        &control_plane_tls_sans,
    ) {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }

    let mut control_plane_tail = LiveLogTail::new(
        &control_plane.artifacts.stderr_log,
        format!("{}:k3s", control_plane.name),
    );
    info!(node = %control_plane.name, "waiting for control-plane kubeconfig and join token");
    debug_feedback("up: waiting for control-plane kubeconfig and join token");
    if let Err(error) = sync_control_plane_artifact(
        backend,
        runner,
        &control_plane,
        &control_plane_guest_kubeconfig,
        &control_plane.artifacts.kubeconfig,
        Duration::from_secs(180),
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }
    if let Err(error) = sync_control_plane_artifact(
        backend,
        runner,
        &control_plane,
        &control_plane_guest_token,
        &control_plane.artifacts.token,
        Duration::from_secs(180),
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }
    info!(
        kubeconfig = %control_plane.artifacts.kubeconfig.display(),
        token_path = %control_plane.artifacts.token.display(),
        "control-plane artifacts ready"
    );

    let initial_ready =
        initial_readiness_command(spec.server_mode, &control_plane_guest_kubeconfig);
    info!(node = %control_plane.name, "waiting for control-plane API in guest");
    debug_feedback("up: waiting for control-plane API in guest");
    if let Err(error) = wait_for_stable_guest_success_with_tail(
        backend,
        runner,
        GuestWait {
            instance_name: &control_plane.name,
            runtime_root: &control_plane.artifacts.root,
            command: &initial_ready,
            timeout: Duration::from_secs(90),
            consecutive_successes: 1,
            interval: Duration::from_secs(2),
        },
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }

    // Avoid the agent's connection backoff before the API first accepts requests.
    // Agents can import images and retry their join while stable API checks
    // run; the token already exists, and all final readiness gates remain below.
    let server_token = fs::read_to_string(&control_plane.artifacts.token)
        .map_err(io_error("read_to_string"))?
        .trim()
        .to_owned();
    let server_url = format!("https://{control_plane_host}:6443");
    if !worker_instances.is_empty() {
        info!(
            workers = worker_instances.len(),
            "bootstrapping workers with join token"
        );
    }
    if let Err(error) = start_and_join_workers(
        backend,
        runner,
        WorkerJoinContext {
            base_config: &base_config,
            k3s_config: &k3s_config,
            control_plane: &control_plane,
            control_plane_guest_kubeconfig: &control_plane_guest_kubeconfig,
            worker_cpus: spec.worker_resources.cpus,
            worker_memory_gib: spec.worker_resources.memory_gib,
            server_url: &server_url,
            server_token: &server_token,
            prestarted_at: Some(cluster_started),
        },
        &worker_instances,
        &mut started_instances,
        false,
    ) {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }

    let stable_ready_check = format!(
        "KUBECONFIG={} /usr/local/bin/k3s kubectl --request-timeout=5s get --raw=/readyz",
        shell_escape(
            control_plane_guest_kubeconfig
                .as_os_str()
                .to_string_lossy()
                .as_ref()
        )
    );
    if let Err(error) = wait_for_stable_guest_success_with_tail(
        backend,
        runner,
        GuestWait {
            instance_name: &control_plane.name,
            runtime_root: &control_plane.artifacts.root,
            command: &stable_ready_check,
            timeout: Duration::from_secs(120),
            consecutive_successes: 3,
            interval: Duration::from_secs(1),
        },
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }

    let api_check = format!("https://{api_endpoint}/livez");
    info!(url = %api_check, "waiting for control-plane API on the host");
    debug_feedback(format!("up: waiting for control-plane API at {api_check}"));
    if let Err(error) = wait_for_host_http_with_tail(
        runner,
        &api_check,
        Duration::from_secs(90),
        &mut control_plane_tail,
    ) {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }
    let control_plane_ready = format_duration(cluster_started.elapsed());
    info!(
        elapsed = %control_plane_ready,
        node = %control_plane.name,
        "control-plane is ready"
    );
    debug_feedback(format!("up: control-plane ready in {control_plane_ready}"));

    if let Err(error) = install_addons_and_wait_for_nodes(
        PostReadyAddonRequest {
            backend,
            runner,
            control_plane: &control_plane,
            guest_kubeconfig: &control_plane_guest_kubeconfig,
            cni: spec.cni_plugin,
            builtin_flannel: !spec.component_toggles.flannel_backend_none,
            cilium_version: &spec.cilium_version,
            addons: &spec.addons,
            metallb: &spec.metallb,
        },
        &worker_instances,
        spec.server_mode,
        prepared.as_ref(),
    ) {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }

    if state_root.join("archive/assets/manifest.json").is_file()
        && let Err(error) = crate::archive::wait_for_fresh_nodes(
            backend,
            runner,
            &control_plane,
            &nodes,
            &control_plane_guest_kubeconfig,
        )
    {
        return Err(rollback_failed_up(
            backend,
            runner,
            &started_instances,
            &state_root,
            &runtime_root,
            spec.keep_failed_runtime,
            error,
        ));
    }

    let persisted_kubeconfig = state_root.join("kubeconfig.yaml");
    let persisted_token = state_root.join("node-token");
    persist_kubeconfig(
        &control_plane.artifacts.kubeconfig,
        &persisted_kubeconfig,
        &api_endpoint,
    )?;
    copy_artifact(
        &control_plane.artifacts.token,
        &persisted_token,
        "node token",
    )?;

    let state = ClusterState {
        name: spec.name.clone(),
        backend: spec.backend,
        workspace_root: workspace_root.clone(),
        template: spec.template.clone(),
        firecracker_kernel: spec.firecracker_kernel.clone(),
        firecracker_rootfs: spec.firecracker_rootfs.clone(),
        api_port: spec.api_port,
        worker_count: spec.worker_count,
        control_plane_resources: spec.control_plane_resources,
        worker_resources: spec.worker_resources,
        enable_rosetta: spec.enable_rosetta,
        network: spec.network.clone(),
        k3s_version: spec.k3s_version.clone(),
        cni_plugin: spec.cni_plugin,
        cilium_version: spec.cilium_version.clone(),
        addons: spec.addons.clone(),
        metallb: spec.metallb.clone(),
        server_mode: spec.server_mode,
        component_toggles: spec.component_toggles,
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
    debug_feedback(format!(
        "up: persisting cluster state to `{}`",
        ClusterState::state_file(&state_root).display()
    ));
    state.save(&state_root)?;
    if let Err(error) = handoff_cluster_paths_to_invoking_user(runner, &state_root, &runtime_root) {
        warn!(
            error = %error,
            state_root = %state_root.display(),
            runtime_root = %runtime_root.display(),
            "failed to hand off cluster files to invoking user"
        );
    }
    let cluster_ready = format_duration(cluster_started.elapsed());
    info!(
        cluster = %state.name,
        kubeconfig = %state.kubeconfig_path.display(),
        control_plane_ready = %control_plane_ready,
        cluster_ready = %cluster_ready,
        "cluster is ready"
    );
    debug_feedback(format!("up: cluster ready in {cluster_ready}"));

    Ok(UpOutcome {
        state,
        control_plane_ready,
        cluster_ready,
    })
}

fn start_planned_instance(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    config: &BackendConfig,
    plan: &InstanceConfig,
) -> Result<()> {
    info!(
        node = %plan.name,
        cpus = plan.cpus,
        memory_gib = plan.memory_gib,
        "starting node VM"
    );
    let role = if plan.host_port_forward.is_some() {
        NodeRole::ControlPlane
    } else {
        NodeRole::Worker
    };
    let node = Instance::new(plan.name.clone(), role, plan.runtime_root.clone());
    start_node_instance(
        backend,
        runner,
        config,
        &node,
        plan.host_port_forward,
        plan.cpus,
        plan.memory_gib,
    )?;
    info!(node = %plan.name, "node VM started");
    Ok(())
}

fn start_cluster_instances(
    backend: &'static dyn ClusterBackend,
    runner: &dyn CommandRunner,
    config: &BackendConfig,
    spec: &ClusterSpec,
    nodes: &[Instance],
    instances: &mut Vec<NodeState>,
) -> Result<()> {
    let mut plans = Vec::with_capacity(nodes.len());
    for node in nodes {
        fs::create_dir_all(&node.artifacts.root).map_err(io_error("create_dir_all"))?;
        let resources = match node.role {
            NodeRole::ControlPlane => spec.control_plane_resources,
            NodeRole::Worker => spec.worker_resources,
        };
        plans.push(InstanceConfig {
            name: node.name.clone(),
            runtime_root: node.artifacts.root.clone(),
            host_port_forward: (node.role == NodeRole::ControlPlane).then_some(spec.api_port),
            cpus: resources.cpus,
            memory_gib: resources.memory_gib,
        });
        // A failed start can still leave a live VM. Rollback owns every attempt.
        instances.push(NodeState {
            name: node.name.clone(),
            role: node.role,
            artifacts_root: node.artifacts.root.clone(),
        });
    }
    let Some(system_runner) = runner.system_runner() else {
        if spec.cni_plugin == CniPlugin::Cilium
            && let Some(assets) = &config.boot_assets_root
            && let Some(node) = nodes
                .iter()
                .find(|node| node.role == NodeRole::ControlPlane)
        {
            assets.prepare_cilium_manifest(runner, node)?;
        }
        for plan in plans {
            start_planned_instance(backend, runner, config, &plan)?;
        }
        return Ok(());
    };
    let runtime = TokioRuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io_error("tokio-runtime-build"))?;
    runtime.block_on(async {
        let mut joins = Vec::with_capacity(plans.len() + 1);
        // Per-cluster RSA/TLS generation is independent of VM boot and must not
        // add to the API-to-CNI critical path. Drain it with the VM tasks.
        if spec.cni_plugin == CniPlugin::Cilium
            && let Some(assets) = &config.boot_assets_root
            && let Some(node) = nodes
                .iter()
                .find(|node| node.role == NodeRole::ControlPlane)
        {
            let assets = assets.clone();
            let node = node.clone();
            joins.push(tokio::task::spawn_blocking(move || {
                assets
                    .prepare_cilium_manifest(&system_runner, &node)
                    .map(|_| ())
            }));
        }
        for plan in plans {
            let config = config.clone();
            joins.push(tokio::task::spawn_blocking(move || {
                start_planned_instance(backend, &system_runner, &config, &plan)
            }));
        }
        let mut first_error = None;
        // Drain every task before returning: cleanup must never race a VM
        // that is still being created by a sibling task.
        for join in joins {
            let result = match join.await {
                Ok(result) => result,
                Err(error) => Err(Error::InvalidArgs(format!(
                    "node start task failed: {error}"
                ))),
            };
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })
}

fn install_addons_and_wait_for_nodes(
    mut request: PostReadyAddonRequest<'_>,
    workers: &[Instance],
    server_mode: ServerMode,
    prepared: Option<&crate::pull::PreparedAssets>,
) -> Result<()> {
    let backend = request.backend;
    let runner = request.runner;
    let control_plane = request.control_plane;
    let guest_kubeconfig = request.guest_kubeconfig;
    let cni = request.cni;
    if cni == CniPlugin::Cilium
        && let Some(assets) = prepared
    {
        assets.install_cilium(backend, runner, control_plane, guest_kubeconfig)?;
        request.cni = CniPlugin::None;
    }
    install_post_ready_addons(request)?;

    // Agents must register before CNI installation, but cannot become Ready
    // until CNI is installed. This applies to Flannel as well as Cilium.
    if cni != CniPlugin::None && server_mode != ServerMode::Agentless {
        for node in std::iter::once(control_plane).chain(workers.iter()) {
            info!(node = %node.name, "waiting for node readiness after CNI bootstrap");
            wait_for_node_ready(
                backend,
                runner,
                &control_plane.name,
                &control_plane.artifacts.root,
                guest_kubeconfig,
                &node.name,
            )?;
            info!(node = %node.name, "node is ready");
        }
    }
    Ok(())
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn down(name: &str, force: bool) -> Result<()> {
    let runner = SystemRunner;
    down_with_runner(name, force, &runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn destroy(name: &str) -> Result<()> {
    let runner = SystemRunner;
    destroy_with_runner(name, &runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn scale(name: &str, workers: usize) -> Result<ScaleOutcome> {
    let runner = SystemRunner;
    scale_with_runner(name, workers, &runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn install_addon(name: &str, addon: &str) -> Result<AddonInstallOutcome> {
    let runner = SystemRunner;
    install_addon_with_runner(name, addon, &runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn down_with_runner(name: &str, force: bool, runner: &dyn CommandRunner) -> Result<()> {
    info!(cluster = %name, force, "starting cluster teardown");
    let state_root = require_cluster_state_root(name)?;
    let _operation = crate::operation::lock(&state_root)?;
    down_at_root(&state_root, name, force, runner)
}

fn down_at_root(
    state_root: &Path,
    name: &str,
    force: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let state = ClusterState::load(state_root)?;
    let backend = backend_for(state.backend);
    stop_instances(backend, runner, &state.nodes, force);
    info!(cluster = %name, force, state_root = %state_root.display(), "cluster stopped");
    Ok(())
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn destroy_with_runner(name: &str, runner: &dyn CommandRunner) -> Result<()> {
    info!(cluster = %name, "starting cluster removal");
    let state_root = require_cluster_state_root(name)?;
    let _operation = crate::operation::lock(&state_root)?;
    destroy_at_root(&state_root, name, runner)
}

fn destroy_at_root(state_root: &Path, name: &str, runner: &dyn CommandRunner) -> Result<()> {
    let state = ClusterState::load(state_root)?;
    let runtime_root = state_root.join("runtime");
    let backend = backend_for(state.backend);
    let base_config = BackendConfig {
        template: state.template.clone(),
        workspace_root: state.workspace_root.clone(),
        enable_rosetta: state.enable_rosetta,
        network: state.network.clone(),
        firecracker_kernel: state.firecracker_kernel.clone(),
        firecracker_rootfs: state.firecracker_rootfs.clone(),
        boot_assets_root: None,
        stream_vm_logs: false,
    };
    let mut failures = Vec::new();
    for node in state.nodes.iter().rev() {
        if let Err(error) = backend.delete_instance(runner, &node.name, &node.artifacts_root) {
            failures.push(format!("{}: {error}", node.name));
        }
    }
    if !failures.is_empty() {
        return Err(Error::InvalidArgs(format!(
            "cluster `{name}` definition retained because node removal failed: {}",
            failures.join("; "),
        )));
    }
    let instance_names = state
        .nodes
        .iter()
        .map(|node| node.name.clone())
        .collect::<Vec<_>>();
    backend.cleanup_cluster_network(runner, &base_config, &instance_names)?;
    fs::remove_dir_all(state_root).map_err(io_error("remove_dir_all"))?;
    if runtime_root.exists() {
        fs::remove_dir_all(&runtime_root).map_err(io_error("remove_dir_all"))?;
    }
    info!(cluster = %name, state_root = %state_root.display(), "cluster removed");
    Ok(())
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn status(name: &str) -> Result<ClusterStatus> {
    let runner = SystemRunner;
    status_with_runner(name, &runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn stats(name: &str) -> Result<ClusterStats> {
    let runner = SystemRunner;
    stats_with_runner(name, &runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn logs(name: &str, node_name: &str) -> Result<NodeLogs> {
    let state_root = require_cluster_state_root(name)?;
    logs_at_root(&state_root, node_name)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn list() -> Result<Vec<ClusterStatus>> {
    let runner = SystemRunner;
    list_with_runner(&runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn defined_cluster_names() -> Result<Vec<String>> {
    defined_cluster_names_at_root(&ClusterSpec::clusters_root_for()?)
}

fn defined_cluster_names_at_root(root: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error("read_dir")(error)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error("read_dir"))?;
        let path = entry.path();
        if path.is_dir() && ClusterState::state_file(&path).is_file() {
            let name = entry.file_name().into_string().map_err(|_| {
                Error::InvalidArgs(format!(
                    "cluster directory is not UTF-8: {}",
                    path.display()
                ))
            })?;
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn logs_at_root(state_root: &Path, node_name: &str) -> Result<NodeLogs> {
    let state = ClusterState::load(state_root)?;
    let node = state
        .nodes
        .iter()
        .find(|node| node.name == node_name)
        .cloned()
        .ok_or_else(|| {
            Error::InvalidArgs(format!(
                "node `{node_name}` is not defined in cluster `{}`",
                state.name
            ))
        })?;
    let sections = collect_node_log_sections(&node)?;

    if sections.is_empty() {
        return Err(Error::InvalidArgs(format!(
            "no local log files found for node `{node_name}`"
        )));
    }

    Ok(NodeLogs {
        cluster: state.name,
        node: node.name,
        sections,
    })
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn scale_with_runner(
    name: &str,
    workers: usize,
    runner: &dyn CommandRunner,
) -> Result<ScaleOutcome> {
    let state_root = require_cluster_state_root(name)?;
    let _operation = crate::operation::lock(&state_root)?;
    scale_at_root(&state_root, name, workers, runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn install_addon_with_runner(
    name: &str,
    addon: &str,
    runner: &dyn CommandRunner,
) -> Result<AddonInstallOutcome> {
    let state_root = require_cluster_state_root(name)?;
    let _operation = crate::operation::lock(&state_root)?;
    install_addon_at_root(&state_root, name, addon, runner)
}

#[allow(clippy::too_many_lines)] // orchestrator; split tracked for a follow-up
fn scale_at_root(
    state_root: &Path,
    _name: &str,
    workers: usize,
    runner: &dyn CommandRunner,
) -> Result<ScaleOutcome> {
    let mut state = ClusterState::load(state_root)?;
    let previous_workers = state.worker_count;
    let scale_span = info_span!(
        "cluster_scale",
        cluster = %state.name,
        backend = state.backend.as_str(),
        current_workers = previous_workers,
        requested_workers = workers
    );
    let _scale_guard = scale_span.enter();
    info!("starting cluster scale");

    if workers < state.worker_count {
        return Err(Error::InvalidArgs(format!(
            "scaling down is not implemented yet; current workers={}, requested workers={workers}",
            state.worker_count
        )));
    }
    if workers == state.worker_count {
        info!("requested worker count already matches cluster state");
        return Ok(ScaleOutcome {
            name: state.name,
            previous_workers: state.worker_count,
            current_workers: state.worker_count,
            added_workers: Vec::new(),
        });
    }

    let runtime_root = state_root.join("runtime");
    let backend = backend_for(state.backend);
    let control_plane_state = state
        .nodes
        .iter()
        .find(|node| node.role == NodeRole::ControlPlane)
        .cloned()
        .ok_or_else(|| {
            Error::InvalidArgs("cluster state is missing a control-plane node".to_owned())
        })?;
    let control_plane_status = backend.query_instance_status(
        runner,
        &control_plane_state.name,
        &control_plane_state.artifacts_root,
    )?;
    info!(
        control_plane = %control_plane_state.name,
        status = %control_plane_status,
        "resolved control-plane status for scale"
    );
    if !instance_status_is_running(&control_plane_status) {
        return Err(Error::InvalidArgs(format!(
            "control-plane `{}` is not running; start the cluster before scaling",
            control_plane_state.name
        )));
    }

    let control_plane = Instance::new(
        control_plane_state.name.clone(),
        NodeRole::ControlPlane,
        control_plane_state.artifacts_root.clone(),
    );
    let control_plane_host =
        backend.control_plane_host(&control_plane.name, &control_plane.artifacts.root)?;
    let control_plane_guest_artifacts =
        backend.guest_artifact_root(&control_plane.name, &control_plane.artifacts.root)?;
    let control_plane_guest_kubeconfig = control_plane_guest_artifacts.join("kubeconfig.yaml");
    let server_token = fs::read_to_string(&state.token_path)
        .map_err(io_error("read_to_string"))?
        .trim()
        .to_owned();
    let server_url = format!("https://{control_plane_host}:6443");
    let k3s_config = K3sBootstrapConfig {
        k3s_version: state.k3s_version.clone(),
        server_mode: state.server_mode,
        component_toggles: state.component_toggles,
    };
    let base_config = BackendConfig {
        template: state.template.clone(),
        workspace_root: state.workspace_root.clone(),
        enable_rosetta: state.enable_rosetta,
        network: state.network.clone(),
        firecracker_kernel: state.firecracker_kernel.clone(),
        firecracker_rootfs: state.firecracker_rootfs.clone(),
        boot_assets_root: if state.backend == BackendKind::Lima {
            let mut asset_spec = cluster_spec_from_state(&state);
            asset_spec.addons.clear();
            match state.cni_plugin {
                CniPlugin::Cilium => Some(crate::pull::load(&asset_spec)?),
                CniPlugin::None => crate::pull::load(&asset_spec).ok(),
                CniPlugin::Flannel => None,
            }
        } else {
            None
        },
        stream_vm_logs: false,
    };

    let new_workers: Vec<Instance> = (state.worker_count..workers)
        .map(|index| {
            let worker_name = format!("kalisto-{}-w{index}", state.name);
            Instance::new(
                worker_name.clone(),
                NodeRole::Worker,
                runtime_root.join(&worker_name),
            )
        })
        .collect();

    info!(
        adding_workers = new_workers.len(),
        worker_names = %new_workers
            .iter()
            .map(|worker| worker.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        "planned worker scale-up"
    );

    let new_worker_names = new_workers
        .iter()
        .map(|worker| worker.name.clone())
        .collect::<Vec<_>>();
    cleanup_scale_target_instances(backend, runner, &new_workers);
    backend.prepare_cluster_network(runner, &base_config, &new_worker_names)?;

    if let Err(error) = start_and_join_workers(
        backend,
        runner,
        WorkerJoinContext {
            base_config: &base_config,
            k3s_config: &k3s_config,
            control_plane: &control_plane,
            control_plane_guest_kubeconfig: &control_plane_guest_kubeconfig,
            worker_cpus: state.worker_resources.cpus,
            worker_memory_gib: state.worker_resources.memory_gib,
            server_url: &server_url,
            server_token: &server_token,
            prestarted_at: None,
        },
        &new_workers,
        &mut state.nodes,
        state.cni_plugin != CniPlugin::None,
    ) {
        cleanup_scale_target_instances(backend, runner, &new_workers);
        return Err(error);
    }

    let added_workers = new_workers
        .iter()
        .map(|worker| worker.name.clone())
        .collect();
    state.worker_count = workers;
    state.save(state_root)?;
    if let Err(error) = handoff_cluster_paths_to_invoking_user(runner, state_root, &runtime_root) {
        warn!(
            error = %error,
            state_root = %state_root.display(),
            runtime_root = %runtime_root.display(),
            "failed to hand off scaled cluster files to invoking user"
        );
    }

    info!(
        previous_workers,
        current_workers = workers,
        added_workers = %new_workers
            .iter()
            .map(|worker| worker.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        "cluster scale completed"
    );

    Ok(ScaleOutcome {
        name: state.name,
        previous_workers,
        current_workers: workers,
        added_workers,
    })
}

fn collect_node_log_sections(node: &NodeState) -> Result<Vec<NodeLogSection>> {
    let candidates = [
        ("k3s stdout", node.artifacts_root.join("k3s.stdout.log")),
        ("k3s stderr", node.artifacts_root.join("k3s.stderr.log")),
        (
            "firecracker serial",
            node.artifacts_root.join("firecracker.stdout.log"),
        ),
        (
            "firecracker stderr",
            node.artifacts_root.join("firecracker.stderr.log"),
        ),
    ];

    let mut sections = Vec::new();
    for (label, path) in candidates {
        if !path.is_file() {
            continue;
        }
        sections.push(NodeLogSection {
            label: label.to_owned(),
            content: fs::read_to_string(&path).map_err(io_error("read_to_string"))?,
            path,
        });
    }
    Ok(sections)
}

fn install_addon_at_root(
    state_root: &Path,
    _name: &str,
    addon: &str,
    runner: &dyn CommandRunner,
) -> Result<AddonInstallOutcome> {
    let addon_id = AddonId::parse(addon)
        .ok_or_else(|| Error::InvalidArgs(format!("unsupported add-on `{addon}`")))?;
    let mut state = ClusterState::load(state_root)?;
    let mut metallb_config = state.metallb.clone();
    if addon_id == AddonId::LbMetalLb {
        if let Some(config) = load_project_config(&state.workspace_root)? {
            let mut configured = ClusterSpec::new(state.name.clone(), state.workspace_root.clone());
            config.apply_to_cluster_spec(&mut configured)?;
            if !configured.metallb.is_empty() {
                metallb_config = configured.metallb;
            }
        }
        metallb_config.validate()?;
    }
    validate_addon_install_allowed(&state, addon_id)?;
    let runtime_root = state_root.join("runtime");
    let backend = backend_for(state.backend);
    let control_plane_state = state
        .nodes
        .iter()
        .find(|node| node.role == NodeRole::ControlPlane)
        .cloned()
        .ok_or_else(|| {
            Error::InvalidArgs("cluster state is missing a control-plane node".to_owned())
        })?;
    let control_plane_status = backend.query_instance_status(
        runner,
        &control_plane_state.name,
        &control_plane_state.artifacts_root,
    )?;
    if !instance_status_is_running(&control_plane_status) {
        return Err(Error::InvalidArgs(format!(
            "control-plane `{}` is not running; start the cluster before installing add-ons",
            control_plane_state.name
        )));
    }

    let control_plane = Instance::new(
        control_plane_state.name.clone(),
        NodeRole::ControlPlane,
        control_plane_state.artifacts_root.clone(),
    );
    let control_plane_guest_artifacts =
        backend.guest_artifact_root(&control_plane.name, &control_plane.artifacts.root)?;
    let control_plane_guest_kubeconfig = control_plane_guest_artifacts.join("kubeconfig.yaml");

    if addon_id == AddonId::CniFlannel
        && state.cni_plugin == CniPlugin::Flannel
        && !state.component_toggles.flannel_backend_none
    {
        return Ok(AddonInstallOutcome {
            cluster: state.name,
            addon: addon_id.as_str().to_owned(),
        });
    }

    info!(
        cluster = %state.name,
        addon = addon_id.as_str(),
        "installing cluster add-on"
    );
    let mut desired_state_changed = record_desired_addon_state(&mut state, addon_id);
    if addon_id == AddonId::LbMetalLb && state.metallb != metallb_config {
        state.metallb = metallb_config.clone();
        desired_state_changed = true;
    }

    install_catalog_addon_with_cilium_version(
        backend,
        runner,
        &control_plane,
        &control_plane_guest_kubeconfig,
        addon_id.as_str(),
        &state.cilium_version,
        if addon_id == AddonId::LbMetalLb {
            Some(&metallb_config)
        } else {
            None
        },
    )?;

    if desired_state_changed {
        state.save(state_root)?;
        if let Err(error) =
            handoff_cluster_paths_to_invoking_user(runner, state_root, &runtime_root)
        {
            warn!(
                error = %error,
                state_root = %state_root.display(),
                runtime_root = %runtime_root.display(),
                "failed to hand off cluster files to invoking user"
            );
        }
    }

    Ok(AddonInstallOutcome {
        cluster: state.name,
        addon: addon_id.as_str().to_owned(),
    })
}

fn validate_addon_install_allowed(state: &ClusterState, addon_id: AddonId) -> Result<()> {
    match (state.cni_plugin, addon_id) {
        (CniPlugin::Cilium, AddonId::CniFlannel) | (CniPlugin::Flannel, AddonId::CniCilium) => {
            Err(Error::InvalidArgs(format!(
                "cannot install `{}` on cluster `{}` because it is already configured with CNI `{}`; CNI add-ons are mutually exclusive",
                addon_id.as_str(),
                state.name,
                state.cni_plugin.as_str(),
            )))
        }
        _ => Ok(()),
    }
}

fn record_desired_addon_state(state: &mut ClusterState, addon_id: AddonId) -> bool {
    match addon_id {
        AddonId::CniFlannel => {
            let mut changed = false;
            if state.cni_plugin != CniPlugin::Flannel {
                state.cni_plugin = CniPlugin::Flannel;
                changed = true;
            }

            let mut desired_toggles = state.component_toggles;
            CniPlugin::Flannel.apply_component_defaults(&mut desired_toggles);
            if state.component_toggles != desired_toggles {
                state.component_toggles = desired_toggles;
                changed = true;
            }
            changed
        }
        AddonId::CniCilium => {
            let mut changed = false;
            if state.cni_plugin != CniPlugin::Cilium {
                state.cni_plugin = CniPlugin::Cilium;
                changed = true;
            }

            let mut desired_toggles = state.component_toggles;
            CniPlugin::Cilium.apply_component_defaults(&mut desired_toggles);
            if state.component_toggles != desired_toggles {
                state.component_toggles = desired_toggles;
                changed = true;
            }
            changed
        }
        AddonId::LbServiceLb => {
            if state.component_toggles.disable_servicelb {
                state.component_toggles.disable_servicelb = false;
                true
            } else {
                false
            }
        }
        _ => {
            if state
                .addons
                .iter()
                .any(|installed| installed == addon_id.as_str())
            {
                false
            } else {
                state.addons.push(addon_id.as_str().to_owned());
                true
            }
        }
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
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
    let backend = backend_for(state.backend);
    let mut nodes = Vec::with_capacity(state.nodes.len());
    for node in &state.nodes {
        let backend_status =
            backend.query_instance_status(runner, &node.name, &node.artifacts_root)?;
        nodes.push(NodeStatus {
            name: node.name.clone(),
            role: node.role,
            backend_status,
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

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn list_with_runner(runner: &dyn CommandRunner) -> Result<Vec<ClusterStatus>> {
    let clusters_root = ClusterSpec::clusters_root_for()?;
    list_at_root(&clusters_root, runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn stats_with_runner(name: &str, runner: &dyn CommandRunner) -> Result<ClusterStats> {
    let state_root = require_cluster_state_root(name)?;
    let state = ClusterState::load(&state_root)?;
    let backend = backend_for(state.backend);
    let mut nodes = Vec::with_capacity(state.nodes.len());
    for node in &state.nodes {
        let backend_status =
            backend.query_instance_status(runner, &node.name, &node.artifacts_root)?;
        let (configured_cpus, configured_memory_gib) = match node.role {
            NodeRole::ControlPlane => (
                state.control_plane_resources.cpus,
                state.control_plane_resources.memory_gib,
            ),
            NodeRole::Worker => (
                state.worker_resources.cpus,
                state.worker_resources.memory_gib,
            ),
        };
        let host_metrics =
            load_host_node_metrics(runner, state.backend, &node.name, &node.artifacts_root);
        nodes.push(NodeStats {
            name: node.name.clone(),
            role: node.role,
            backend_status,
            configured_cpus,
            configured_memory_gib,
            pid: host_metrics.pid,
            guest_ip: host_metrics.guest_ip,
            rss_bytes: host_metrics.rss_bytes,
            vsz_bytes: host_metrics.vsz_bytes,
            rootfs_bytes: host_metrics.rootfs_bytes,
            rx_bytes: host_metrics.rx_bytes,
            tx_bytes: host_metrics.tx_bytes,
        });
    }

    Ok(ClusterStats {
        name: state.name,
        state_root,
        kubeconfig_path: state.kubeconfig_path,
        nodes,
    })
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
        let backend = backend_for(state.backend);
        let mut nodes = Vec::with_capacity(state.nodes.len());
        for node in &state.nodes {
            let backend_status =
                backend.query_instance_status(runner, &node.name, &node.artifacts_root)?;
            nodes.push(NodeStatus {
                name: node.name.clone(),
                role: node.role,
                backend_status,
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

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn kubeconfig_path(name: &str) -> Result<std::path::PathBuf> {
    let state_root = require_cluster_state_root(name)?;
    let kubeconfig_path = ClusterState::load(state_root.as_path())?.kubeconfig_path;
    debug!(
        cluster = %name,
        kubeconfig = %kubeconfig_path.display(),
        "resolved kubeconfig path"
    );
    Ok(kubeconfig_path)
}

#[derive(Debug, Default)]
struct HostNodeMetrics {
    pid: Option<u32>,
    guest_ip: Option<String>,
    rss_bytes: Option<u64>,
    vsz_bytes: Option<u64>,
    rootfs_bytes: Option<u64>,
    rx_bytes: Option<u64>,
    tx_bytes: Option<u64>,
}

fn load_host_node_metrics(
    runner: &dyn CommandRunner,
    backend: BackendKind,
    instance_name: &str,
    artifacts_root: &Path,
) -> HostNodeMetrics {
    match backend {
        BackendKind::Firecracker => load_firecracker_host_metrics(runner, artifacts_root),
        BackendKind::Lima => load_lima_host_metrics(runner, instance_name),
    }
}

fn load_lima_host_metrics(runner: &dyn CommandRunner, instance_name: &str) -> HostNodeMetrics {
    let mut metrics = query_lima_instance_metrics(runner, instance_name).unwrap_or_default();
    if let Some(pid) = metrics.pid
        && let Ok((rss_bytes, vsz_bytes)) = read_process_memory_bytes(runner, pid)
    {
        metrics.rss_bytes = rss_bytes;
        let _ = vsz_bytes;
        metrics.vsz_bytes = None;
    }
    if let Ok((guest_ip, rx_bytes, tx_bytes)) =
        query_lima_guest_network_metrics(runner, instance_name)
    {
        metrics.guest_ip = guest_ip.or(metrics.guest_ip);
        metrics.rx_bytes = rx_bytes;
        metrics.tx_bytes = tx_bytes;
    }
    metrics
}

fn query_lima_instance_metrics(
    runner: &dyn CommandRunner,
    instance_name: &str,
) -> Result<HostNodeMetrics> {
    let output = runner.run(
        "limactl",
        &[
            "list".to_owned(),
            "--format".to_owned(),
            "{{ .Name }}\t{{ .Dir }}\t{{ .HostAgentPID }}\t{{ .DriverPID }}\t{{ .Disk }}"
                .to_owned(),
            instance_name.to_owned(),
        ],
    )?;
    parse_lima_instance_metrics(&output.stdout, instance_name)
}

fn parse_lima_instance_metrics(output: &str, instance_name: &str) -> Result<HostNodeMetrics> {
    let line = output
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| Error::InvalidArgs(format!("no Lima stats found for `{instance_name}`")))?;
    let parts = line.split('\t').collect::<Vec<_>>();
    if parts.len() < 5 {
        return Err(Error::InvalidArgs(format!(
            "unexpected Lima stats output for `{instance_name}`: {line}"
        )));
    }

    let host_agent_pid = parse_optional_u32(parts[2]);
    let driver_pid = parse_optional_u32(parts[3]);
    Ok(HostNodeMetrics {
        pid: host_agent_pid.or(driver_pid),
        guest_ip: None,
        rss_bytes: None,
        vsz_bytes: None,
        rootfs_bytes: parse_optional_u64(parts[4]),
        rx_bytes: None,
        tx_bytes: None,
    })
}

fn query_lima_guest_network_metrics(
    runner: &dyn CommandRunner,
    instance_name: &str,
) -> Result<(Option<String>, Option<u64>, Option<u64>)> {
    let output = runner.run(
        "limactl",
        &[
            "-y".to_owned(),
            "shell".to_owned(),
            instance_name.to_owned(),
            "sh".to_owned(),
            "-lc".to_owned(),
            concat!(
                "iface=$(ip route show default 2>/dev/null | awk 'NR==1 {print $5}'); ",
                "ip=$(ip -o -4 addr show scope global 2>/dev/null | awk 'NR==1 {sub(/\\/.*/, \"\", $4); print $4}'); ",
                "printf '%s\\n' \"$ip\"; ",
                "if [ -n \"$iface\" ] && [ -r \"/sys/class/net/$iface/statistics/rx_bytes\" ]; then cat \"/sys/class/net/$iface/statistics/rx_bytes\"; else echo; fi; ",
                "if [ -n \"$iface\" ] && [ -r \"/sys/class/net/$iface/statistics/tx_bytes\" ]; then cat \"/sys/class/net/$iface/statistics/tx_bytes\"; else echo; fi"
            )
            .to_owned(),
        ],
    )?;
    Ok(parse_lima_guest_network_metrics(&output.stdout))
}

fn parse_lima_guest_network_metrics(output: &str) -> (Option<String>, Option<u64>, Option<u64>) {
    let mut lines = output.lines();
    let guest_ip = lines.next().and_then(non_empty_string);
    let rx_bytes = lines.next().and_then(parse_optional_u64);
    let tx_bytes = lines.next().and_then(parse_optional_u64);
    (guest_ip, rx_bytes, tx_bytes)
}

fn load_firecracker_host_metrics(
    runner: &dyn CommandRunner,
    artifacts_root: &Path,
) -> HostNodeMetrics {
    let pid = read_optional_u32(&artifacts_root.join("firecracker.pid"));
    let guest_ip = read_optional_trimmed(&artifacts_root.join("guest-ip"));
    let tap_name = read_optional_trimmed(&artifacts_root.join("tap-name"));
    let rootfs_bytes = fs::metadata(artifacts_root.join("rootfs.raw"))
        .ok()
        .map(|metadata| metadata.len());
    let (rss_bytes, vsz_bytes) = pid
        .and_then(|pid| read_process_memory_bytes(runner, pid).ok())
        .unwrap_or((None, None));
    let (rx_bytes, tx_bytes) = tap_name.as_deref().map_or((None, None), read_tap_bytes);

    HostNodeMetrics {
        pid,
        guest_ip,
        rss_bytes,
        vsz_bytes,
        rootfs_bytes,
        rx_bytes,
        tx_bytes,
    }
}

fn read_process_memory_bytes(
    runner: &dyn CommandRunner,
    pid: u32,
) -> Result<(Option<u64>, Option<u64>)> {
    let output = runner.run(
        "ps",
        &[
            "-o".to_owned(),
            "rss=,vsz=".to_owned(),
            "-p".to_owned(),
            pid.to_string(),
        ],
    )?;
    let Some(line) = output.stdout.lines().find(|line| !line.trim().is_empty()) else {
        return Ok((None, None));
    };
    let mut parts = line.split_whitespace();
    let rss_bytes = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value * 1024);
    let vsz_bytes = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value * 1024);
    Ok((rss_bytes, vsz_bytes))
}

fn read_tap_bytes(tap_name: &str) -> (Option<u64>, Option<u64>) {
    let statistics_root = Path::new("/sys/class/net")
        .join(tap_name)
        .join("statistics");
    let rx_bytes = read_optional_u64(&statistics_root.join("rx_bytes"));
    let tx_bytes = read_optional_u64(&statistics_root.join("tx_bytes"));
    (rx_bytes, tx_bytes)
}

fn read_optional_trimmed(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn parse_optional_u32(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" || trimmed == "0" {
        None
    } else {
        trimmed.parse::<u32>().ok()
    }
}

fn parse_optional_u64(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        None
    } else {
        trimmed.parse::<u64>().ok()
    }
}

fn non_empty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn read_optional_u32(path: &Path) -> Option<u32> {
    read_optional_trimmed(path)?.parse::<u32>().ok()
}

fn read_optional_u64(path: &Path) -> Option<u64> {
    read_optional_trimmed(path)?.parse::<u64>().ok()
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn benchmark(spec: &ClusterSpec, runs: usize) -> Result<Vec<BenchmarkSample>> {
    let runner = SystemRunner;
    benchmark_with_runner(spec, runs, &runner)
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
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
        run_spec.name.clone_from(&run_name);
        info!(
            benchmark_run = run + 1,
            benchmark_total = runs,
            cluster = %run_name,
            "starting benchmark run"
        );

        create_with_runner(&run_spec, runner)?;
        let outcome = up_with_runner(&run_spec, runner)?;
        samples.push(BenchmarkSample {
            run: run + 1,
            cluster_name: run_name.clone(),
            control_plane_ready: outcome.control_plane_ready,
            cluster_ready: outcome.cluster_ready,
        });
        down_with_runner(&run_name, false, runner)?;
        destroy_with_runner(&run_name, runner)?;
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

fn cleanup_instances(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    nodes: &[NodeState],
) {
    delete_instances(backend, runner, nodes);
}

fn cleanup_scale_target_instances(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    workers: &[Instance],
) {
    let nodes = workers
        .iter()
        .map(|worker| NodeState {
            name: worker.name.clone(),
            role: worker.role,
            artifacts_root: worker.artifacts.root.clone(),
        })
        .collect::<Vec<_>>();
    cleanup_instances(backend, runner, &nodes);
}

fn stop_instances(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    nodes: &[NodeState],
    force: bool,
) {
    for node in nodes.iter().rev() {
        info!(
            backend = backend.kind().as_str(),
            instance = %node.name,
            force,
            "stopping backend instance"
        );
        let _ = backend.stop_instance(runner, &node.name, &node.artifacts_root, force);
    }
}

fn delete_instances(backend: &dyn ClusterBackend, runner: &dyn CommandRunner, nodes: &[NodeState]) {
    for node in nodes.iter().rev() {
        info!(
            backend = backend.kind().as_str(),
            instance = %node.name,
            "cleaning up backend instance"
        );
        let _ = backend.delete_instance(runner, &node.name, &node.artifacts_root);
    }
}

fn start_and_join_workers(
    backend: &'static dyn ClusterBackend,
    runner: &dyn CommandRunner,
    context: WorkerJoinContext<'_>,
    worker_instances: &[Instance],
    instances: &mut Vec<NodeState>,
    wait_for_ready: bool,
) -> Result<()> {
    if let Some(system_runner) = runner.system_runner() {
        return start_and_join_workers_parallel(
            backend,
            system_runner,
            context,
            worker_instances,
            instances,
            wait_for_ready,
        );
    }

    let started_workers = if let Some(boot_started) = context.prestarted_at {
        worker_instances
            .iter()
            .cloned()
            .map(|node| StartedWorker { node, boot_started })
            .collect()
    } else {
        start_worker_instances(
            backend,
            runner,
            context.base_config,
            worker_instances,
            context.worker_cpus,
            context.worker_memory_gib,
            instances,
        )
        .map_err(|error| Error::InvalidArgs(error.detail))?
    };

    for started_worker in started_workers {
        let worker = started_worker.node;
        info!(node = %worker.name, "bootstrapping worker");
        bootstrap_worker(
            backend,
            runner,
            context.k3s_config,
            &worker,
            JoinConfig {
                server_url: context.server_url,
                token: context.server_token,
            },
        )?;
        info!(node = %worker.name, "waiting for worker process");
        wait_for_worker_process(backend, runner, &worker.name, &worker.artifacts.root)?;
        if wait_for_ready {
            info!(node = %worker.name, "waiting for worker readiness");
            wait_for_node_ready(
                backend,
                runner,
                &context.control_plane.name,
                &context.control_plane.artifacts.root,
                context.control_plane_guest_kubeconfig,
                &worker.name,
            )?;
            info!(
                node = %worker.name,
                ready_after = %format_duration(started_worker.boot_started.elapsed()),
                "worker is ready"
            );
        }
    }

    Ok(())
}

#[allow(clippy::too_many_lines)] // orchestrator; split tracked for a follow-up
fn start_and_join_workers_parallel(
    backend: &'static dyn ClusterBackend,
    runner: SystemRunner,
    context: WorkerJoinContext<'_>,
    worker_instances: &[Instance],
    instances: &mut Vec<NodeState>,
    wait_for_ready: bool,
) -> Result<()> {
    let runtime = TokioRuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io_error("tokio-runtime-build"))?;

    let base_config = context.base_config.clone();
    let worker_cpus = context.worker_cpus;
    let worker_memory_gib = context.worker_memory_gib;
    let workers = worker_instances.to_vec();

    let mut started_workers = if let Some(boot_started) = context.prestarted_at {
        workers
            .into_iter()
            .map(|node| StartedWorker { node, boot_started })
            .collect()
    } else {
        runtime.block_on(async move {
            let mut joins = Vec::with_capacity(workers.len());
            for worker in workers {
                let base_config = base_config.clone();
                joins.push(tokio::task::spawn_blocking(
                    move || -> Result<StartedWorker> {
                        fs::create_dir_all(&worker.artifacts.root)
                            .map_err(io_error("create_dir_all"))?;
                        let boot_started = Instant::now();
                        info!(
                            node = %worker.name,
                            cpus = worker_cpus,
                            memory_gib = worker_memory_gib,
                            "starting worker VM"
                        );
                        start_node_instance(
                            backend,
                            &runner,
                            &base_config,
                            &worker,
                            None,
                            worker_cpus,
                            worker_memory_gib,
                        )
                        .map_err(|error| Error::InvalidArgs(format!("{}: {error}", worker.name)))?;
                        info!(node = %worker.name, "worker VM started");
                        Ok(StartedWorker {
                            node: worker,
                            boot_started,
                        })
                    },
                ));
            }

            let mut started = Vec::new();
            for join in joins {
                let started_worker = join.await.map_err(|error| {
                    Error::InvalidArgs(format!("worker start task failed: {error}"))
                })??;
                started.push(started_worker);
            }
            Ok::<Vec<StartedWorker>, Error>(started)
        })?
    };

    if context.prestarted_at.is_none() {
        for started_worker in &started_workers {
            instances.push(NodeState {
                name: started_worker.node.name.clone(),
                role: started_worker.node.role,
                artifacts_root: started_worker.node.artifacts.root.clone(),
            });
        }
    }

    let k3s_config = context.k3s_config.clone();
    let control_plane = context.control_plane.clone();
    let control_plane_guest_kubeconfig = context.control_plane_guest_kubeconfig.to_path_buf();
    let server_url = context.server_url.to_owned();
    let server_token = context.server_token.to_owned();

    runtime.block_on(async move {
        let mut joins = Vec::with_capacity(started_workers.len());
        for started_worker in started_workers.drain(..) {
            let k3s_config = k3s_config.clone();
            let control_plane = control_plane.clone();
            let control_plane_guest_kubeconfig = control_plane_guest_kubeconfig.clone();
            let server_url = server_url.clone();
            let server_token = server_token.clone();
            joins.push(tokio::task::spawn_blocking(move || -> Result<()> {
                let worker = started_worker.node;
                info!(node = %worker.name, "bootstrapping worker");
                bootstrap_worker(
                    backend,
                    &runner,
                    &k3s_config,
                    &worker,
                    JoinConfig {
                        server_url: &server_url,
                        token: &server_token,
                    },
                )?;
                info!(node = %worker.name, "waiting for worker process");
                wait_for_worker_process(backend, &runner, &worker.name, &worker.artifacts.root)?;
                if wait_for_ready {
                    info!(node = %worker.name, "waiting for worker readiness");
                    wait_for_node_ready(
                        backend,
                        &runner,
                        &control_plane.name,
                        &control_plane.artifacts.root,
                        &control_plane_guest_kubeconfig,
                        &worker.name,
                    )?;
                    info!(
                        node = %worker.name,
                        ready_after = %format_duration(started_worker.boot_started.elapsed()),
                        "worker is ready"
                    );
                }
                Ok(())
            }));
        }

        for join in joins {
            join.await.map_err(|error| {
                Error::InvalidArgs(format!("worker bootstrap task failed: {error}"))
            })??;
        }
        Ok::<(), Error>(())
    })
}

pub(crate) fn instance_status_is_running(status: &str) -> bool {
    status.split_whitespace().any(|token| token == "Running")
}

fn cluster_has_running_nodes(runner: &dyn CommandRunner, state: &ClusterState) -> Result<bool> {
    let backend = backend_for(state.backend);
    for node in &state.nodes {
        let status = backend.query_instance_status(runner, &node.name, &node.artifacts_root)?;
        if instance_status_is_running(&status) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn cluster_spec_from_state(state: &ClusterState) -> ClusterSpec {
    ClusterSpec {
        name: state.name.clone(),
        backend: state.backend,
        workspace_root: state.workspace_root.clone(),
        template: state.template.clone(),
        image_arch: None,
        firecracker_kernel: state.firecracker_kernel.clone(),
        firecracker_rootfs: state.firecracker_rootfs.clone(),
        api_port: state.api_port,
        worker_count: state.worker_count,
        requested_worker_count: None,
        control_plane_resources: state.control_plane_resources,
        worker_resources: state.worker_resources,
        enable_rosetta: state.enable_rosetta,
        network: state.network.clone(),
        k3s_version: state.k3s_version.clone(),
        cni_plugin: state.cni_plugin,
        cilium_version: state.cilium_version.clone(),
        addons: state.addons.clone(),
        metallb: state.metallb.clone(),
        server_mode: state.server_mode,
        component_toggles: state.component_toggles,
        keep_failed_runtime: false,
        stream_vm_logs: false,
    }
}

fn resolve_up_spec(state: &ClusterState, requested: &ClusterSpec) -> ClusterSpec {
    let mut resolved_spec = cluster_spec_from_state(state);
    resolved_spec.image_arch = requested.image_arch;
    if let Some(workers) = requested.requested_worker_count {
        resolved_spec.worker_count = workers;
        resolved_spec.requested_worker_count = Some(workers);
    }
    resolved_spec.keep_failed_runtime = requested.keep_failed_runtime;
    resolved_spec.stream_vm_logs = requested.stream_vm_logs;
    resolved_spec
}

fn cluster_state_from_spec(
    spec: &ClusterSpec,
    workspace_root: std::path::PathBuf,
    state_root: &Path,
) -> ClusterState {
    let runtime_root = state_root.join("runtime");
    let control_plane_name = spec.control_plane_name();
    let mut nodes = Vec::with_capacity(spec.worker_count + 1);
    nodes.push(NodeState {
        name: control_plane_name,
        role: NodeRole::ControlPlane,
        artifacts_root: runtime_root.join("control-plane"),
    });
    for index in 0..spec.worker_count {
        let name = format!("kalisto-{}-w{index}", spec.name);
        nodes.push(NodeState {
            name: name.clone(),
            role: NodeRole::Worker,
            artifacts_root: runtime_root.join(&name),
        });
    }

    ClusterState {
        name: spec.name.clone(),
        backend: spec.backend,
        workspace_root,
        template: spec.template.clone(),
        firecracker_kernel: spec.firecracker_kernel.clone(),
        firecracker_rootfs: spec.firecracker_rootfs.clone(),
        api_port: spec.api_port,
        worker_count: spec.worker_count,
        control_plane_resources: spec.control_plane_resources,
        worker_resources: spec.worker_resources,
        enable_rosetta: spec.enable_rosetta,
        network: spec.network.clone(),
        k3s_version: spec.k3s_version.clone(),
        cni_plugin: spec.cni_plugin,
        cilium_version: spec.cilium_version.clone(),
        addons: spec.addons.clone(),
        metallb: spec.metallb.clone(),
        server_mode: spec.server_mode,
        component_toggles: spec.component_toggles,
        kubeconfig_path: state_root.join("kubeconfig.yaml"),
        token_path: state_root.join("node-token"),
        nodes,
    }
}

fn rollback_failed_up(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    started_instances: &[NodeState],
    state_root: &Path,
    runtime_root: &Path,
    keep_failed_runtime: bool,
    error: Error,
) -> Error {
    if let Err(chown_error) =
        handoff_cluster_paths_to_invoking_user(runner, state_root, runtime_root)
    {
        warn!(
            error = %chown_error,
            state_root = %state_root.display(),
            runtime_root = %runtime_root.display(),
            "failed to hand off preserved failed cluster files to invoking user"
        );
    }
    if keep_failed_runtime {
        warn!(
            state_root = %state_root.display(),
            runtime_root = %runtime_root.display(),
            error = %error,
            "cluster bring-up failed; preserving runtime artifacts"
        );
    } else {
        warn!(
            state_root = %state_root.display(),
            runtime_root = %runtime_root.display(),
            error = %error,
            "cluster bring-up failed; preserving cluster definition"
        );
    }
    // A retry may be resuming existing VMs with user data. Failed bring-up
    // must never delete their disks; only an explicit destroy may do that.
    if !keep_failed_runtime {
        stop_instances(backend, runner, started_instances, true);
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

fn handoff_cluster_paths_to_invoking_user(
    runner: &dyn CommandRunner,
    state_root: &Path,
    runtime_root: &Path,
) -> Result<()> {
    let Some((uid, gid)) = sudo_invoking_user_ids() else {
        return Ok(());
    };

    for path in [state_root, runtime_root] {
        if !path.exists() {
            continue;
        }
        runner.run(
            "chown",
            &[
                "-R".to_owned(),
                format!("{uid}:{gid}"),
                path.display().to_string(),
            ],
        )?;
    }

    Ok(())
}

fn sudo_invoking_user_ids() -> Option<(u32, u32)> {
    let uid = std::env::var("SUDO_UID").ok()?.parse::<u32>().ok()?;
    let gid = std::env::var("SUDO_GID").ok()?.parse::<u32>().ok()?;
    Some((uid, gid))
}

fn sync_control_plane_artifact(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    guest_path: &Path,
    host_path: &Path,
    timeout: Duration,
    tail: &mut LiveLogTail,
) -> Result<()> {
    wait_for_guest_text_file_copy(
        backend,
        runner,
        GuestFileCopy {
            instance_name: &control_plane.name,
            runtime_root: &control_plane.artifacts.root,
            guest_path,
            host_path,
            timeout,
        },
        tail,
    )
}

fn persist_kubeconfig(source: &Path, destination: &Path, api_endpoint: &str) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(io_error("create_dir_all"))?;
    }

    let kubeconfig = fs::read_to_string(source).map_err(io_error("read_to_string"))?;
    let rewritten = rewrite_kubeconfig_server(&kubeconfig, api_endpoint)?;
    fs::write(destination, rewritten).map_err(io_error("write"))?;
    info!(
        artifact = "kubeconfig",
        source = %source.display(),
        destination = %destination.display(),
        api_endpoint,
        "copied cluster artifact into persisted state"
    );
    Ok(())
}

fn rewrite_kubeconfig_server(kubeconfig: &str, api_endpoint: &str) -> Result<String> {
    let mut found_server = false;
    let rewritten = kubeconfig
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("server: ") {
                found_server = true;
                format!("    server: https://{api_endpoint}")
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

fn tls_sans_for_control_plane(control_plane_host: &str, api_endpoint: &str) -> Vec<String> {
    let mut tls_sans = vec!["127.0.0.1".to_owned()];
    let api_host = endpoint_host(api_endpoint);
    if control_plane_host != "127.0.0.1" {
        tls_sans.push(control_plane_host.to_owned());
    }
    if api_host != "127.0.0.1" && api_host != control_plane_host {
        tls_sans.push(api_host);
    }
    tls_sans
}

fn endpoint_host(api_endpoint: &str) -> String {
    api_endpoint
        .rsplit_once(':')
        .map_or_else(|| api_endpoint.to_owned(), |(host, _)| host.to_owned())
}

pub(crate) fn require_cluster_state_root(name: &str) -> Result<std::path::PathBuf> {
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

fn debug_feedback(message: impl AsRef<str>) {
    if raw_streams_enabled() {
        eprintln!("[debug] {}", message.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;

    use crate::backend::BackendKind;
    use crate::cluster::addon::{
        AddonId, CniPlugin, DEFAULT_CILIUM_VERSION, MetalLbConfig, PostReadyAddonRequest,
    };
    use crate::cluster::node::Instance;
    use crate::cluster::spec::{ClusterSpec, NodeResources};
    use crate::cluster::state::{ClusterState, NodeState};
    use crate::error::Error;
    use crate::k3s::{ComponentToggles, NodeRole, ServerMode};
    use crate::process::{CommandRunner, Output};
    use crate::test_support::env_lock;

    use super::{
        cleanup_scale_target_instances, down_at_root, install_addon_at_root, list_at_root,
        logs_at_root, parse_lima_guest_network_metrics, parse_lima_instance_metrics,
        record_desired_addon_state, resolve_up_spec, scale_at_root, validate_addon_install_allowed,
    };

    #[derive(Default)]
    struct FakeRunner {
        statuses: HashMap<String, String>,
        fail_limactl_version: bool,
        fail_on_arg_substring: Option<String>,
        invocations: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeRunner {
        fn with_statuses(statuses: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
            Self {
                statuses: statuses
                    .into_iter()
                    .map(|(name, status)| (name.to_owned(), status.to_owned()))
                    .collect(),
                fail_limactl_version: false,
                fail_on_arg_substring: None,
                invocations: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_limactl_version_failure() -> Self {
            Self {
                statuses: HashMap::new(),
                fail_limactl_version: true,
                fail_on_arg_substring: None,
                invocations: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_invocation_failure_containing(
            statuses: impl IntoIterator<Item = (&'static str, &'static str)>,
            needle: &str,
        ) -> Self {
            Self {
                statuses: statuses
                    .into_iter()
                    .map(|(name, status)| (name.to_owned(), status.to_owned()))
                    .collect(),
                fail_limactl_version: false,
                fail_on_arg_substring: Some(needle.to_owned()),
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
            if self.fail_limactl_version
                && program == "limactl"
                && args == [String::from("--version")]
            {
                return Err(Error::CommandFailed {
                    program: program.to_owned(),
                    args: args.to_vec(),
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "limactl: must not run as the root user".to_owned(),
                });
            }
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
            if let Some(needle) = &self.fail_on_arg_substring
                && program == "limactl"
                && args.iter().any(|arg| arg.contains(needle))
            {
                return Err(Error::CommandFailed {
                    program: program.to_owned(),
                    args: args.to_vec(),
                    code: Some(1),
                    stdout: String::new(),
                    stderr: format!("simulated invocation failure for `{needle}`"),
                });
            }

            Ok(Output {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn cluster_start_tracks_all_attempts_for_rollback() {
        let _guard = env_lock();
        let root =
            std::env::temp_dir().join(format!("kalisto-start-attempts-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let spec = ClusterSpec::new("attempts".to_owned(), root.clone());
        let config = crate::backend::BackendConfig {
            template: "template://default".to_owned(),
            workspace_root: root.clone(),
            enable_rosetta: false,
            network: "lima:user-v2".to_owned(),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            boot_assets_root: None,
            stream_vm_logs: false,
        };
        let nodes = [
            Instance::new("cp".to_owned(), NodeRole::ControlPlane, root.join("cp")),
            Instance::new("w0".to_owned(), NodeRole::Worker, root.join("w0")),
            Instance::new("w1".to_owned(), NodeRole::Worker, root.join("w1")),
        ];
        let runner = FakeRunner::with_invocation_failure_containing([], "start");
        let mut attempts = Vec::new();
        let result = super::start_cluster_instances(
            &crate::backend::lima::BACKEND,
            &runner,
            &config,
            &spec,
            &nodes,
            &mut attempts,
        );
        assert!(result.is_err());
        assert_eq!(attempts.len(), 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prestarted_workers_join_without_starting_vms_again() {
        let runner = FakeRunner::default();
        let control_plane = Instance::new(
            "cp".to_owned(),
            NodeRole::ControlPlane,
            "/runtime/cp".into(),
        );
        let worker = Instance::new("w0".to_owned(), NodeRole::Worker, "/runtime/w0".into());
        let config = crate::backend::BackendConfig {
            template: "template://default".to_owned(),
            workspace_root: "/runtime".into(),
            enable_rosetta: false,
            network: "lima:user-v2".to_owned(),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            boot_assets_root: None,
            stream_vm_logs: false,
        };
        let k3s_config = crate::cluster::node::K3sBootstrapConfig {
            k3s_version: "v1.33.5+k3s1".to_owned(),
            server_mode: ServerMode::Standard,
            component_toggles: ComponentToggles::default(),
        };
        super::start_and_join_workers(
            &crate::backend::lima::BACKEND,
            &runner,
            super::WorkerJoinContext {
                base_config: &config,
                k3s_config: &k3s_config,
                control_plane: &control_plane,
                control_plane_guest_kubeconfig: Path::new("/runtime/cp/kubeconfig.yaml"),
                worker_cpus: 2,
                worker_memory_gib: 3,
                server_url: "https://cp.example.test:6443",
                server_token: "test-token",
                prestarted_at: Some(std::time::Instant::now()),
            },
            &[worker],
            &mut Vec::new(),
            false,
        )
        .unwrap();
        let invocations = runner.invocations.lock().unwrap();
        assert!(
            invocations
                .iter()
                .all(|(_, args)| args.iter().all(|arg| arg != "start"))
        );
        assert!(
            invocations
                .iter()
                .any(|(_, args)| args.iter().any(|arg| arg.contains("k3s.pid")))
        );
    }

    #[test]
    fn cni_is_installed_before_all_three_nodes_are_checked_for_readiness() {
        for cni in [CniPlugin::Flannel, CniPlugin::Cilium] {
            let runner = FakeRunner::default();
            let control_plane = Instance::new(
                "cp".to_owned(),
                NodeRole::ControlPlane,
                "/runtime/cp".into(),
            );
            let workers = ["w0", "w1"].map(|name| {
                Instance::new(
                    name.to_owned(),
                    NodeRole::Worker,
                    format!("/runtime/{name}").into(),
                )
            });
            super::install_addons_and_wait_for_nodes(
                PostReadyAddonRequest {
                    backend: &crate::backend::lima::BACKEND,
                    runner: &runner,
                    control_plane: &control_plane,
                    guest_kubeconfig: Path::new("/runtime/cp/kubeconfig.yaml"),
                    cni,
                    builtin_flannel: false,
                    cilium_version: DEFAULT_CILIUM_VERSION,
                    addons: &[],
                    metallb: &MetalLbConfig::default(),
                },
                &workers,
                ServerMode::Standard,
                None,
            )
            .unwrap();

            let invocations = runner.invocations.lock().unwrap();
            let install = invocations
                .iter()
                .position(|(_, args)| args.iter().any(|arg| arg.contains("cat <<'EOF'")))
                .expect("CNI manifest should be installed");
            let readiness = invocations
                .iter()
                .enumerate()
                .filter(|(_, (_, args))| args.iter().any(|arg| arg.contains("Ready=True")))
                .collect::<Vec<_>>();
            assert_eq!(
                readiness.len(),
                3,
                "all three nodes must be checked for {cni:?}"
            );
            assert!(readiness.iter().all(|(index, _)| *index > install));
        }
    }

    #[test]
    fn builtin_flannel_does_not_install_a_second_cni() {
        let runner = FakeRunner::default();
        let control_plane = Instance::new(
            "cp".to_owned(),
            NodeRole::ControlPlane,
            "/runtime/cp".into(),
        );
        super::install_addons_and_wait_for_nodes(
            PostReadyAddonRequest {
                backend: &crate::backend::lima::BACKEND,
                runner: &runner,
                control_plane: &control_plane,
                guest_kubeconfig: Path::new("/runtime/cp/kubeconfig.yaml"),
                cni: CniPlugin::Flannel,
                builtin_flannel: true,
                cilium_version: DEFAULT_CILIUM_VERSION,
                addons: &[],
                metallb: &MetalLbConfig::default(),
            },
            &[],
            ServerMode::Standard,
            None,
        )
        .unwrap();
        let invocations = runner.invocations.lock().unwrap();
        assert!(
            invocations
                .iter()
                .all(|(_, args)| { args.iter().all(|arg| !arg.contains("kalisto-flannel.yaml")) })
        );
        assert!(
            invocations
                .iter()
                .any(|(_, args)| { args.iter().any(|arg| arg.contains("Ready=True")) })
        );
    }

    #[test]
    fn failed_cni_install_does_not_report_node_readiness() {
        let runner = FakeRunner::with_invocation_failure_containing([], "kalisto-flannel.yaml");
        let control_plane = Instance::new(
            "cp".to_owned(),
            NodeRole::ControlPlane,
            "/runtime/cp".into(),
        );
        let result = super::install_addons_and_wait_for_nodes(
            PostReadyAddonRequest {
                backend: &crate::backend::lima::BACKEND,
                runner: &runner,
                control_plane: &control_plane,
                guest_kubeconfig: Path::new("/runtime/cp/kubeconfig.yaml"),
                cni: CniPlugin::Flannel,
                builtin_flannel: false,
                cilium_version: DEFAULT_CILIUM_VERSION,
                addons: &[],
                metallb: &MetalLbConfig::default(),
            },
            &[],
            ServerMode::Standard,
            None,
        );
        assert!(result.is_err());
        assert!(
            runner
                .invocations
                .lock()
                .unwrap()
                .iter()
                .all(|(_, args)| { args.iter().all(|arg| !arg.contains("Ready=True")) })
        );
    }

    #[test]
    fn failed_node_deletion_preserves_cluster_definition() {
        let root =
            std::env::temp_dir().join(format!("kalisto-destroy-failed-{}", std::process::id()));
        let state_root = root.join("workspace/.kalisto/clusters/broken");
        save_state(
            &state_root,
            "broken",
            vec![("cp", NodeRole::ControlPlane), ("worker", NodeRole::Worker)],
        );
        let runner = FakeRunner::with_invocation_failure_containing([], "delete");
        assert!(super::destroy_at_root(&state_root, "broken", &runner).is_err());
        assert!(ClusterState::state_file(&state_root).exists());
        let invocations = runner.invocations.lock().unwrap();
        assert_eq!(
            invocations
                .iter()
                .filter(|(_, args)| args.iter().any(|arg| arg == "delete"))
                .count(),
            2
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn defined_names_include_saved_clusters_without_probing_backends() {
        let root = std::env::temp_dir().join(format!("kalisto-defined-{}", std::process::id()));
        let clusters = root.join("workspace/.kalisto/clusters");
        save_state(&clusters.join("second"), "second", vec![]);
        save_state(&clusters.join("first"), "first", vec![]);
        fs::create_dir_all(clusters.join("not-a-cluster")).unwrap();
        assert_eq!(
            super::defined_cluster_names_at_root(&clusters).unwrap(),
            vec!["first".to_owned(), "second".to_owned()],
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn defined_names_are_empty_when_no_clusters_directory_exists() {
        let root =
            std::env::temp_dir().join(format!("kalisto-no-definitions-{}", std::process::id()));
        assert!(
            super::defined_cluster_names_at_root(&root)
                .unwrap()
                .is_empty()
        );
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
            clusters[0].nodes[0].backend_status,
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
        down_at_root(&state_root, "dev", false, &runner).unwrap();
        assert!(state_root.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scale_with_runner_adds_workers_and_persists_state() {
        let root = std::env::temp_dir().join(format!("kalisto-scale-{}", std::process::id()));
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
        fs::write(state_root.join("node-token"), "secret\n").unwrap();

        let runner = FakeRunner::with_statuses([("kalisto-dev-cp", "Running")]);
        let outcome = scale_at_root(&state_root, "dev", 2, &runner).unwrap();
        assert_eq!(outcome.previous_workers, 1);
        assert_eq!(outcome.current_workers, 2);
        assert_eq!(outcome.added_workers, vec!["kalisto-dev-w1".to_owned()]);

        let loaded = ClusterState::load(&state_root).unwrap();
        assert_eq!(loaded.worker_count, 2);
        assert_eq!(loaded.nodes.len(), 3);
        assert!(
            loaded
                .nodes
                .iter()
                .any(|node| node.name == "kalisto-dev-w1")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scale_with_runner_skips_worker_readiness_when_cluster_has_no_cni() {
        let root =
            std::env::temp_dir().join(format!("kalisto-scale-no-cni-{}", std::process::id()));
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
        fs::write(state_root.join("node-token"), "secret\n").unwrap();

        let mut state = ClusterState::load(&state_root).unwrap();
        state.cni_plugin = CniPlugin::None;
        CniPlugin::None.apply_component_defaults(&mut state.component_toggles);
        state.save(&state_root).unwrap();

        let runner = FakeRunner::with_invocation_failure_containing(
            [("kalisto-dev-cp", "Running")],
            "kubectl get node",
        );
        let outcome = scale_at_root(&state_root, "dev", 2, &runner).unwrap();

        assert_eq!(outcome.previous_workers, 1);
        assert_eq!(outcome.current_workers, 2);
        assert_eq!(outcome.added_workers, vec!["kalisto-dev-w1".to_owned()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_scale_target_instances_deletes_target_workers() {
        let runner = FakeRunner::default();
        let workers = vec![
            Instance::new(
                "kalisto-dev-w1".to_owned(),
                NodeRole::Worker,
                Path::new("/tmp/kalisto-dev-w1").to_path_buf(),
            ),
            Instance::new(
                "kalisto-dev-w2".to_owned(),
                NodeRole::Worker,
                Path::new("/tmp/kalisto-dev-w2").to_path_buf(),
            ),
        ];

        cleanup_scale_target_instances(&crate::backend::lima::BACKEND, &runner, &workers);

        let invocations = runner.invocations.lock().unwrap();
        assert!(invocations.iter().any(|(program, args)| {
            program == "limactl"
                && args
                    == &vec![
                        "-y".to_owned(),
                        "delete".to_owned(),
                        "--force".to_owned(),
                        "kalisto-dev-w2".to_owned(),
                    ]
        }));
        assert!(invocations.iter().any(|(program, args)| {
            program == "limactl"
                && args
                    == &vec![
                        "-y".to_owned(),
                        "delete".to_owned(),
                        "--force".to_owned(),
                        "kalisto-dev-w1".to_owned(),
                    ]
        }));
    }

    #[test]
    fn up_with_runner_respects_requested_worker_count_for_existing_cluster() {
        let root = std::env::temp_dir().join(format!("kalisto-up-workers-{}", std::process::id()));
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

        let mut spec = ClusterSpec::new("dev".to_owned(), workspace_root.clone());
        spec.worker_count = 5;
        spec.requested_worker_count = Some(5);
        let state = ClusterState::load(&state_root).unwrap();

        let resolved = resolve_up_spec(&state, &spec);

        assert_eq!(resolved.worker_count, 5);
        assert_eq!(resolved.requested_worker_count, Some(5));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn record_desired_cni_state_updates_cluster_networking_mode() {
        let root = std::env::temp_dir().join(format!("kalisto-addon-state-{}", std::process::id()));
        let state_root = root.join("cluster");
        fs::create_dir_all(&state_root).unwrap();
        save_state(
            &state_root,
            "dev",
            vec![("kalisto-dev-cp", NodeRole::ControlPlane)],
        );

        let mut state = ClusterState::load(&state_root).unwrap();
        assert_eq!(state.cni_plugin, CniPlugin::Flannel);
        assert!(state.component_toggles.flannel_backend_none);

        assert!(record_desired_addon_state(&mut state, AddonId::CniCilium));
        assert_eq!(state.cni_plugin, CniPlugin::Cilium);
        assert!(state.component_toggles.flannel_backend_none);
        assert!(!state.component_toggles.disable_coredns);
        assert!(state.component_toggles.disable_network_policy);
        assert!(state.addons.is_empty());
    }

    #[test]
    fn record_desired_storage_addon_persists_once() {
        let root = std::env::temp_dir().join(format!("kalisto-addon-list-{}", std::process::id()));
        let state_root = root.join("cluster");
        fs::create_dir_all(&state_root).unwrap();
        save_state(
            &state_root,
            "dev",
            vec![("kalisto-dev-cp", NodeRole::ControlPlane)],
        );

        let mut state = ClusterState::load(&state_root).unwrap();
        assert!(record_desired_addon_state(
            &mut state,
            AddonId::StorageLocalPath
        ));
        assert_eq!(state.addons, vec!["storage/local-path".to_owned()]);
        assert!(!record_desired_addon_state(
            &mut state,
            AddonId::StorageLocalPath
        ));
        assert_eq!(state.addons, vec!["storage/local-path".to_owned()]);
    }

    #[test]
    fn lb_servicelb_enables_k3s_servicelb_without_mutating_addon_catalog() {
        let root =
            std::env::temp_dir().join(format!("kalisto-servicelb-state-{}", std::process::id()));
        let state_root = root.join("cluster");
        fs::create_dir_all(&state_root).unwrap();
        save_state(
            &state_root,
            "dev",
            vec![("kalisto-dev-cp", NodeRole::ControlPlane)],
        );

        let mut state = ClusterState::load(&state_root).unwrap();
        assert!(state.component_toggles.disable_servicelb);
        assert!(state.addons.is_empty());

        assert!(record_desired_addon_state(&mut state, AddonId::LbServiceLb));
        assert!(!state.component_toggles.disable_servicelb);
        assert!(state.addons.is_empty());
        assert!(!record_desired_addon_state(
            &mut state,
            AddonId::LbServiceLb
        ));
    }

    #[test]
    fn addon_installs_do_not_require_cluster_recycle() {
        let mut state = fixture_cluster_state("dev");

        assert!(record_desired_addon_state(&mut state, AddonId::CniCilium));
        assert!(record_desired_addon_state(&mut state, AddonId::LbServiceLb));
        assert!(record_desired_addon_state(
            &mut state,
            AddonId::StorageLocalPath
        ));
    }

    #[test]
    fn install_addon_rejects_flannel_when_cluster_uses_cilium() {
        let mut state = fixture_cluster_state("dev");
        state.cni_plugin = CniPlugin::Cilium;

        let error = validate_addon_install_allowed(&state, AddonId::CniFlannel).unwrap_err();

        match error {
            Error::InvalidArgs(message) => {
                assert!(message.contains("cni/flannel"));
                assert!(message.contains("cilium"));
                assert!(message.contains("mutually exclusive"));
            }
            other => panic!("expected invalid args error, got {other:?}"),
        }
    }

    #[test]
    fn install_addon_rejects_cilium_when_cluster_uses_flannel() {
        let mut state = fixture_cluster_state("dev");
        state.cni_plugin = CniPlugin::Flannel;

        let error = validate_addon_install_allowed(&state, AddonId::CniCilium).unwrap_err();

        match error {
            Error::InvalidArgs(message) => {
                assert!(message.contains("cni/cilium"));
                assert!(message.contains("flannel"));
                assert!(message.contains("mutually exclusive"));
            }
            other => panic!("expected invalid args error, got {other:?}"),
        }
    }

    #[test]
    fn install_addon_allows_non_conflicting_cni_selection() {
        let mut state = fixture_cluster_state("dev");
        state.cni_plugin = CniPlugin::None;

        assert!(validate_addon_install_allowed(&state, AddonId::CniCilium).is_ok());
        assert!(validate_addon_install_allowed(&state, AddonId::CniFlannel).is_ok());
    }

    #[test]
    fn parse_lima_instance_metrics_extracts_pid_and_disk() {
        let metrics = parse_lima_instance_metrics(
            "kalisto-default-cp\t/Users/example/.lima/kalisto-default-cp\t65023\t65023\t12884901888\n",
            "kalisto-default-cp",
        )
        .unwrap();

        assert_eq!(metrics.pid, Some(65023));
        assert_eq!(metrics.rootfs_bytes, Some(12_884_901_888));
    }

    #[test]
    fn parse_lima_guest_network_metrics_extracts_ip_and_counters() {
        let (guest_ip, rx_bytes, tx_bytes) =
            parse_lima_guest_network_metrics("192.168.5.2\n1024\n2048\n");

        assert_eq!(guest_ip.as_deref(), Some("192.168.5.2"));
        assert_eq!(rx_bytes, Some(1024));
        assert_eq!(tx_bytes, Some(2048));
    }

    #[test]
    fn install_addon_at_root_applies_cilium_without_restarting_cluster() {
        let _guard = env_lock();
        let original_wait_for_addons = std::env::var_os("KALISTO_WAIT_FOR_ADDONS");
        let root =
            std::env::temp_dir().join(format!("kalisto-addon-install-{}", std::process::id()));
        let state_root = root.join("cluster");
        fs::create_dir_all(&state_root).unwrap();
        save_state(
            &state_root,
            "dev",
            vec![("kalisto-dev-cp", NodeRole::ControlPlane)],
        );

        let mut state = ClusterState::load(&state_root).unwrap();
        state.cni_plugin = CniPlugin::None;
        CniPlugin::None.apply_component_defaults(&mut state.component_toggles);
        state.save(&state_root).unwrap();

        unsafe {
            std::env::remove_var("KALISTO_WAIT_FOR_ADDONS");
        }

        let runner = FakeRunner::with_statuses([("kalisto-dev-cp", "Running")]);
        let outcome = install_addon_at_root(&state_root, "dev", "cni/cilium", &runner).unwrap();

        assert_eq!(outcome.cluster, "dev");
        assert_eq!(outcome.addon, "cni/cilium");

        let state = ClusterState::load(&state_root).unwrap();
        assert_eq!(state.cni_plugin, CniPlugin::Cilium);

        let invocations = runner.invocations.lock().unwrap().clone();
        assert!(invocations.iter().any(|(program, args)| {
            program == "limactl"
                && args.iter().any(|arg| arg == "shell")
                && args.iter().any(|arg| arg.contains("kalisto-cilium.yaml"))
        }));
        assert!(!invocations.iter().any(|(program, args)| {
            program == "limactl" && args.iter().any(|arg| arg == "stop" || arg == "start")
        }));

        restore_env("KALISTO_WAIT_FOR_ADDONS", original_wait_for_addons);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_addon_at_root_applies_flannel_without_restarting_cluster() {
        let _guard = env_lock();
        let original_wait_for_addons = std::env::var_os("KALISTO_WAIT_FOR_ADDONS");
        let root = std::env::temp_dir().join(format!(
            "kalisto-addon-install-flannel-{}",
            std::process::id()
        ));
        let state_root = root.join("cluster");
        fs::create_dir_all(&state_root).unwrap();
        save_state(
            &state_root,
            "dev",
            vec![("kalisto-dev-cp", NodeRole::ControlPlane)],
        );

        let mut state = ClusterState::load(&state_root).unwrap();
        state.cni_plugin = CniPlugin::None;
        CniPlugin::None.apply_component_defaults(&mut state.component_toggles);
        state.save(&state_root).unwrap();

        unsafe {
            std::env::remove_var("KALISTO_WAIT_FOR_ADDONS");
        }

        let runner = FakeRunner::with_statuses([("kalisto-dev-cp", "Running")]);
        let outcome = install_addon_at_root(&state_root, "dev", "cni/flannel", &runner).unwrap();

        assert_eq!(outcome.cluster, "dev");
        assert_eq!(outcome.addon, "cni/flannel");

        let state = ClusterState::load(&state_root).unwrap();
        assert_eq!(state.cni_plugin, CniPlugin::Flannel);
        assert!(state.component_toggles.flannel_backend_none);

        let invocations = runner.invocations.lock().unwrap().clone();
        assert!(invocations.iter().any(|(program, args)| {
            program == "limactl"
                && args.iter().any(|arg| arg == "shell")
                && args.iter().any(|arg| arg.contains("kalisto-flannel.yaml"))
        }));
        assert!(!invocations.iter().any(|(program, args)| {
            program == "limactl" && args.iter().any(|arg| arg == "stop" || arg == "start")
        }));

        restore_env("KALISTO_WAIT_FOR_ADDONS", original_wait_for_addons);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_addon_at_root_does_not_persist_cilium_state_when_apply_fails() {
        let _guard = env_lock();
        let original_wait_for_addons = std::env::var_os("KALISTO_WAIT_FOR_ADDONS");
        let root =
            std::env::temp_dir().join(format!("kalisto-addon-install-fail-{}", std::process::id()));
        let state_root = root.join("cluster");
        fs::create_dir_all(&state_root).unwrap();
        save_state(
            &state_root,
            "dev",
            vec![("kalisto-dev-cp", NodeRole::ControlPlane)],
        );

        let mut state = ClusterState::load(&state_root).unwrap();
        state.cni_plugin = CniPlugin::None;
        CniPlugin::None.apply_component_defaults(&mut state.component_toggles);
        state.save(&state_root).unwrap();

        unsafe {
            std::env::remove_var("KALISTO_WAIT_FOR_ADDONS");
        }

        let runner = FakeRunner::with_invocation_failure_containing(
            [("kalisto-dev-cp", "Running")],
            "kalisto-cilium.yaml",
        );
        let error = install_addon_at_root(&state_root, "dev", "cni/cilium", &runner).unwrap_err();
        assert!(matches!(error, Error::CommandFailed { .. }));

        let state = ClusterState::load(&state_root).unwrap();
        assert_eq!(state.cni_plugin, CniPlugin::None);

        restore_env("KALISTO_WAIT_FOR_ADDONS", original_wait_for_addons);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_metallb_addon_uses_project_kalisto_toml_config() {
        let _guard = env_lock();
        let original_wait_for_addons = std::env::var_os("KALISTO_WAIT_FOR_ADDONS");
        let root = std::env::temp_dir().join(format!(
            "kalisto-addon-metallb-config-{}",
            std::process::id()
        ));
        let workspace_root = root.join("workspace");
        let state_root = root.join("cluster");
        fs::create_dir_all(&workspace_root).unwrap();
        fs::create_dir_all(&state_root).unwrap();

        fs::write(
            workspace_root.join("kalisto.toml"),
            r#"
[plugins.metallb]
addresses = ["10.20.0.200-10.20.0.220"]
"#,
        )
        .unwrap();

        let mut component_toggles = ComponentToggles::default();
        CniPlugin::Flannel.apply_component_defaults(&mut component_toggles);

        let state = ClusterState {
            name: "dev".to_owned(),
            backend: BackendKind::Lima,
            workspace_root: workspace_root.clone(),
            template: "template://default".to_owned(),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            api_port: 16443,
            worker_count: 0,
            control_plane_resources: NodeResources {
                cpus: 4,
                memory_gib: 6,
            },
            worker_resources: NodeResources {
                cpus: 2,
                memory_gib: 4,
            },
            enable_rosetta: true,
            network: "lima:user-v2".to_owned(),
            k3s_version: "v1.33.5+k3s1".to_owned(),
            cni_plugin: CniPlugin::Flannel,
            cilium_version: DEFAULT_CILIUM_VERSION.to_owned(),
            addons: Vec::new(),
            metallb: MetalLbConfig::default(),
            server_mode: ServerMode::Standard,
            component_toggles,
            kubeconfig_path: state_root.join("kubeconfig.yaml"),
            token_path: state_root.join("node-token"),
            nodes: vec![NodeState {
                name: "kalisto-dev-cp".to_owned(),
                role: NodeRole::ControlPlane,
                artifacts_root: state_root.join("kalisto-dev-cp"),
            }],
        };
        state.save(&state_root).unwrap();

        unsafe {
            std::env::remove_var("KALISTO_WAIT_FOR_ADDONS");
        }

        let runner = FakeRunner::with_statuses([("kalisto-dev-cp", "Running")]);
        let outcome = install_addon_at_root(&state_root, "dev", "lb/metallb", &runner).unwrap();
        assert_eq!(outcome.addon, "lb/metallb");

        let state = ClusterState::load(&state_root).unwrap();
        assert_eq!(
            state.metallb.addresses,
            vec!["10.20.0.200-10.20.0.220".to_owned()]
        );

        let invocations = runner.invocations.lock().unwrap().clone();
        assert!(invocations.iter().any(|(program, args)| {
            program == "limactl"
                && args.iter().any(|arg| arg == "shell")
                && args
                    .iter()
                    .any(|arg| arg.contains("kalisto-metallb-config.yaml"))
        }));

        restore_env("KALISTO_WAIT_FOR_ADDONS", original_wait_for_addons);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn logs_returns_local_node_log_sections() {
        let root = std::env::temp_dir().join(format!("kalisto-logs-{}", std::process::id()));
        let state_root = root.join("cluster");
        let node_root = state_root.join("kalisto-dev-cp");
        fs::create_dir_all(&node_root).unwrap();
        save_state(
            &state_root,
            "dev",
            vec![("kalisto-dev-cp", NodeRole::ControlPlane)],
        );
        fs::write(node_root.join("k3s.stdout.log"), "server up\n").unwrap();
        fs::write(node_root.join("firecracker.stdout.log"), "boot log\n").unwrap();

        let logs = logs_at_root(&state_root, "kalisto-dev-cp").unwrap();
        assert_eq!(logs.cluster, "dev");
        assert_eq!(logs.node, "kalisto-dev-cp");
        assert_eq!(logs.sections.len(), 2);
        assert_eq!(logs.sections[0].label, "k3s stdout");
        assert_eq!(logs.sections[0].content, "server up\n");
        assert_eq!(logs.sections[1].label, "firecracker serial");
        assert_eq!(logs.sections[1].content, "boot log\n");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[allow(clippy::similar_names)] // uid and gid are intentionally parallel variables
    fn handoff_cluster_paths_to_invoking_user_runs_recursive_chown_under_sudo() {
        let _guard = env_lock();
        let sudo_uid_before = std::env::var_os("SUDO_UID");
        let sudo_gid_before = std::env::var_os("SUDO_GID");
        let root = std::env::temp_dir().join(format!("kalisto-handoff-{}", std::process::id()));
        let state_root = root.join("state");
        let runtime_root = root.join("runtime");
        fs::create_dir_all(&state_root).unwrap();
        fs::create_dir_all(&runtime_root).unwrap();

        unsafe {
            std::env::set_var("SUDO_UID", "1000");
            std::env::set_var("SUDO_GID", "1001");
        }

        let runner = FakeRunner::default();
        super::handoff_cluster_paths_to_invoking_user(&runner, &state_root, &runtime_root).unwrap();

        let invocations = runner.invocations.lock().unwrap().clone();
        assert_eq!(
            invocations,
            vec![
                chown_invocation("1000:1001", &state_root),
                chown_invocation("1000:1001", &runtime_root),
            ]
        );

        restore_env("SUDO_UID", sudo_uid_before);
        restore_env("SUDO_GID", sudo_gid_before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[allow(clippy::similar_names)] // uid and gid are intentionally parallel variables
    fn handoff_cluster_paths_to_invoking_user_is_noop_without_sudo_ids() {
        let _guard = env_lock();
        let sudo_uid_before = std::env::var_os("SUDO_UID");
        let sudo_gid_before = std::env::var_os("SUDO_GID");
        let root =
            std::env::temp_dir().join(format!("kalisto-handoff-noop-{}", std::process::id()));
        let state_root = root.join("state");
        let runtime_root = root.join("runtime");
        fs::create_dir_all(&state_root).unwrap();
        fs::create_dir_all(&runtime_root).unwrap();

        unsafe {
            std::env::remove_var("SUDO_UID");
            std::env::remove_var("SUDO_GID");
        }

        let runner = FakeRunner::default();
        super::handoff_cluster_paths_to_invoking_user(&runner, &state_root, &runtime_root).unwrap();

        assert!(runner.invocations.lock().unwrap().is_empty());

        restore_env("SUDO_UID", sudo_uid_before);
        restore_env("SUDO_GID", sudo_gid_before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn create_with_runner_checks_backend_before_creating_cluster_roots() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root =
            std::env::temp_dir().join(format!("kalisto-create-preflight-{}", std::process::id()));
        let workspace_root = root.join("workspace");
        let data_home = root.join("xdg-data");
        fs::create_dir_all(&workspace_root).unwrap();

        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }

        let mut spec =
            crate::cluster::spec::ClusterSpec::new("default".to_owned(), workspace_root.clone());
        spec.backend = BackendKind::Lima;
        let template_root = workspace_root.join("artifacts").join("lima-node");
        fs::create_dir_all(&template_root).unwrap();
        let template = template_root.join("ubuntu-24.04-minimal-k3s-arm64-finalized.yaml");
        fs::write(&template, "vmType: vz\n").unwrap();
        spec.template = template.display().to_string();

        let state_root = ClusterSpec::state_root_for(&spec.name).unwrap();
        let runtime_root = state_root.join("runtime");

        let error = super::create_with_runner(&spec, &FakeRunner::with_limactl_version_failure())
            .unwrap_err();
        assert!(matches!(error, Error::CommandFailed { .. }));
        assert!(!state_root.exists());
        assert!(!runtime_root.exists());

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn up_with_runner_creates_missing_cluster_definition_before_starting_instances() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root =
            std::env::temp_dir().join(format!("kalisto-up-autocreate-{}", std::process::id()));
        let workspace_root = root.join("workspace");
        let data_home = root.join("xdg-data");
        fs::create_dir_all(&workspace_root).unwrap();

        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }

        let mut spec =
            crate::cluster::spec::ClusterSpec::new("default".to_owned(), workspace_root.clone());
        spec.backend = BackendKind::Lima;
        let template_root = workspace_root.join("artifacts").join("lima-node");
        fs::create_dir_all(&template_root).unwrap();
        let template = template_root.join("ubuntu-24.04-minimal-k3s-arm64-finalized.yaml");
        fs::write(&template, "vmType: vz\n").unwrap();
        spec.template = template.display().to_string();

        let state_root = ClusterSpec::state_root_for(&spec.name).unwrap();
        let state_file = ClusterState::state_file(&state_root);
        let runtime_root = state_root.join("runtime");

        let image = template_root.join("node.qcow2");
        fs::write(&image, "image").unwrap();
        fs::write(
            &template,
            format!("vmType: vz\nimages:\n  - location: {}\n", image.display()),
        )
        .unwrap();
        crate::pull::test_fixture(&spec);
        let runner = FakeRunner::with_invocation_failure_containing([], "start");
        let error = super::up_with_runner(&spec, &runner).unwrap_err();
        assert!(matches!(error, Error::CommandFailed { .. }));
        assert!(state_root.exists());
        assert!(state_file.exists());
        assert!(runtime_root.exists());

        let state = ClusterState::load(&state_root).unwrap();
        assert_eq!(state.name, spec.name);
        assert_eq!(state.worker_count, spec.worker_count);

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    fn save_state(root: &std::path::Path, name: &str, nodes: Vec<(&str, NodeRole)>) {
        let mut component_toggles = ComponentToggles::default();
        CniPlugin::Flannel.apply_component_defaults(&mut component_toggles);

        let state = ClusterState {
            name: name.to_owned(),
            backend: BackendKind::Lima,
            workspace_root: root.ancestors().nth(3).unwrap().to_path_buf(),
            template: "template://default".to_owned(),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            api_port: 16443,
            worker_count: nodes
                .iter()
                .filter(|(_, role)| *role == NodeRole::Worker)
                .count(),
            control_plane_resources: NodeResources {
                cpus: 4,
                memory_gib: 6,
            },
            worker_resources: NodeResources {
                cpus: 2,
                memory_gib: 4,
            },
            enable_rosetta: true,
            network: "lima:user-v2".to_owned(),
            k3s_version: "v1.33.5+k3s1".to_owned(),
            cni_plugin: CniPlugin::Flannel,
            cilium_version: DEFAULT_CILIUM_VERSION.to_owned(),
            addons: Vec::new(),
            metallb: MetalLbConfig::default(),
            server_mode: ServerMode::Standard,
            component_toggles,
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

    fn fixture_cluster_state(name: &str) -> ClusterState {
        ClusterState {
            name: name.to_owned(),
            backend: BackendKind::Lima,
            workspace_root: Path::new(".").to_path_buf(),
            template: "template://default".to_owned(),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            api_port: 16443,
            worker_count: 0,
            control_plane_resources: NodeResources {
                cpus: 4,
                memory_gib: 6,
            },
            worker_resources: NodeResources {
                cpus: 2,
                memory_gib: 4,
            },
            enable_rosetta: true,
            network: "lima:user-v2".to_owned(),
            k3s_version: "v1.33.5+k3s1".to_owned(),
            cni_plugin: CniPlugin::None,
            cilium_version: DEFAULT_CILIUM_VERSION.to_owned(),
            addons: Vec::new(),
            metallb: MetalLbConfig::default(),
            server_mode: ServerMode::Standard,
            component_toggles: ComponentToggles::default(),
            kubeconfig_path: Path::new("kubeconfig.yaml").to_path_buf(),
            token_path: Path::new("node-token").to_path_buf(),
            nodes: Vec::new(),
        }
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
            Error::InvalidArgs(message) => assert!(message.contains("invalid cluster state json")),
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

        let rewritten = super::rewrite_kubeconfig_server(kubeconfig, "127.0.0.1:16443").unwrap();
        assert!(rewritten.contains("server: https://127.0.0.1:16443"));
        assert!(!rewritten.contains("server: https://127.0.0.1:6443"));
    }

    #[test]
    fn rewrite_kubeconfig_server_requires_server_line() {
        let error =
            super::rewrite_kubeconfig_server("apiVersion: v1\n", "127.0.0.1:16443").unwrap_err();
        match error {
            Error::InvalidArgs(message) => {
                assert!(message.contains("did not contain a server endpoint"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn control_plane_tls_sans_include_join_and_api_hosts_once() {
        let tls_sans = super::tls_sans_for_control_plane("172.31.2.10", "172.31.2.10:6443");
        assert_eq!(
            tls_sans,
            vec!["127.0.0.1".to_owned(), "172.31.2.10".to_owned()]
        );
    }

    fn chown_invocation(owner: &str, path: &Path) -> (String, Vec<String>) {
        (
            "chown".to_owned(),
            vec![
                "-R".to_owned(),
                owner.to_owned(),
                path.display().to_string(),
            ],
        )
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }
}
