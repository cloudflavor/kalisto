use std::collections::{BTreeMap, HashMap};
use std::fs::{self};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::backend::BackendKind;
use crate::cli::{BenchArgs, UpArgs};
use crate::cluster::addon::{CniPlugin, MetalLbConfig, MetalLbMode};
use crate::cluster::manager::{
    benchmark as cluster_benchmark, create as cluster_create, destroy as cluster_destroy,
    down as cluster_down, install_addon as cluster_install_addon, list as cluster_list,
    scale as cluster_scale, stats as cluster_stats, status as cluster_status, up as cluster_up,
};
use crate::cluster::spec::ImageArch;
use crate::cluster::state::ClusterState;
use crate::cluster::workload::{ManifestApplyOutcome, apply_manifest as cluster_apply_manifest};
use crate::error::{Error, Result};
use crate::k3s::ServerMode;
use crate::paths::{
    clear_default_cluster_name_if_matches, data_root,
    handoff_default_cluster_file_to_invoking_user, read_default_cluster_name,
    write_default_cluster_name,
};
use crate::plugin::{PluginSpec, built_in_catalog};
use crate::probe::macos_lima::{ProbeConfig as MacosLimaProbeConfig, run as run_macos_lima_probe};
use crate::probe::macos_lima_k3s::{
    ProbeConfig as MacosLimaK3sProbeConfig, run as run_macos_lima_k3s_probe,
};
use crate::process::SystemRunner;

const SUPPORTED_PROTOCOL_VERSION: &str = "2025-11-25";
const JSONRPC_VERSION: &str = "2.0";
const DEFAULT_TASK_TTL_MILLIS: u64 = 3_600_000;
const DEFAULT_TASK_POLL_INTERVAL_MILLIS: u64 = 1_000;
const DEFAULT_TASK_LIST_LIMIT: usize = 100;
const TASK_STATUS_WORKING: &str = "working";
const TASK_STATUS_COMPLETED: &str = "completed";
const TASK_STATUS_FAILED: &str = "failed";
const TASK_STATUS_CANCELLING: &str = "cancelling";
const TASK_STATUS_CANCELLED: &str = "cancelled";
const META_RELATED_TASK: &str = "io.modelcontextprotocol/related-task";
const META_IMMEDIATE_RESPONSE: &str = "io.modelcontextprotocol/model-immediate-response";

pub fn run(project_root: Option<PathBuf>) -> Result<()> {
    let server = McpServer::load(project_root)?;
    server.serve()
}

pub fn run_task_runner(manifest_path: PathBuf, exit_path: PathBuf) -> Result<()> {
    let manifest = load_task_manifest(&manifest_path)?;
    let result = execute_task_runner_manifest(&manifest);
    write_task_exit_record(&exit_path, &result)?;
    result
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskRunnerManifest {
    tool_name: String,
    arguments: Value,
    default_project_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskExitRecord {
    exit_code: Option<i32>,
    error: Option<String>,
    finished_at: String,
}

fn load_task_manifest(path: &std::path::Path) -> Result<TaskRunnerManifest> {
    let content = fs::read_to_string(path).map_err(io_error("read_to_string"))?;
    serde_json::from_str(&content).map_err(|error| {
        Error::InvalidArgs(format!(
            "failed to parse task runner manifest at {}: {error}",
            path.display()
        ))
    })
}

fn execute_task_runner_manifest(manifest: &TaskRunnerManifest) -> Result<()> {
    match manifest.tool_name.as_str() {
        "clusters.create" => {
            let lifecycle: ClusterLifecycleArgs = parse_task_runner_args(&manifest.arguments)?;
            let up = lifecycle
                .into_up_args(manifest.default_project_root.clone())
                .map_err(jsonrpc_to_error)?;
            let spec = up.into_cluster_spec()?;
            let outcome = cluster_create(&spec)?;
            maybe_write_default_cluster_name(&outcome.state.name)?;
            let state_root =
                crate::cluster::spec::ClusterSpec::state_root_for(&outcome.state.name)?;
            println!("cluster: {}", outcome.state.name);
            println!("state root: {}", state_root.display());
            println!("runtime root: {}", spec.runtime_root().display());
            println!("created cluster definition");
            Ok(())
        }
        "clusters.export" => {
            let request: ArchiveExportPayload = parse_task_runner_args(&manifest.arguments)?;
            let encryption = crate::archive::ExportEncryption::from_files(
                &request.recipients,
                request.passphrase_file.as_deref(),
            )?;
            let outcome = crate::archive::export_encrypted(
                &request.name,
                &request.output,
                request.stop,
                &encryption,
            )?;
            println!(
                "cluster: {}\narchive: {}\nnodes: {}\nbytes: {}\nencrypted: {}\ncontains secrets: true",
                outcome.name,
                outcome.path.display(),
                outcome.nodes,
                outcome.bytes,
                outcome.encrypted
            );
            Ok(())
        }
        "clusters.import" => {
            let request: ArchiveImportPayload = parse_task_runner_args(&manifest.arguments)?;
            let credentials = crate::archive::ImportDecryption::from_files(
                &request.identity_files,
                request.passphrase_file.as_deref(),
            )?;
            let outcome = crate::archive::import_encrypted(
                &request.input,
                request.api_port,
                request
                    .max_bytes
                    .unwrap_or(crate::archive::DEFAULT_MAX_BYTES),
                &credentials,
            )?;
            println!(
                "cluster: {}\nnodes: {}\nstate: stopped",
                outcome.name, outcome.nodes
            );
            Ok(())
        }
        "clusters.pull" => {
            let lifecycle: ClusterLifecycleArgs = parse_task_runner_args(&manifest.arguments)?;
            let up = lifecycle
                .into_up_args(manifest.default_project_root.clone())
                .map_err(jsonrpc_to_error)?;
            let assets = crate::pull::prepare(&up.into_cluster_spec()?)?;
            println!("prepared assets: {}", assets.root.display());
            println!("probe image: {}", assets.probe_image());
            Ok(())
        }
        "clusters.up" => {
            let lifecycle: ClusterLifecycleArgs = parse_task_runner_args(&manifest.arguments)?;
            let up = lifecycle
                .into_up_args(manifest.default_project_root.clone())
                .map_err(jsonrpc_to_error)?;
            let spec = up.into_cluster_spec()?;
            let outcome = cluster_up(&spec)?;
            maybe_write_default_cluster_name(&outcome.state.name)?;
            println!("cluster: {}", outcome.state.name);
            println!("kubeconfig: {}", outcome.state.kubeconfig_path.display());
            println!("control-plane ready: {}", outcome.control_plane_ready);
            println!("cluster ready: {}", outcome.cluster_ready);
            Ok(())
        }
        "clusters.down" => {
            let request: ClusterDownArgs = parse_task_runner_args(&manifest.arguments)?;
            cluster_down(&request.name, request.force)?;
            println!("stopped cluster `{}`", request.name);
            Ok(())
        }
        "clusters.destroy" => {
            let request: NamedClusterArgs = parse_task_runner_args(&manifest.arguments)?;
            cluster_destroy(&request.name)?;
            clear_default_cluster_name_if_matches(&request.name)?;
            println!("destroyed cluster `{}`", request.name);
            Ok(())
        }
        "clusters.scale" => {
            let request: ClusterScaleArgs = parse_task_runner_args(&manifest.arguments)?;
            let outcome = cluster_scale(&request.name, request.workers)?;
            println!("cluster: {}", outcome.name);
            println!(
                "workers: {} -> {}",
                outcome.previous_workers, outcome.current_workers
            );
            if outcome.added_workers.is_empty() {
                println!("added nodes: none");
            } else {
                println!("added nodes: {}", outcome.added_workers.join(", "));
            }
            Ok(())
        }
        "addons.install" => {
            let request: AddonInstallArgsPayload = parse_task_runner_args(&manifest.arguments)?;
            let outcome = cluster_install_addon(&request.name, &request.addon)?;
            println!("cluster: {}", outcome.cluster);
            println!("installed add-on: {}", outcome.addon);
            Ok(())
        }
        "workloads.apply" => {
            let request: WorkloadApplyArgs = parse_task_runner_args(&manifest.arguments)?;
            let manifest_content = fs::read_to_string(&request.manifest_path).map_err(|error| {
                Error::InvalidArgs(format!(
                    "failed to read workload manifest at {}: {error}",
                    request.manifest_path.display()
                ))
            })?;
            let outcome: ManifestApplyOutcome =
                cluster_apply_manifest(&request.name, &manifest_content, &SystemRunner)?;
            println!("cluster: {}", outcome.cluster);
            println!("node: {}", outcome.node);
            println!("applied: {}", request.manifest_path.display());
            Ok(())
        }
        "bench.run" => {
            let request: BenchArgsPayload = parse_task_runner_args(&manifest.arguments)?;
            let bench = request
                .into_bench_args(manifest.default_project_root.clone())
                .map_err(jsonrpc_to_error)?;
            let (spec, runs) = bench.into_cluster_spec()?;
            let samples = cluster_benchmark(&spec, runs)?;
            for sample in samples {
                println!(
                    "run {}: cluster={} control-plane-ready={} cluster-ready={}",
                    sample.run,
                    sample.cluster_name,
                    sample.control_plane_ready,
                    sample.cluster_ready
                );
            }
            Ok(())
        }
        "probes.macos.lima" => {
            let request: ProbeMacosLimaArgsPayload = parse_task_runner_args(&manifest.arguments)?;
            let mut config = MacosLimaProbeConfig::new(request.workspace_root);
            config.keep_resources = request.keep_resources.unwrap_or(false);
            config.enable_rosetta = !request.no_rosetta.unwrap_or(false);
            if let Some(template) = request.template {
                config.template = template;
            }
            if let Some(network) = request.network {
                config.network = network;
            }

            let report = run_macos_lima_probe(&config);
            for result in &report.results {
                println!("[{}] {}: {}", result.status, result.name, result.detail);
            }
            println!("{}", report.summary());
            if report.has_failures() {
                Err(Error::InvalidArgs(
                    "macOS Lima probe reported one or more failures".to_owned(),
                ))
            } else {
                Ok(())
            }
        }
        "probes.macos.lima.k3s" => {
            let request: ProbeMacosLimaK3sArgsPayload =
                parse_task_runner_args(&manifest.arguments)?;
            let mut config = MacosLimaK3sProbeConfig::new(request.workspace_root);
            config.keep_resources = request.keep_resources.unwrap_or(false);
            config.server_mode = if request.agentless.unwrap_or(false) {
                ServerMode::Agentless
            } else {
                ServerMode::Standard
            };
            config.enable_rosetta = !request.no_rosetta.unwrap_or(false);
            if let Some(api_port) = request.api_port {
                config.api_port = api_port;
            }
            if let Some(k3s_version) = request.k3s_version {
                config.k3s_version = k3s_version;
            }
            if let Some(template) = request.template {
                config.template = template;
            }
            if let Some(worker_count) = request.workers {
                config.worker_count = worker_count;
            }
            if let Some(network) = request.network {
                config.network = Some(network);
            }

            let report = run_macos_lima_k3s_probe(&config);
            for result in &report.results {
                println!("[{}] {}: {}", result.status, result.name, result.detail);
            }
            println!("{}", report.summary());
            if report.has_failures() {
                Err(Error::InvalidArgs(
                    "macOS Lima k3s probe reported one or more failures".to_owned(),
                ))
            } else {
                Ok(())
            }
        }
        _ => Err(Error::InvalidArgs(format!(
            "tool `{}` is not available for task runner execution",
            manifest.tool_name
        ))),
    }
}

fn write_task_exit_record(path: &std::path::Path, result: &Result<()>) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(io_error("create_dir_all"))?;
    }
    let record = TaskExitRecord {
        exit_code: Some(if result.is_ok() { 0 } else { 1 }),
        error: result.as_ref().err().map(ToString::to_string),
        finished_at: iso8601_now(),
    };
    let content = serde_json::to_vec_pretty(&record).map_err(|error| {
        Error::InvalidArgs(format!(
            "failed to serialize task runner exit record at {}: {error}",
            path.display()
        ))
    })?;
    fs::write(path, content).map_err(io_error("write"))
}

fn parse_task_runner_args<T: DeserializeOwned>(arguments: &Value) -> Result<T> {
    parse_tool_args(arguments.clone()).map_err(|error| Error::InvalidArgs(error.message))
}

fn maybe_write_default_cluster_name(name: &str) -> Result<()> {
    if read_default_cluster_name()?.is_none() {
        write_default_cluster_name(name)?;
        handoff_default_cluster_file_to_invoking_user(&SystemRunner)?;
    }
    Ok(())
}

fn jsonrpc_to_error(error: JsonRpcError) -> Error {
    Error::InvalidArgs(error.message)
}

fn task_manifest_for_record(record: &TaskRecord) -> Result<TaskRunnerManifest> {
    if !record.manifest_path.is_empty() {
        return load_task_manifest(std::path::Path::new(&record.manifest_path));
    }
    Ok(TaskRunnerManifest {
        tool_name: record.request.tool_name.clone(),
        arguments: record.request.arguments.clone(),
        default_project_root: None,
    })
}

fn task_cluster_name(manifest: &TaskRunnerManifest) -> Result<Option<String>> {
    match manifest.tool_name.as_str() {
        "clusters.create" | "clusters.pull" | "clusters.up" => {
            let lifecycle: ClusterLifecycleArgs = parse_task_runner_args(&manifest.arguments)?;
            let args = lifecycle
                .into_up_args(manifest.default_project_root.clone())
                .map_err(jsonrpc_to_error)?;
            Ok(Some(args.into_cluster_spec()?.name))
        }
        "clusters.down" | "clusters.destroy" | "clusters.scale" | "addons.install"
        | "workloads.apply" => {
            let name = manifest
                .arguments
                .get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            Ok(name)
        }
        "bench.run" => {
            let request: BenchArgsPayload = parse_task_runner_args(&manifest.arguments)?;
            let bench = request
                .into_bench_args(manifest.default_project_root.clone())
                .map_err(jsonrpc_to_error)?;
            let (spec, _) = bench.into_cluster_spec()?;
            Ok(Some(spec.name))
        }
        _ => Ok(None),
    }
}

fn cancel_task_from_cluster_state(record: &TaskRecord) -> Result<bool> {
    let manifest = task_manifest_for_record(record)?;
    let Some(name) = task_cluster_name(&manifest)? else {
        return Ok(false);
    };
    let state_root = crate::cluster::spec::ClusterSpec::state_root_for(&name)?;
    if !state_root.exists() {
        return Ok(false);
    }
    match manifest.tool_name.as_str() {
        "clusters.create" | "clusters.up" | "clusters.scale" | "addons.install" => {
            cluster_down(&name, true)?;
            Ok(true)
        }
        "bench.run" => {
            cluster_destroy(&name)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

struct McpServer {
    default_project_root: Option<PathBuf>,
    initialized: Mutex<bool>,
    framing: Mutex<Option<FramingMode>>,
    writer: Mutex<()>,
    tasks_dir: PathBuf,
    next_task_counter: AtomicU64,
    tasks: Mutex<BTreeMap<String, TaskRecord>>,
    executions: Mutex<HashMap<String, Arc<TaskExecution>>>,
}

impl McpServer {
    fn load(project_root: Option<PathBuf>) -> Result<Arc<Self>> {
        let tasks_dir = data_root()?.join("mcp").join("tasks");
        fs::create_dir_all(&tasks_dir).map_err(io_error("create_dir_all"))?;
        let mut records = BTreeMap::new();
        let mut refresh_after_load = Vec::new();
        for entry in fs::read_dir(&tasks_dir).map_err(io_error("read_dir"))? {
            let entry = entry.map_err(io_error("read_dir"))?;
            let path = entry.path();
            if !is_persisted_task_record_path(&path) {
                continue;
            }
            let content = fs::read_to_string(&path).map_err(io_error("read_to_string"))?;
            let mut record: TaskRecord = serde_json::from_str(&content).map_err(|error| {
                Error::InvalidArgs(format!(
                    "failed to parse persisted MCP task at {}: {error}",
                    path.display()
                ))
            })?;
            record.progress = progress_for_status(
                &record.status,
                record.status_message.clone(),
                record.child_id,
            );
            if !record.is_terminal() {
                let has_exit_record =
                    !record.exit_path.is_empty() && PathBuf::from(&record.exit_path).is_file();
                if has_exit_record {
                    refresh_after_load.push(record.task_id.clone());
                } else {
                    record.status_message = Some(
                        "Recovered detached MCP task; waiting for runner exit record.".to_owned(),
                    );
                    record.progress = running_task_progress(record.status_message.clone());
                    record.child_id = None;
                    record.last_updated_at = iso8601_now();
                    refresh_after_load.push(record.task_id.clone());
                }
            }
            records.insert(record.task_id.clone(), record);
        }

        let next_counter = records
            .keys()
            .filter_map(|task_id| task_id.rsplit('-').next())
            .filter_map(|suffix| suffix.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        let server = Arc::new(Self {
            default_project_root: project_root,
            initialized: Mutex::new(false),
            framing: Mutex::new(None),
            writer: Mutex::new(()),
            tasks_dir,
            next_task_counter: AtomicU64::new(next_counter),
            tasks: Mutex::new(records),
            executions: Mutex::new(HashMap::new()),
        });

        for task_id in refresh_after_load {
            let execution = Arc::new(TaskExecution::detached());
            let _ = server.refresh_task_state(&task_id, execution.as_ref());
            if let Some(record) = server.task(&task_id) {
                let _ = server.persist_task(&record);
            }
        }

        Ok(server)
    }

    fn serve(self: Arc<Self>) -> Result<()> {
        let stdin = io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let mut framing = self.framing_mode()?;
        while let Some(value) = read_stdio_message(&mut reader, &mut framing)? {
            if let Some(mode) = framing {
                self.set_framing_mode(mode)?;
            }
            self.handle_value(value)?;
        }
        Ok(())
    }

    fn handle_value(self: &Arc<Self>, value: Value) -> Result<()> {
        let Some(message) = value.as_object() else {
            self.write_error(Value::Null, -32600, "invalid JSON-RPC message".to_owned())?;
            return Ok(());
        };
        if message.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            self.write_error(
                message.get("id").cloned().unwrap_or(Value::Null),
                -32600,
                "unsupported jsonrpc version".to_owned(),
            )?;
            return Ok(());
        }

        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

        if !self.is_initialized() && method != "initialize" && method != "ping" {
            if let Some(id) = id {
                self.write_error(
                    id,
                    -32002,
                    "server is not initialized; call `initialize` first".to_owned(),
                )?;
            }
            return Ok(());
        }

        match id {
            Some(id) => self.handle_request(id, method, params),
            None => self.handle_notification(method),
        }
    }

    fn handle_request(self: &Arc<Self>, id: Value, method: &str, params: Value) -> Result<()> {
        match method {
            "initialize" => self.write_result(id, self.initialize_result()),
            "ping" => self.write_result(id, json!({})),
            "tools/list" => self.write_result(id, json!({ "tools": tool_definitions() })),
            "tools/call" => self.handle_tools_call(id, params),
            "tasks/get" => self.handle_task_get(id, params),
            "tasks/list" => self.handle_task_list(id, params),
            "tasks/result" => self.handle_task_result(id, params),
            "tasks/cancel" => self.handle_task_cancel(id, params),
            _ => self.write_error(id, -32601, format!("method not found: {method}")),
        }
    }

    fn handle_notification(&self, method: &str) -> Result<()> {
        if method == "notifications/initialized" {
            self.set_initialized(true);
        }
        Ok(())
    }

    fn initialize_result(&self) -> Value {
        self.set_initialized(true);
        json!({
            "protocolVersion": SUPPORTED_PROTOCOL_VERSION,
            "capabilities": {
                "tools": {
                    "listChanged": false
                },
                "tasks": {
                    "listChanged": false,
                    "cancellable": true,
                    "requests": {
                        "tools/call": true
                    }
                }
            },
            "serverInfo": {
                "name": "kalisto",
                "version": crate::version::build_version()
            },
            "instructions": concat!(
                "Kalisto exposes cluster lifecycle, inspection, add-on, bench, and probe operations over MCP stdio. ",
                "Use task-augmented `tools/call` for long-running or mutating operations. ",
                "On Linux with the firecracker backend, bridge networking requires sudo. ",
                "Start the MCP server with sudo, for example `sudo -E kalisto mcp`, only when you explicitly use `--network bridge`. ",
                "The default firecracker network mode is rootless `slirp4netns`. ",
                "VM log streaming is not available over MCP stdio because stdout is reserved for protocol messages."
            )
        })
    }

    fn handle_tools_call(self: &Arc<Self>, id: Value, params: Value) -> Result<()> {
        let request: ToolCallRequest = match parse_params(params) {
            Ok(request) => request,
            Err(error) => return self.write_error(id, error.code, error.message),
        };
        let Some(tool) = tool_by_name(&request.name) else {
            return self.write_error(id, -32601, format!("unknown tool `{}`", request.name));
        };

        let wants_task = request.task.is_some();
        match tool.task_support {
            TaskSupport::Required if !wants_task => {
                return self.write_error(
                    id,
                    -32601,
                    format!("tool `{}` requires task-augmented requests", request.name),
                );
            }
            TaskSupport::Forbidden if wants_task => {
                return self.write_error(
                    id,
                    -32601,
                    format!(
                        "tool `{}` does not support task-augmented requests",
                        request.name
                    ),
                );
            }
            TaskSupport::Required | TaskSupport::Forbidden => {}
        }

        let arguments = request.arguments.unwrap_or_else(|| json!({}));

        if wants_task {
            let task = match self.spawn_task(&request.name, arguments) {
                Ok(task) => task,
                Err(error) => return self.write_error(id, error.code, error.message),
            };
            return self.write_result(
                id,
                attach_related_task_meta(
                    json!({ "task": task.public_view() }),
                    &task.task_id,
                    Some(format!(
                        "Task `{}` accepted for tool `{}`. Poll `tasks/get` and fetch the final result with `tasks/result`.",
                        task.task_id, request.name
                    )),
                ),
            );
        }

        let result = match execute_direct_tool(
            &request.name,
            arguments,
            self.default_project_root.clone(),
        ) {
            Ok(result) => result,
            Err(error) => return self.write_error(id, error.code, error.message),
        };
        self.write_result(id, result)
    }

    fn spawn_task(
        self: &Arc<Self>,
        tool_name: &str,
        arguments: Value,
    ) -> std::result::Result<TaskRecord, JsonRpcError> {
        let tool = tool_by_name(tool_name)
            .ok_or_else(|| jsonrpc_error(-32601, format!("unknown tool `{tool_name}`")))?;
        validate_archive_arguments(tool_name, &arguments)?;
        // Open/validate secret files before persisting their paths, so accidentally
        // placing a literal private key/password in a file-path field is rejected.
        preflight_archive_secret_files(tool_name, &arguments)?;
        let task_id = self.next_task_id();
        let now = iso8601_now();
        let manifest_path = self.tasks_dir.join(format!("{task_id}.manifest.json"));
        let stdout_path = self.tasks_dir.join(format!("{task_id}.stdout.log"));
        let stderr_path = self.tasks_dir.join(format!("{task_id}.stderr.log"));
        let exit_path = self.tasks_dir.join(format!("{task_id}.exit.json"));
        let title = Some(format!("kalisto {}", tool.name.replace('.', " ")));
        let record = TaskRecord {
            task_id: task_id.clone(),
            title,
            status: TASK_STATUS_WORKING.to_owned(),
            status_message: Some("Starting Kalisto task.".to_owned()),
            progress: accepted_task_progress(),
            created_at: now.clone(),
            last_updated_at: now,
            ttl_millis: Some(DEFAULT_TASK_TTL_MILLIS),
            poll_interval_millis: DEFAULT_TASK_POLL_INTERVAL_MILLIS,
            request: TaskRequestRecord {
                tool_name: tool_name.to_owned(),
                arguments: arguments.clone(),
            },
            result: None,
            manifest_path: manifest_path.display().to_string(),
            stdout_path: stdout_path.display().to_string(),
            stderr_path: stderr_path.display().to_string(),
            exit_path: exit_path.display().to_string(),
            child_id: None,
        };
        self.store_task(record.clone())?;
        self.notify_task(&record);

        let manifest = TaskRunnerManifest {
            tool_name: tool_name.to_owned(),
            arguments,
            default_project_root: self.default_project_root.clone(),
        };
        let manifest_content = serde_json::to_vec_pretty(&manifest).map_err(|error| {
            jsonrpc_error(
                -32000,
                format!("failed to serialize task manifest: {error}"),
            )
        })?;
        fs::write(&manifest_path, manifest_content).map_err(|source| {
            jsonrpc_error(-32000, format!("failed to persist task manifest: {source}"))
        })?;

        let program = std::env::current_exe()
            .map_err(|source| {
                jsonrpc_error(
                    -32000,
                    format!("failed to resolve current executable for task spawn: {source}"),
                )
            })?
            .display()
            .to_string();
        let args = vec![
            "mcp-task-runner".to_owned(),
            "--manifest".to_owned(),
            manifest_path.display().to_string(),
            "--exit-path".to_owned(),
            exit_path.display().to_string(),
        ];
        let execution = Arc::new(TaskExecution {
            child: Mutex::new(None),
            cancel_requested: AtomicBool::new(false),
        });
        self.executions
            .lock()
            .map_err(|_| jsonrpc_error(-32000, "task execution registry was poisoned".to_owned()))?
            .insert(task_id.clone(), Arc::clone(&execution));

        let mut command = Command::new(&program);
        command.args(&args);
        let stdout_file = fs::File::create(&stdout_path).map_err(|source| {
            jsonrpc_error(
                -32000,
                format!(
                    "failed to create task stdout log at {}: {source}",
                    stdout_path.display()
                ),
            )
        })?;
        let stderr_file = fs::File::create(&stderr_path).map_err(|source| {
            jsonrpc_error(
                -32000,
                format!(
                    "failed to create task stderr log at {}: {source}",
                    stderr_path.display()
                ),
            )
        })?;
        command.stdout(Stdio::from(stdout_file));
        command.stderr(Stdio::from(stderr_file));
        let child = command.spawn().map_err(|source| {
            jsonrpc_error(-32000, format!("failed to spawn `{program}`: {source}"))
        })?;
        let child_id = child.id();

        {
            let mut child_slot = execution.child.lock().map_err(|_| {
                jsonrpc_error(-32000, "task child handle mutex was poisoned".to_owned())
            })?;
            *child_slot = Some(child);
        }

        let updated_record = self.update_task(&task_id, |record| {
            record.child_id = Some(child_id);
            record.status_message = Some(format!("Started `{program}` with pid {child_id}."));
            record.progress = started_task_progress(&program, child_id);
        })?;
        self.notify_task(&updated_record);

        let server = Arc::clone(self);
        let task_id_for_thread = task_id.clone();
        let execution_for_thread = Arc::clone(&execution);
        thread::spawn(move || {
            loop {
                let terminal = server
                    .refresh_task_state(&task_id_for_thread, execution_for_thread.as_ref())
                    .ok()
                    .flatten()
                    .map(|record| record.is_terminal())
                    .unwrap_or(false);
                if terminal {
                    if let Ok(mut executions) = server.executions.lock() {
                        executions.remove(&task_id_for_thread);
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(200));
            }
        });

        Ok(updated_record)
    }

    fn finalize_task(&self, task_id: &str, tool_name: &str, outcome: TaskCompletion) {
        let prior_record = self.task(task_id);
        let already_cancelled = prior_record
            .as_ref()
            .map(|record| record.status == TASK_STATUS_CANCELLED)
            .unwrap_or(false);
        let guidance = self.task(task_id).and_then(|record| {
            linux_firecracker_sudo_guidance_from_args(&record.request.arguments)
        });
        let result = match (outcome.exit_code, outcome.exit_error) {
            _ if outcome.cancelled || already_cancelled => {
                tool_execution_error_result(&format!("task `{task_id}` was cancelled"), None)
            }
            (Some(0), None) if !(outcome.cancelled || already_cancelled) => complete_child_result(
                tool_name,
                task_id,
                outcome.stdout.clone(),
                outcome.stderr.clone(),
            ),
            (code, maybe_error) => tool_execution_error_result(
                &format!(
                    "`kalisto` task `{tool_name}` failed with exit code {:?}{}\nstdout:\n{}\nstderr:\n{}",
                    code,
                    maybe_error
                        .as_deref()
                        .map(|error| format!("\nerror:\n{error}"))
                        .unwrap_or_default(),
                    outcome.stdout,
                    outcome.stderr
                ),
                guidance,
            ),
        };

        let status = if outcome.cancelled || already_cancelled {
            TASK_STATUS_CANCELLED
        } else if result_is_error(&result) {
            TASK_STATUS_FAILED
        } else {
            TASK_STATUS_COMPLETED
        };
        if let Ok(record) = self.update_task(task_id, |record| {
            let preserve_cancelled = record.status == TASK_STATUS_CANCELLED;
            record.status = status.to_owned();
            record.status_message = Some(match status {
                TASK_STATUS_COMPLETED => "Kalisto task completed successfully.".to_owned(),
                TASK_STATUS_CANCELLED => "Kalisto task was cancelled.".to_owned(),
                _ => "Kalisto task finished with an error result.".to_owned(),
            });
            record.progress = terminal_task_progress(status);
            if !preserve_cancelled {
                record.result = Some(attach_related_task_meta(result, task_id, None));
            }
            record.child_id = None;
        }) {
            self.notify_task(&record);
        }
    }

    fn refresh_task_state(
        &self,
        task_id: &str,
        execution: &TaskExecution,
    ) -> Result<Option<TaskRecord>> {
        let Some(record) = self.task(task_id) else {
            return Ok(None);
        };
        if record.is_terminal() {
            return Ok(Some(record));
        }

        if execution.cancel_requested.load(Ordering::SeqCst)
            && let Ok(mut child_slot) = execution.child.lock()
            && let Some(child) = child_slot.as_mut()
        {
            let _ = child.kill();
        }

        let stdout_path = PathBuf::from(&record.stdout_path);
        let stderr_path = PathBuf::from(&record.stderr_path);
        let exit_path = PathBuf::from(&record.exit_path);
        if let Some(exit_record) = load_task_exit_record(&exit_path)? {
            let stdout = read_log_content(&stdout_path)?;
            let stderr = read_log_content(&stderr_path)?;
            self.finalize_task(
                task_id,
                &record.request.tool_name,
                TaskCompletion {
                    stdout,
                    stderr,
                    exit_code: exit_record.exit_code,
                    exit_error: exit_record.error,
                    cancelled: record.status == TASK_STATUS_CANCELLED,
                },
            );
            return Ok(self.task(task_id));
        }

        if let Some(last_detail) = latest_task_log_detail(&stdout_path, &stderr_path)? {
            let updated = self
                .update_task(task_id, |current| {
                    if current.status == TASK_STATUS_WORKING
                        && current.status_message.as_deref() != Some(last_detail.as_str())
                    {
                        current.status_message = Some(last_detail.clone());
                        current.progress = running_task_progress(Some(last_detail.clone()));
                    }
                })
                .map_err(jsonrpc_to_error)?;
            if updated.status_message.as_deref() == Some(last_detail.as_str()) {
                self.notify_task(&updated);
            }
            return Ok(Some(updated));
        }

        Ok(Some(record))
    }

    fn handle_task_get(&self, id: Value, params: Value) -> Result<()> {
        let params: TaskLookupParams = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return self.write_error(id, error.code, error.message),
        };
        let execution = self
            .executions
            .lock()
            .ok()
            .and_then(|executions| executions.get(&params.task_id).cloned())
            .unwrap_or_else(|| Arc::new(TaskExecution::detached()));
        let Some(record) = self.refresh_task_state(&params.task_id, execution.as_ref())? else {
            return self.write_error(id, -32602, format!("unknown task `{}`", params.task_id));
        };
        self.write_result(id, record.public_view())
    }

    fn handle_task_list(&self, id: Value, params: Value) -> Result<()> {
        let params: TaskListParams = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return self.write_error(id, error.code, error.message),
        };
        let limit = params.limit.unwrap_or(DEFAULT_TASK_LIST_LIMIT).max(1);
        let tasks = self.tasks.lock().map_err(poisoned("task registry"))?;
        let mut all = tasks.values().cloned().collect::<Vec<_>>();
        all.sort_by(|left, right| left.task_id.cmp(&right.task_id));
        let start_index = match params.cursor.as_deref() {
            Some(cursor) => all
                .iter()
                .position(|task| task.task_id == cursor)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    Error::InvalidArgs(format!("unknown task cursor `{cursor}` for tasks/list"))
                })?,
            None => 0,
        };
        let slice = all
            .iter()
            .skip(start_index)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next_cursor = if start_index + slice.len() < all.len() {
            slice.last().map(|task| task.task_id.clone())
        } else {
            None
        };
        self.write_result(
            id,
            json!({
                "tasks": slice.into_iter().map(|task| task.public_view()).collect::<Vec<_>>(),
                "nextCursor": next_cursor
            }),
        )
    }

    fn handle_task_result(&self, id: Value, params: Value) -> Result<()> {
        let params: TaskLookupParams = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return self.write_error(id, error.code, error.message),
        };
        let record = match self.wait_for_terminal_task(&params.task_id) {
            Ok(record) => record,
            Err(Error::InvalidArgs(message)) => return self.write_error(id, -32602, message),
            Err(error) => return Err(error),
        };
        let result = record.result.unwrap_or_else(|| {
            tool_execution_error_result("task did not persist a result payload", None)
        });
        self.write_result(id, attach_related_task_meta(result, &params.task_id, None))
    }

    fn handle_task_cancel(&self, id: Value, params: Value) -> Result<()> {
        let params: TaskLookupParams = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return self.write_error(id, error.code, error.message),
        };
        let Some(record) = self.task(&params.task_id) else {
            return self.write_error(id, -32602, format!("unknown task `{}`", params.task_id));
        };
        if matches!(
            record.request.tool_name.as_str(),
            "clusters.export" | "clusters.import"
        ) {
            return self.write_error(
                id,
                -32602,
                "archive tasks cannot be cancelled while copying/publishing; wait for completion"
                    .to_owned(),
            );
        }
        if record.is_terminal() {
            return self.write_error(
                id,
                -32602,
                format!("task `{}` is already terminal", params.task_id),
            );
        }

        let execution = self
            .executions
            .lock()
            .ok()
            .and_then(|executions| executions.get(&params.task_id).cloned());
        let detached_cleanup_note = if execution.is_none() {
            match cancel_task_from_cluster_state(&record) {
                Ok(true) => Some(
                    "Kalisto task was cancelled and detached cluster resources were stopped from persisted state."
                        .to_owned(),
                ),
                Ok(false) => Some(
                    "Kalisto task was cancelled. No persisted cluster resources were available for detached cleanup."
                        .to_owned(),
                ),
                Err(error) => {
                    return self.write_error(
                        id,
                        -32000,
                        format!("failed detached task cleanup for `{}`: {error}", params.task_id),
                    )
                }
            }
        } else {
            None
        };

        let updated = match self.update_task(&params.task_id, |record| {
            record.status = TASK_STATUS_CANCELLED.to_owned();
            let message = detached_cleanup_note
                .clone()
                .unwrap_or_else(|| "Kalisto task was cancelled.".to_owned());
            record.status_message = Some(message.clone());
            record.progress = terminal_task_progress(TASK_STATUS_CANCELLED);
            record.progress.detail = Some(message);
            record.result = Some(attach_related_task_meta(
                tool_execution_error_result(
                    &format!("task `{}` was cancelled", params.task_id),
                    None,
                ),
                &params.task_id,
                None,
            ));
            record.child_id = None;
        }) {
            Ok(updated) => updated,
            Err(error) => return self.write_error(id, error.code, error.message),
        };

        if let Some(execution) = execution.as_ref() {
            execution.cancel_requested.store(true, Ordering::SeqCst);
            if let Ok(mut child_slot) = execution.child.lock()
                && let Some(child) = child_slot.as_mut()
            {
                let _ = child.kill();
            }
        }

        self.notify_task(&updated);
        self.write_result(id, updated.public_view())
    }

    fn wait_for_terminal_task(&self, task_id: &str) -> Result<TaskRecord> {
        let execution = self
            .executions
            .lock()
            .ok()
            .and_then(|executions| executions.get(task_id).cloned())
            .unwrap_or_else(|| Arc::new(TaskExecution::detached()));
        loop {
            let Some(record) = self.refresh_task_state(task_id, execution.as_ref())? else {
                return Err(Error::InvalidArgs(format!("unknown task `{task_id}`")));
            };
            if record.is_terminal() {
                return Ok(record);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn next_task_id(&self) -> String {
        format!(
            "task-{}-{}-{}",
            std::process::id(),
            unix_timestamp_millis(),
            self.next_task_counter.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn store_task(&self, record: TaskRecord) -> std::result::Result<(), JsonRpcError> {
        {
            let mut tasks = self.tasks.lock().map_err(|_| {
                jsonrpc_error(-32000, "task registry mutex was poisoned".to_owned())
            })?;
            tasks.insert(record.task_id.clone(), record.clone());
        }
        self.persist_task(&record)
            .map_err(|error| jsonrpc_error(-32000, error.to_string()))
    }

    fn update_task<F>(
        &self,
        task_id: &str,
        update: F,
    ) -> std::result::Result<TaskRecord, JsonRpcError>
    where
        F: FnOnce(&mut TaskRecord),
    {
        let record = {
            let mut tasks = self.tasks.lock().map_err(|_| {
                jsonrpc_error(-32000, "task registry mutex was poisoned".to_owned())
            })?;
            let record = tasks
                .get_mut(task_id)
                .ok_or_else(|| jsonrpc_error(-32602, format!("unknown task `{task_id}`")))?;
            update(record);
            record.last_updated_at = iso8601_now();
            record.clone()
        };
        self.persist_task(&record)
            .map_err(|error| jsonrpc_error(-32000, error.to_string()))?;
        Ok(record)
    }

    fn task(&self, task_id: &str) -> Option<TaskRecord> {
        self.tasks
            .lock()
            .ok()
            .and_then(|tasks| tasks.get(task_id).cloned())
    }

    fn persist_task(&self, record: &TaskRecord) -> Result<()> {
        let path = self.tasks_dir.join(format!("{}.json", record.task_id));
        let content = serde_json::to_vec_pretty(record).map_err(|error| {
            Error::InvalidArgs(format!(
                "failed to serialize task `{}`: {error}",
                record.task_id
            ))
        })?;
        fs::write(path, content).map_err(io_error("write"))
    }

    fn notify_task(&self, record: &TaskRecord) {
        let _ = self.write_message(&json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": "notifications/tasks/status",
            "params": record.public_view()
        }));
    }

    fn write_result(&self, id: Value, result: Value) -> Result<()> {
        self.write_message(&json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "result": result
        }))
    }

    fn write_error(&self, id: Value, code: i64, message: String) -> Result<()> {
        self.write_message(&json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "error": {
                "code": code,
                "message": message
            }
        }))
    }

    fn write_message(&self, message: &Value) -> Result<()> {
        let _guard = self
            .writer
            .lock()
            .map_err(|_| Error::InvalidArgs("stdout writer mutex was poisoned".to_owned()))?;
        let mut stdout = io::stdout().lock();
        write_stdio_message(
            &mut stdout,
            message,
            self.framing_mode()?
                .unwrap_or(FramingMode::NewlineDelimited),
        )
    }

    fn framing_mode(&self) -> Result<Option<FramingMode>> {
        self.framing
            .lock()
            .map(|mode| *mode)
            .map_err(|_| Error::InvalidArgs("stdio framing mutex was poisoned".to_owned()))
    }

    fn set_framing_mode(&self, mode: FramingMode) -> Result<()> {
        let mut framing = self
            .framing
            .lock()
            .map_err(|_| Error::InvalidArgs("stdio framing mutex was poisoned".to_owned()))?;
        *framing = Some(mode);
        Ok(())
    }

    fn is_initialized(&self) -> bool {
        self.initialized.lock().map(|state| *state).unwrap_or(false)
    }

    fn set_initialized(&self, initialized: bool) {
        if let Ok(mut state) = self.initialized.lock() {
            *state = initialized;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FramingMode {
    ContentLength,
    NewlineDelimited,
}

fn read_stdio_message(
    reader: &mut dyn BufRead,
    framing: &mut Option<FramingMode>,
) -> Result<Option<Value>> {
    let framing_mode = match framing {
        Some(mode) => *mode,
        None => match detect_stdio_framing(reader)? {
            Some(mode) => mode,
            None => return Ok(None),
        },
    };
    *framing = Some(framing_mode);

    match framing_mode {
        FramingMode::ContentLength => read_content_length_message(reader),
        FramingMode::NewlineDelimited => read_newline_message(reader),
    }
}

fn detect_stdio_framing(reader: &mut dyn BufRead) -> Result<Option<FramingMode>> {
    loop {
        let buffer = reader.fill_buf().map_err(io_error("stdin"))?;
        if buffer.is_empty() {
            return Ok(None);
        }
        match buffer[0] {
            b'{' => return Ok(Some(FramingMode::NewlineDelimited)),
            b'C' | b'c' => return Ok(Some(FramingMode::ContentLength)),
            b'\n' | b'\r' | b' ' | b'\t' => reader.consume(1),
            _ => return Ok(Some(FramingMode::NewlineDelimited)),
        }
    }
}

fn read_content_length_message(reader: &mut dyn BufRead) -> Result<Option<Value>> {
    let mut content_length = None;

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).map_err(io_error("stdin"))?;
        if bytes == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            content_length = Some(value.trim().parse::<usize>().map_err(|error| {
                Error::InvalidArgs(format!("invalid Content-Length header: {error}"))
            })?);
        }
    }

    let length = content_length
        .ok_or_else(|| Error::InvalidArgs("missing Content-Length header".to_owned()))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(io_error("stdin"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| Error::InvalidArgs(format!("failed to parse MCP JSON message: {error}")))
}

fn read_newline_message(reader: &mut dyn BufRead) -> Result<Option<Value>> {
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).map_err(io_error("stdin"))?;
        if bytes == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str(trimmed)
            .map_err(|error| Error::InvalidArgs(format!("parse error: {error}")))?;
        return Ok(Some(value));
    }
}

fn write_stdio_message(
    writer: &mut dyn Write,
    message: &Value,
    framing: FramingMode,
) -> Result<()> {
    let body = serde_json::to_vec(message)
        .map_err(|error| Error::InvalidArgs(format!("failed to serialize MCP message: {error}")))?;
    match framing {
        FramingMode::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", body.len()).map_err(io_error("stdout"))?;
            writer.write_all(&body).map_err(io_error("stdout"))?;
        }
        FramingMode::NewlineDelimited => {
            writer.write_all(&body).map_err(io_error("stdout"))?;
            writer.write_all(b"\n").map_err(io_error("stdout"))?;
        }
    }
    writer.flush().map_err(io_error("stdout"))
}

fn is_persisted_task_record_path(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    file_name.ends_with(".json")
        && !file_name.ends_with(".manifest.json")
        && !file_name.ends_with(".exit.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskSupport {
    Forbidden,
    Required,
}

#[derive(Debug, Clone, Copy)]
struct ToolSpec {
    name: &'static str,
    description: &'static str,
    task_support: TaskSupport,
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "clusters.list",
        description: "List known Kalisto clusters and their node status.",
        task_support: TaskSupport::Forbidden,
    },
    ToolSpec {
        name: "clusters.status",
        description: "Inspect the recorded state and backend status for a specific Kalisto cluster.",
        task_support: TaskSupport::Forbidden,
    },
    ToolSpec {
        name: "clusters.stats",
        description: "Load host-side resource and network metrics for a specific Kalisto cluster.",
        task_support: TaskSupport::Forbidden,
    },
    ToolSpec {
        name: "clusters.state",
        description: "Load the persisted Kalisto cluster definition and backend/runtime details from disk.",
        task_support: TaskSupport::Forbidden,
    },
    ToolSpec {
        name: "clusters.kubeconfig",
        description: "Return the persisted kubeconfig path for a specific Kalisto cluster.",
        task_support: TaskSupport::Forbidden,
    },
    ToolSpec {
        name: "clusters.logs",
        description: "Read locally persisted node log files for a specific cluster node, optionally limited to the last tailLines lines of each section for incremental polling.",
        task_support: TaskSupport::Forbidden,
    },
    ToolSpec {
        name: "workloads.apply",
        description: "Apply a Kubernetes workload manifest file to a running Kalisto cluster through the control plane's kubectl. The cluster must already be up.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "clusters.create",
        description: "Create Kalisto cluster state and backend networking. On Linux with the firecracker backend, run the MCP server itself with sudo only when using `--network bridge`; the default firecracker mode is rootless `slirp4netns`.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "clusters.pull",
        description: "Prepare managed Lima tools, a sanitized node image, K3s and optional Cilium assets. May use a temporary OS builder VM, but creates no cluster VMs or cluster state. Match the lifecycle selection used for clusters.up.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "clusters.export",
        description: "Export a trusted cold Lima/VZ cluster archive containing credentials and VM-local application data. Requires stopped nodes unless stop=true authorizes clean shutdown. Never overwrites output; leaves nodes stopped. Select recipients or a private passphraseFile for native age encryption. Archive tasks are not cancellable.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "clusters.import",
        description: "Verify and restore a trusted cold Lima/VZ archive with original identity and compatible architecture. Refuses existing cluster/VM destinations, leaves restored nodes stopped, and does not execute archived host configuration. May prepare verified host tools. Contains secrets; age decryption accepts identityFiles or a private passphraseFile, never inline secrets. Archive tasks are not cancellable.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "clusters.up",
        description: "Create the cluster if needed and boot the control-plane and worker nodes. First prepare matching assets with clusters.pull. Default CNI is none; select addons [\"cni/cilium\"] for pod networking. On Linux with the firecracker backend, run the MCP server itself with sudo only when using `--network bridge`; the default firecracker mode is rootless `slirp4netns`.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "clusters.down",
        description: "Stop all nodes for a specific Kalisto cluster.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "clusters.destroy",
        description: "Destroy a Kalisto cluster and remove its persisted state.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "clusters.scale",
        description: "Scale a running Kalisto cluster up by adding workers.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "addons.list",
        description: "List the built-in Kalisto add-on catalog, optionally with install status for a specific cluster.",
        task_support: TaskSupport::Forbidden,
    },
    ToolSpec {
        name: "addons.install",
        description: "Install a built-in Kalisto add-on onto a cluster.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "bench.run",
        description: "Run the Kalisto lifecycle benchmark for one or more iterations.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "probes.macos.lima",
        description: "Run the host macOS Lima prerequisite probe.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "probes.macos.lima.k3s",
        description: "Run the host macOS Lima k3s live probe.",
        task_support: TaskSupport::Required,
    },
];

fn tool_definitions() -> Vec<Value> {
    TOOLS.iter().map(tool_definition).collect()
}

fn tool_definition(tool: &ToolSpec) -> Value {
    let mut definition = json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": input_schema(tool.name),
    });
    if let Some(object) = definition.as_object_mut() {
        let task_support = match tool.task_support {
            TaskSupport::Forbidden => "forbidden",
            TaskSupport::Required => "required",
        };
        object.insert(
            "execution".to_owned(),
            json!({ "taskSupport": task_support }),
        );
        if matches!(
            tool.name,
            "clusters.list"
                | "clusters.status"
                | "clusters.stats"
                | "clusters.state"
                | "clusters.kubeconfig"
                | "clusters.logs"
                | "addons.list"
        ) {
            object.insert("annotations".to_owned(), json!({ "readOnlyHint": true }));
        }
        if matches!(tool.name, "clusters.down" | "clusters.destroy") {
            object.insert("annotations".to_owned(), json!({ "destructiveHint": true }));
        }
    }
    definition
}

fn input_schema(tool_name: &str) -> Value {
    match tool_name {
        "clusters.list" => empty_object_schema(),
        "clusters.status" | "clusters.stats" | "clusters.state" | "clusters.kubeconfig" => {
            named_cluster_schema()
        }
        "clusters.logs" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "node": { "type": "string" },
                "tailLines": { "type": "integer", "minimum": 1 }
            },
            "required": ["name", "node"],
            "additionalProperties": false
        }),
        "workloads.apply" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "manifestPath": { "type": "string" }
            },
            "required": ["name", "manifestPath"],
            "additionalProperties": false
        }),
        "clusters.down" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "force": { "type": "boolean" }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        "clusters.destroy" => named_cluster_schema(),
        "clusters.scale" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "workers": { "type": "integer", "minimum": 0 }
            },
            "required": ["name", "workers"],
            "additionalProperties": false
        }),
        "clusters.export" => {
            json!({"type":"object","properties":{
                "name":{"type":"string"},"output":{"type":"string"},"stop":{"type":"boolean","default":false},
                "recipients":{"type":"array","items":{"type":"string"},"maxItems":64,"description":"Native age1... public recipients, mutually exclusive with passphraseFile"},
                "passphraseFile":{"type":"string","description":"Private mode-0600 host file; never provide a raw passphrase"}
            },"required":["name","output"],"additionalProperties":false})
        }
        "clusters.import" => {
            json!({"type":"object","properties":{
                "input":{"type":"string"},"apiPort":{"type":"integer","minimum":1,"maximum":65535},"maxBytes":{"type":"integer","minimum":1},
                "identityFiles":{"type":"array","items":{"type":"string"},"maxItems":64,"description":"Private native age identity files on host, mutually exclusive with passphraseFile"},
                "passphraseFile":{"type":"string","description":"Private mode-0600 host file; never provide a raw passphrase"}
            },"required":["input"],"additionalProperties":false})
        }
        "clusters.create" | "clusters.pull" | "clusters.up" => lifecycle_schema(),
        "addons.list" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "additionalProperties": false
        }),
        "addons.install" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "addon": { "type": "string" }
            },
            "required": ["name", "addon"],
            "additionalProperties": false
        }),
        "bench.run" => json!({
            "type": "object",
            "properties": lifecycle_properties().into_iter().chain([
                ("runs".to_owned(), json!({ "type": "integer", "minimum": 1 }))
            ]).collect::<Map<String, Value>>(),
            "additionalProperties": false
        }),
        "probes.macos.lima" => json!({
            "type": "object",
            "properties": {
                "workspaceRoot": { "type": "string" },
                "template": { "type": "string" },
                "network": { "type": "string" },
                "keepResources": { "type": "boolean" },
                "noRosetta": { "type": "boolean" }
            },
            "required": ["workspaceRoot"],
            "additionalProperties": false
        }),
        "probes.macos.lima.k3s" => json!({
            "type": "object",
            "properties": {
                "workspaceRoot": { "type": "string" },
                "template": { "type": "string" },
                "workers": { "type": "integer", "minimum": 0 },
                "network": { "type": "string" },
                "apiPort": { "type": "integer", "minimum": 1, "maximum": 65535 },
                "k3sVersion": { "type": "string" },
                "agentless": { "type": "boolean" },
                "noRosetta": { "type": "boolean" },
                "keepResources": { "type": "boolean" }
            },
            "required": ["workspaceRoot"],
            "additionalProperties": false
        }),
        _ => empty_object_schema(),
    }
}

fn lifecycle_schema() -> Value {
    json!({
        "type": "object",
        "properties": lifecycle_properties(),
        "additionalProperties": false
    })
}

fn lifecycle_properties() -> Map<String, Value> {
    [
        ("projectRoot".to_owned(), json!({ "type": "string" })),
        ("name".to_owned(), json!({ "type": "string" })),
        (
            "backend".to_owned(),
            json!({ "type": "string", "enum": ["firecracker", "lima"] }),
        ),
        ("template".to_owned(), json!({ "type": "string" })),
        (
            "arch".to_owned(),
            json!({ "type": "string", "enum": ["amd64", "x86_64", "arm64", "aarch64"] }),
        ),
        ("firecrackerKernel".to_owned(), json!({ "type": "string" })),
        ("firecrackerRootfs".to_owned(), json!({ "type": "string" })),
        (
            "workers".to_owned(),
            json!({ "type": "integer", "minimum": 0 }),
        ),
        (
            "controlPlaneCpus".to_owned(),
            json!({ "type": "integer", "minimum": 1 }),
        ),
        (
            "controlPlaneMemoryGib".to_owned(),
            json!({ "type": "integer", "minimum": 1 }),
        ),
        (
            "workerCpus".to_owned(),
            json!({ "type": "integer", "minimum": 1 }),
        ),
        (
            "workerMemoryGib".to_owned(),
            json!({ "type": "integer", "minimum": 1 }),
        ),
        ("network".to_owned(), json!({ "type": "string" })),
        (
            "apiPort".to_owned(),
            json!({ "type": "integer", "minimum": 1, "maximum": 65535 }),
        ),
        ("k3sVersion".to_owned(), json!({ "type": "string" })),
        ("ciliumVersion".to_owned(), json!({ "type": "string" })),
        (
            "addons".to_owned(),
            json!({
                "type": "array",
                "items": { "type": "string" }
            }),
        ),
        ("keepFailedRuntime".to_owned(), json!({ "type": "boolean" })),
        ("agentless".to_owned(), json!({ "type": "boolean" })),
        ("noRosetta".to_owned(), json!({ "type": "boolean" })),
    ]
    .into_iter()
    .collect()
}

fn named_cluster_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

fn tool_by_name(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|tool| tool.name == name)
}

fn execute_direct_tool(
    tool_name: &str,
    arguments: Value,
    _default_project_root: Option<PathBuf>,
) -> std::result::Result<Value, JsonRpcError> {
    match tool_name {
        "clusters.list" => {
            parse_tool_args::<EmptyArgs>(arguments)?;
            let clusters = cluster_list().map_err(cluster_error_result)?;
            Ok(success_tool_result(
                format!("Listed {} cluster(s).", clusters.len()),
                json!({
                    "clusters": clusters.iter().map(cluster_status_payload).collect::<Vec<_>>()
                }),
            ))
        }
        "clusters.status" => {
            let args: NamedClusterArgs = parse_tool_args(arguments)?;
            let status = cluster_status(&args.name).map_err(cluster_error_result)?;
            Ok(success_tool_result(
                format!("Loaded status for cluster `{}`.", status.name),
                json!({ "cluster": cluster_status_payload(&status) }),
            ))
        }
        "clusters.state" => {
            let args: NamedClusterArgs = parse_tool_args(arguments)?;
            let state = load_cluster_state(&args.name).map_err(cluster_error_result)?;
            Ok(success_tool_result(
                format!("Loaded persisted state for cluster `{}`.", state.name),
                json!({ "cluster": cluster_state_payload(&state) }),
            ))
        }
        "clusters.stats" => {
            let args: NamedClusterArgs = parse_tool_args(arguments)?;
            let stats = cluster_stats(&args.name).map_err(cluster_error_result)?;
            Ok(success_tool_result(
                format!("Loaded stats for cluster `{}`.", stats.name),
                json!({ "cluster": cluster_stats_payload(&stats) }),
            ))
        }
        "clusters.kubeconfig" => {
            let args: NamedClusterArgs = parse_tool_args(arguments)?;
            let state_root = crate::cluster::spec::ClusterSpec::state_root_for(&args.name)
                .map_err(cluster_error_result)?;
            let path = ClusterState::load(&state_root)
                .map_err(cluster_error_result)?
                .kubeconfig_path;
            Ok(success_tool_result(
                format!("Resolved kubeconfig for cluster `{}`.", args.name),
                json!({
                    "name": args.name,
                    "kubeconfigPath": path.display().to_string()
                }),
            ))
        }
        "clusters.logs" => {
            let args: ClusterLogsArgs = parse_tool_args(arguments)?;
            let logs = crate::cluster::manager::logs(&args.name, &args.node)
                .map_err(cluster_error_result)?;
            let sections = logs
                .sections
                .iter()
                .map(|section| {
                    let content = match args.tail_lines {
                        Some(limit) => tail_lines(&section.content, limit),
                        None => section.content.clone(),
                    };
                    json!({
                        "label": section.label,
                        "path": section.path.display().to_string(),
                        "content": content
                    })
                })
                .collect::<Vec<_>>();
            Ok(success_tool_result(
                format!(
                    "Loaded {} log section(s) for node `{}` in cluster `{}`.",
                    logs.sections.len(),
                    logs.node,
                    logs.cluster
                ),
                json!({
                    "cluster": logs.cluster,
                    "node": logs.node,
                    "sections": sections
                }),
            ))
        }
        "addons.list" => {
            let args: OptionalClusterArgs = parse_tool_args(arguments)?;
            let cluster_state = args
                .name
                .as_deref()
                .map(load_cluster_state)
                .transpose()
                .map_err(cluster_error_result)?;
            let catalog = built_in_catalog().map_err(cluster_error_result)?;
            Ok(success_tool_result(
                format!("Loaded {} add-on(s).", catalog.len()),
                json!({
                    "cluster": args.name,
                    "addons": catalog.iter().map(|plugin| addon_payload(plugin, cluster_state.as_ref())).collect::<Vec<_>>()
                }),
            ))
        }
        _ => Err(jsonrpc_error(
            -32601,
            format!("tool `{tool_name}` only supports task-backed execution"),
        )),
    }
}

fn tail_lines(content: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(limit);
    let mut result = lines[start..].join("\n");
    if content.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    result
}

fn cluster_status_payload(status: &crate::cluster::manager::ClusterStatus) -> Value {
    json!({
        "name": status.name,
        "stateRoot": status.state_root.display().to_string(),
        "kubeconfigPath": status.kubeconfig_path.display().to_string(),
        "nodes": status.nodes.iter().map(|node| json!({
            "name": node.name,
            "role": format!("{:?}", node.role).to_ascii_lowercase(),
            "backendStatus": node.backend_status
        })).collect::<Vec<_>>()
    })
}

fn cluster_stats_payload(stats: &crate::cluster::manager::ClusterStats) -> Value {
    json!({
        "name": stats.name,
        "stateRoot": stats.state_root.display().to_string(),
        "kubeconfigPath": stats.kubeconfig_path.display().to_string(),
        "nodes": stats.nodes.iter().map(|node| json!({
            "name": node.name,
            "role": format!("{:?}", node.role).to_ascii_lowercase(),
            "backendStatus": node.backend_status,
            "configuredCpus": node.configured_cpus,
            "configuredMemoryGib": node.configured_memory_gib,
            "pid": node.pid,
            "guestIp": node.guest_ip,
            "rssBytes": node.rss_bytes,
            "vszBytes": node.vsz_bytes,
            "rootfsBytes": node.rootfs_bytes,
            "rxBytes": node.rx_bytes,
            "txBytes": node.tx_bytes
        })).collect::<Vec<_>>()
    })
}

fn cluster_state_payload(state: &ClusterState) -> Value {
    serde_json::to_value(ClusterStateView::from_state(state))
        .expect("cluster state payload should serialize")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterStateView {
    name: String,
    backend: String,
    workspace_root: String,
    template: String,
    firecracker_kernel: Option<String>,
    firecracker_rootfs: Option<String>,
    api_port: u16,
    worker_count: usize,
    control_plane_resources: NodeResourcesView,
    worker_resources: NodeResourcesView,
    enable_rosetta: bool,
    network: String,
    k3s_version: String,
    cni_plugin: String,
    cilium_version: String,
    addons: Vec<String>,
    metallb: MetalLbConfigView,
    server_mode: String,
    component_toggles: ComponentTogglesView,
    kubeconfig_path: String,
    token_path: String,
    nodes: Vec<ClusterNodeView>,
}

impl ClusterStateView {
    fn from_state(state: &ClusterState) -> Self {
        Self {
            name: state.name.clone(),
            backend: state.backend.as_str().to_owned(),
            workspace_root: state.workspace_root.display().to_string(),
            template: state.template.clone(),
            firecracker_kernel: state
                .firecracker_kernel
                .as_ref()
                .map(|path| path.display().to_string()),
            firecracker_rootfs: state
                .firecracker_rootfs
                .as_ref()
                .map(|path| path.display().to_string()),
            api_port: state.api_port,
            worker_count: state.worker_count,
            control_plane_resources: NodeResourcesView::from_resources(
                &state.control_plane_resources,
            ),
            worker_resources: NodeResourcesView::from_resources(&state.worker_resources),
            enable_rosetta: state.enable_rosetta,
            network: state.network.clone(),
            k3s_version: state.k3s_version.clone(),
            cni_plugin: state.cni_plugin.as_str().to_owned(),
            cilium_version: state.cilium_version.clone(),
            addons: state.addons.clone(),
            metallb: MetalLbConfigView::from_config(&state.metallb),
            server_mode: match state.server_mode {
                ServerMode::Standard => "standard".to_owned(),
                ServerMode::Agentless => "agentless".to_owned(),
            },
            component_toggles: ComponentTogglesView::from_state(state),
            kubeconfig_path: state.kubeconfig_path.display().to_string(),
            token_path: state.token_path.display().to_string(),
            nodes: state.nodes.iter().map(ClusterNodeView::from_node).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeResourcesView {
    cpus: u8,
    memory_gib: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetalLbConfigView {
    mode: Option<String>,
    addresses: Vec<String>,
    config_file: Option<String>,
}

impl MetalLbConfigView {
    fn from_config(config: &MetalLbConfig) -> Self {
        Self {
            mode: config.mode.map(MetalLbMode::as_str).map(ToOwned::to_owned),
            addresses: config.addresses.clone(),
            config_file: config
                .config_file
                .as_ref()
                .map(|path| path.display().to_string()),
        }
    }
}

impl NodeResourcesView {
    fn from_resources(resources: &crate::cluster::spec::NodeResources) -> Self {
        Self {
            cpus: resources.cpus,
            memory_gib: resources.memory_gib,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComponentTogglesView {
    disable_traefik: bool,
    disable_service_lb: bool,
    disable_metrics_server: bool,
    disable_local_storage: bool,
    disable_core_dns: bool,
    disable_network_policy: bool,
    disable_kube_proxy: bool,
    flannel_backend_none: bool,
}

impl ComponentTogglesView {
    fn from_state(state: &ClusterState) -> Self {
        Self {
            disable_traefik: state.component_toggles.disable_traefik,
            disable_service_lb: state.component_toggles.disable_servicelb,
            disable_metrics_server: state.component_toggles.disable_metrics_server,
            disable_local_storage: state.component_toggles.disable_local_storage,
            disable_core_dns: state.component_toggles.disable_coredns,
            disable_network_policy: state.component_toggles.disable_network_policy,
            disable_kube_proxy: state.component_toggles.disable_kube_proxy,
            flannel_backend_none: state.component_toggles.flannel_backend_none,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterNodeView {
    name: String,
    role: String,
    artifacts_root: String,
}

impl ClusterNodeView {
    fn from_node(node: &crate::cluster::state::NodeState) -> Self {
        Self {
            name: node.name.clone(),
            role: match node.role {
                crate::k3s::NodeRole::ControlPlane => "control-plane".to_owned(),
                crate::k3s::NodeRole::Worker => "worker".to_owned(),
            },
            artifacts_root: node.artifacts_root.display().to_string(),
        }
    }
}

fn addon_payload(plugin: &PluginSpec, state: Option<&ClusterState>) -> Value {
    json!({
        "id": plugin.id,
        "category": plugin.category,
        "installKind": plugin.install_kind,
        "description": plugin.description,
        "conflicts": plugin.conflicts,
        "status": plugin_status(state, plugin)
    })
}

fn plugin_status(state: Option<&ClusterState>, plugin: &PluginSpec) -> &'static str {
    let Some(state) = state else {
        return "available";
    };
    match plugin.id.as_str() {
        "cni/flannel" if state.cni_plugin == CniPlugin::Flannel => "installed",
        "cni/cilium" if state.cni_plugin == CniPlugin::Cilium => "installed",
        "lb/servicelb" if !state.component_toggles.disable_servicelb => "installed",
        _ if state.addons.iter().any(|installed| installed == &plugin.id) => "installed",
        _ => "available",
    }
}

fn load_cluster_state(name: &str) -> Result<ClusterState> {
    let state_root = crate::cluster::spec::ClusterSpec::state_root_for(name)?;
    ClusterState::load(&state_root)
}

fn success_tool_result(summary: String, structured_content: Value) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": summary
            }
        ],
        "structuredContent": structured_content,
        "isError": false
    })
}

fn tool_execution_error_result(message: &str, guidance: Option<String>) -> Value {
    let mut structured = Map::new();
    structured.insert("ok".to_owned(), Value::Bool(false));
    structured.insert("error".to_owned(), Value::String(message.to_owned()));
    if let Some(ref guidance) = guidance {
        structured.insert("hint".to_owned(), Value::String(guidance.clone()));
        structured.insert(
            "requiresSudoOnLinuxFirecracker".to_owned(),
            Value::Bool(true),
        );
    }
    let text = match guidance {
        Some(guidance) => format!("{message}\n\nHint: {guidance}"),
        None => message.to_owned(),
    };
    json!({
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "structuredContent": Value::Object(structured),
        "isError": true
    })
}

fn result_is_error(result: &Value) -> bool {
    result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn linux_firecracker_sudo_guidance_from_args(arguments: &Value) -> Option<String> {
    let backend = arguments
        .get("backend")
        .and_then(Value::as_str)
        .and_then(BackendKind::parse)
        .unwrap_or_else(BackendKind::host_default);
    let bridge_network_requested = arguments
        .get("network")
        .and_then(Value::as_str)
        .map(|network| network.trim().is_empty() || network.trim() == "bridge")
        .unwrap_or(false);
    if cfg!(target_os = "linux") && backend == BackendKind::Firecracker && bridge_network_requested
    {
        Some("This operation uses firecracker bridge networking on Linux. Run the MCP server itself with sudo, for example `sudo -E kalisto mcp`.".to_owned())
    } else {
        None
    }
}

fn attach_related_task_meta(mut result: Value, task_id: &str, message: Option<String>) -> Value {
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    let meta = object
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(meta_object) = meta.as_object_mut() {
        meta_object.insert(META_RELATED_TASK.to_owned(), json!({ "taskId": task_id }));
        if let Some(message) = message {
            meta_object.insert(META_IMMEDIATE_RESPONSE.to_owned(), Value::String(message));
        }
    }
    result
}

fn parse_tool_args<T: DeserializeOwned>(arguments: Value) -> std::result::Result<T, JsonRpcError> {
    let value = if arguments.is_null() {
        json!({})
    } else {
        arguments
    };
    serde_json::from_value(value)
        .map_err(|error| jsonrpc_error(-32602, format!("invalid tool arguments: {error}")))
}

fn parse_params<T: DeserializeOwned>(params: Value) -> std::result::Result<T, JsonRpcError> {
    serde_json::from_value(params)
        .map_err(|error| jsonrpc_error(-32602, format!("invalid params: {error}")))
}

fn cluster_error_result(error: Error) -> JsonRpcError {
    jsonrpc_error(-32000, error.to_string())
}

#[derive(Debug, Clone)]
struct JsonRpcError {
    code: i64,
    message: String,
}

fn jsonrpc_error(code: i64, message: String) -> JsonRpcError {
    JsonRpcError { code, message }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchiveExportPayload {
    name: String,
    output: PathBuf,
    #[serde(default)]
    stop: bool,
    #[serde(default)]
    recipients: Vec<String>,
    passphrase_file: Option<PathBuf>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchiveImportPayload {
    input: PathBuf,
    api_port: Option<u16>,
    max_bytes: Option<u64>,
    #[serde(default)]
    identity_files: Vec<PathBuf>,
    passphrase_file: Option<PathBuf>,
}

// Reject inline secrets and malformed recipients before persisting any task
// request/manifest. Do not echo rejected values through serde's error text.
fn validate_archive_arguments(
    name: &str,
    arguments: &Value,
) -> std::result::Result<(), JsonRpcError> {
    let rejected = || {
        jsonrpc_error(-32602, "invalid archive arguments; use native public recipients and private secret-file paths, never inline passwords/keys".into())
    };
    match name {
        "clusters.export" => {
            let request: ArchiveExportPayload =
                serde_json::from_value(arguments.clone()).map_err(|_| rejected())?;
            if request.recipients.len() > 64
                || (!request.recipients.is_empty() && request.passphrase_file.is_some())
                || request
                    .recipients
                    .iter()
                    .any(|key| key.parse::<age::x25519::Recipient>().is_err())
            {
                return Err(rejected());
            }
        }
        "clusters.import" => {
            let request: ArchiveImportPayload =
                serde_json::from_value(arguments.clone()).map_err(|_| rejected())?;
            if request.identity_files.len() > 64
                || (!request.identity_files.is_empty() && request.passphrase_file.is_some())
                || request.api_port == Some(0)
                || request.max_bytes == Some(0)
            {
                return Err(rejected());
            }
        }
        _ => {}
    }
    Ok(())
}

fn preflight_archive_secret_files(
    name: &str,
    arguments: &Value,
) -> std::result::Result<(), JsonRpcError> {
    let rejected = || {
        jsonrpc_error(-32602, "archive secret inputs must reference existing private files on the MCP host; never put secrets in file-path fields".into())
    };
    match name {
        "clusters.export" => {
            let request: ArchiveExportPayload =
                serde_json::from_value(arguments.clone()).map_err(|_| rejected())?;
            crate::archive::ExportEncryption::from_files(
                &request.recipients,
                request.passphrase_file.as_deref(),
            )
            .map_err(|_| rejected())?;
        }
        "clusters.import" => {
            let request: ArchiveImportPayload =
                serde_json::from_value(arguments.clone()).map_err(|_| rejected())?;
            crate::archive::ImportDecryption::from_files(
                &request.identity_files,
                request.passphrase_file.as_deref(),
            )
            .map_err(|_| rejected())?;
        }
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ToolCallRequest {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
    #[serde(default)]
    task: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct TaskLookupParams {
    #[serde(rename = "taskId")]
    task_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskListParams {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NamedClusterArgs {
    name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OptionalClusterArgs {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterLogsArgs {
    name: String,
    node: String,
    #[serde(default)]
    tail_lines: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkloadApplyArgs {
    name: String,
    manifest_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterDownArgs {
    name: String,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterScaleArgs {
    name: String,
    workers: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddonInstallArgsPayload {
    name: String,
    addon: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClusterLifecycleArgs {
    project_root: Option<PathBuf>,
    name: Option<String>,
    backend: Option<String>,
    template: Option<String>,
    arch: Option<String>,
    firecracker_kernel: Option<PathBuf>,
    firecracker_rootfs: Option<PathBuf>,
    workers: Option<usize>,
    control_plane_cpus: Option<u8>,
    control_plane_memory_gib: Option<u8>,
    worker_cpus: Option<u8>,
    worker_memory_gib: Option<u8>,
    network: Option<String>,
    api_port: Option<u16>,
    k3s_version: Option<String>,
    cilium_version: Option<String>,
    addons: Option<Vec<String>>,
    keep_failed_runtime: Option<bool>,
    agentless: Option<bool>,
    no_rosetta: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BenchArgsPayload {
    #[serde(flatten)]
    lifecycle: ClusterLifecycleArgs,
    runs: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeMacosLimaArgsPayload {
    workspace_root: PathBuf,
    template: Option<String>,
    network: Option<String>,
    keep_resources: Option<bool>,
    no_rosetta: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeMacosLimaK3sArgsPayload {
    workspace_root: PathBuf,
    template: Option<String>,
    workers: Option<usize>,
    network: Option<String>,
    api_port: Option<u16>,
    k3s_version: Option<String>,
    agentless: Option<bool>,
    keep_resources: Option<bool>,
    no_rosetta: Option<bool>,
}

impl ClusterLifecycleArgs {
    fn into_up_args(
        self,
        default_project_root: Option<PathBuf>,
    ) -> std::result::Result<UpArgs, JsonRpcError> {
        let backend = match self.backend.as_deref() {
            Some(value) => Some(parse_backend(value)?),
            None => None,
        };
        let image_arch = match self.arch.as_deref() {
            Some(value) => Some(parse_image_arch(value)?),
            None => None,
        };
        Ok(UpArgs {
            project_root: self.project_root.or(default_project_root),
            name: self.name,
            backend,
            template: self.template,
            image_arch,
            firecracker_kernel: self.firecracker_kernel,
            firecracker_rootfs: self.firecracker_rootfs,
            workers: self.workers,
            control_plane_cpus: self.control_plane_cpus,
            control_plane_memory_gib: self.control_plane_memory_gib,
            worker_cpus: self.worker_cpus,
            worker_memory_gib: self.worker_memory_gib,
            network: self.network,
            api_port: self.api_port,
            k3s_version: self.k3s_version,
            cilium_version: self.cilium_version,
            addons: self.addons.unwrap_or_default(),
            keep_failed_runtime: self.keep_failed_runtime.unwrap_or(false),
            stream_vm_logs: false,
            agentless: self.agentless.unwrap_or(false),
            no_rosetta: self.no_rosetta.unwrap_or(false),
        })
    }
}

impl BenchArgsPayload {
    fn into_bench_args(
        self,
        default_project_root: Option<PathBuf>,
    ) -> std::result::Result<BenchArgs, JsonRpcError> {
        let lifecycle = self.lifecycle;
        Ok(BenchArgs {
            project_root: lifecycle.project_root.or(default_project_root),
            name: lifecycle.name,
            backend: match lifecycle.backend.as_deref() {
                Some(value) => Some(parse_backend(value)?),
                None => None,
            },
            template: lifecycle.template,
            image_arch: match lifecycle.arch.as_deref() {
                Some(value) => Some(parse_image_arch(value)?),
                None => None,
            },
            firecracker_kernel: lifecycle.firecracker_kernel,
            firecracker_rootfs: lifecycle.firecracker_rootfs,
            workers: lifecycle.workers,
            control_plane_cpus: lifecycle.control_plane_cpus,
            control_plane_memory_gib: lifecycle.control_plane_memory_gib,
            worker_cpus: lifecycle.worker_cpus,
            worker_memory_gib: lifecycle.worker_memory_gib,
            network: lifecycle.network,
            api_port: lifecycle.api_port,
            k3s_version: lifecycle.k3s_version,
            cilium_version: lifecycle.cilium_version,
            addons: lifecycle.addons.unwrap_or_default(),
            keep_failed_runtime: lifecycle.keep_failed_runtime.unwrap_or(false),
            stream_vm_logs: false,
            agentless: lifecycle.agentless.unwrap_or(false),
            no_rosetta: lifecycle.no_rosetta.unwrap_or(false),
            runs: self.runs.unwrap_or(3),
        })
    }
}

fn parse_backend(value: &str) -> std::result::Result<BackendKind, JsonRpcError> {
    BackendKind::parse(value).ok_or_else(|| {
        jsonrpc_error(
            -32602,
            format!("unsupported backend `{value}`; expected one of: firecracker, lima"),
        )
    })
}

fn parse_image_arch(value: &str) -> std::result::Result<ImageArch, JsonRpcError> {
    ImageArch::parse(value).ok_or_else(|| {
        jsonrpc_error(
            -32602,
            format!("unsupported arch `{value}`; expected one of: amd64, x86_64, arm64, aarch64"),
        )
    })
}

#[cfg(test)]
fn task_command(
    tool_name: &str,
    arguments: &Value,
    default_project_root: Option<PathBuf>,
) -> std::result::Result<(String, Vec<String>), JsonRpcError> {
    let executable = std::env::current_exe().map_err(|source| {
        jsonrpc_error(
            -32000,
            format!("failed to resolve current executable for task spawn: {source}"),
        )
    })?;
    let program = executable.display().to_string();
    let mut args = Vec::new();
    match tool_name {
        "clusters.create" | "clusters.pull" | "clusters.up" => {
            let lifecycle: ClusterLifecycleArgs = parse_tool_args(arguments.clone())?;
            let up = lifecycle.into_up_args(default_project_root)?;
            args.push(
                match tool_name {
                    "clusters.create" => "create",
                    "clusters.pull" => "pull",
                    _ => "up",
                }
                .to_owned(),
            );
            append_up_args(&mut args, &up);
        }
        "clusters.export" => {
            let request: ArchiveExportPayload = parse_tool_args(arguments.clone())?;
            args.extend([
                "export".into(),
                "--name".into(),
                request.name,
                "--output".into(),
                request.output.display().to_string(),
            ]);
            if request.stop {
                args.push("--stop".into());
            }
            for key in request.recipients {
                args.extend(["--recipient".into(), key]);
            }
            if let Some(path) = request.passphrase_file {
                args.extend(["--passphrase-file".into(), path.display().to_string()]);
            }
        }
        "clusters.import" => {
            let request: ArchiveImportPayload = parse_tool_args(arguments.clone())?;
            args.extend(["import".into(), request.input.display().to_string()]);
            if let Some(port) = request.api_port {
                args.extend(["--api-port".into(), port.to_string()]);
            }
            if let Some(bytes) = request.max_bytes {
                args.extend(["--max-bytes".into(), bytes.to_string()]);
            }
            for path in request.identity_files {
                args.extend(["--identity".into(), path.display().to_string()]);
            }
            if let Some(path) = request.passphrase_file {
                args.extend(["--passphrase-file".into(), path.display().to_string()]);
            }
        }
        "clusters.down" => {
            let request: ClusterDownArgs = parse_tool_args(arguments.clone())?;
            args.extend(["down".to_owned(), "--name".to_owned(), request.name]);
            if request.force {
                args.push("--force".to_owned());
            }
        }
        "clusters.destroy" => {
            let request: NamedClusterArgs = parse_tool_args(arguments.clone())?;
            args.extend(["destroy".to_owned(), "--name".to_owned(), request.name]);
        }
        "clusters.scale" => {
            let request: ClusterScaleArgs = parse_tool_args(arguments.clone())?;
            args.extend([
                "scale".to_owned(),
                "--name".to_owned(),
                request.name,
                "--workers".to_owned(),
                request.workers.to_string(),
            ]);
        }
        "addons.install" => {
            let request: AddonInstallArgsPayload = parse_tool_args(arguments.clone())?;
            args.extend([
                "addon".to_owned(),
                "install".to_owned(),
                "--name".to_owned(),
                request.name,
                request.addon,
            ]);
        }
        "workloads.apply" => {
            let request: WorkloadApplyArgs = parse_tool_args(arguments.clone())?;
            args.extend([
                "apply".to_owned(),
                "--name".to_owned(),
                request.name,
                "--manifest".to_owned(),
                request.manifest_path.display().to_string(),
            ]);
        }
        "bench.run" => {
            let request: BenchArgsPayload = parse_tool_args(arguments.clone())?;
            let bench = request.into_bench_args(default_project_root)?;
            args.push("bench".to_owned());
            append_bench_args(&mut args, &bench);
        }
        "probes.macos.lima" => {
            let request: ProbeMacosLimaArgsPayload = parse_tool_args(arguments.clone())?;
            args.extend([
                "probe-macos-lima".to_owned(),
                "--workspace-root".to_owned(),
                request.workspace_root.display().to_string(),
            ]);
            if let Some(template) = request.template {
                args.extend(["--template".to_owned(), template]);
            }
            if let Some(network) = request.network {
                args.extend(["--network".to_owned(), network]);
            }
            if request.keep_resources.unwrap_or(false) {
                args.push("--keep-resources".to_owned());
            }
            if request.no_rosetta.unwrap_or(false) {
                args.push("--no-rosetta".to_owned());
            }
        }
        "probes.macos.lima.k3s" => {
            let request: ProbeMacosLimaK3sArgsPayload = parse_tool_args(arguments.clone())?;
            args.extend([
                "probe-macos-lima-k3s".to_owned(),
                "--workspace-root".to_owned(),
                request.workspace_root.display().to_string(),
            ]);
            if let Some(template) = request.template {
                args.extend(["--template".to_owned(), template]);
            }
            if let Some(workers) = request.workers {
                args.extend(["--workers".to_owned(), workers.to_string()]);
            }
            if let Some(network) = request.network {
                args.extend(["--network".to_owned(), network]);
            }
            if let Some(api_port) = request.api_port {
                args.extend(["--api-port".to_owned(), api_port.to_string()]);
            }
            if let Some(k3s_version) = request.k3s_version {
                args.extend(["--k3s-version".to_owned(), k3s_version]);
            }
            if request.agentless.unwrap_or(false) {
                args.push("--agentless".to_owned());
            }
            if request.keep_resources.unwrap_or(false) {
                args.push("--keep-resources".to_owned());
            }
            if request.no_rosetta.unwrap_or(false) {
                args.push("--no-rosetta".to_owned());
            }
        }
        _ => {
            return Err(jsonrpc_error(
                -32601,
                format!("tool `{tool_name}` is not available for task execution"),
            ));
        }
    }
    Ok((program, args))
}

#[cfg(test)]
fn append_up_args(args: &mut Vec<String>, up: &UpArgs) {
    if let Some(path) = &up.project_root {
        args.extend(["--project-root".to_owned(), path.display().to_string()]);
    }
    if let Some(name) = &up.name {
        args.extend(["--name".to_owned(), name.clone()]);
    }
    if let Some(backend) = up.backend {
        args.extend(["--backend".to_owned(), backend.as_str().to_owned()]);
    }
    if let Some(template) = &up.template {
        args.extend(["--template".to_owned(), template.clone()]);
    }
    if let Some(arch) = up.image_arch {
        args.extend([
            "--arch".to_owned(),
            match arch {
                ImageArch::Aarch64 => "arm64".to_owned(),
                ImageArch::X86_64 => "amd64".to_owned(),
            },
        ]);
    }
    if let Some(kernel) = &up.firecracker_kernel {
        args.extend([
            "--firecracker-kernel".to_owned(),
            kernel.display().to_string(),
        ]);
    }
    if let Some(rootfs) = &up.firecracker_rootfs {
        args.extend([
            "--firecracker-rootfs".to_owned(),
            rootfs.display().to_string(),
        ]);
    }
    if let Some(workers) = up.workers {
        args.extend(["--workers".to_owned(), workers.to_string()]);
    }
    if let Some(value) = up.control_plane_cpus {
        args.extend(["--control-plane-cpus".to_owned(), value.to_string()]);
    }
    if let Some(value) = up.control_plane_memory_gib {
        args.extend(["--control-plane-memory-gib".to_owned(), value.to_string()]);
    }
    if let Some(value) = up.worker_cpus {
        args.extend(["--worker-cpus".to_owned(), value.to_string()]);
    }
    if let Some(value) = up.worker_memory_gib {
        args.extend(["--worker-memory-gib".to_owned(), value.to_string()]);
    }
    if let Some(network) = &up.network {
        args.extend(["--network".to_owned(), network.clone()]);
    }
    if let Some(api_port) = up.api_port {
        args.extend(["--api-port".to_owned(), api_port.to_string()]);
    }
    if let Some(version) = &up.k3s_version {
        args.extend(["--k3s-version".to_owned(), version.clone()]);
    }
    if let Some(version) = &up.cilium_version {
        args.extend(["--cilium-version".to_owned(), version.clone()]);
    }
    for addon in &up.addons {
        args.extend(["--addons".to_owned(), addon.clone()]);
    }
    if up.keep_failed_runtime {
        args.push("--keep-failed-runtime".to_owned());
    }
    if up.agentless {
        args.push("--agentless".to_owned());
    }
    if up.no_rosetta {
        args.push("--no-rosetta".to_owned());
    }
}

#[cfg(test)]
fn append_bench_args(args: &mut Vec<String>, bench: &BenchArgs) {
    let up = UpArgs {
        project_root: bench.project_root.clone(),
        name: bench.name.clone(),
        backend: bench.backend,
        template: bench.template.clone(),
        image_arch: bench.image_arch,
        firecracker_kernel: bench.firecracker_kernel.clone(),
        firecracker_rootfs: bench.firecracker_rootfs.clone(),
        workers: bench.workers,
        control_plane_cpus: bench.control_plane_cpus,
        control_plane_memory_gib: bench.control_plane_memory_gib,
        worker_cpus: bench.worker_cpus,
        worker_memory_gib: bench.worker_memory_gib,
        network: bench.network.clone(),
        api_port: bench.api_port,
        k3s_version: bench.k3s_version.clone(),
        cilium_version: bench.cilium_version.clone(),
        addons: bench.addons.clone(),
        keep_failed_runtime: bench.keep_failed_runtime,
        stream_vm_logs: false,
        agentless: bench.agentless,
        no_rosetta: bench.no_rosetta,
    };
    append_up_args(args, &up);
    args.extend(["--runs".to_owned(), bench.runs.to_string()]);
}

fn load_task_exit_record(path: &PathBuf) -> Result<Option<TaskExitRecord>> {
    if !path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).map_err(io_error("read_to_string"))?;
    let record = serde_json::from_str(&content).map_err(|error| {
        Error::InvalidArgs(format!(
            "failed to parse MCP task exit record at {}: {error}",
            path.display()
        ))
    })?;
    Ok(Some(record))
}

fn read_log_content(path: &PathBuf) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(path).map_err(io_error("read_to_string"))
}

fn latest_task_log_detail(stdout_path: &PathBuf, stderr_path: &PathBuf) -> Result<Option<String>> {
    if let Some(line) = last_nonempty_line(stderr_path)? {
        return Ok(Some(format!("stderr: {line}")));
    }
    if let Some(line) = last_nonempty_line(stdout_path)? {
        return Ok(Some(format!("stdout: {line}")));
    }
    Ok(None)
}

fn last_nonempty_line(path: &PathBuf) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut file = fs::File::open(path).map_err(io_error("open"))?;
    let len = file.metadata().map_err(io_error("metadata"))?.len();
    if len == 0 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error("seek"))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(io_error("read_to_string"))?;
    Ok(content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned))
}

fn complete_child_result(tool_name: &str, task_id: &str, stdout: String, stderr: String) -> Value {
    match tool_name {
        "clusters.create" => parse_create_result(&stdout).unwrap_or_else(|| {
            success_tool_result(
                format!("Created cluster for task `{task_id}`."),
                json!({ "stdout": stdout, "stderr": stderr }),
            )
        }),
        "clusters.pull" => {
            let values = parse_key_value_lines(&stdout);
            success_tool_result(
                "Prepared boot assets.".to_owned(),
                json!({
                    "assetsRoot": values.get("prepared assets"),
                    "probeImage": values.get("probe image"),
                    "stdout": stdout, "stderr": stderr,
                }),
            )
        }
        "clusters.export" | "clusters.import" => {
            let values = parse_key_value_lines(&stdout);
            success_tool_result(
                "Cold archive operation completed.".into(),
                json!({"cluster":values.get("cluster"),"archive":values.get("archive"),"nodes":values.get("nodes"),"state":values.get("state"),"containsSecrets":true,"encrypted":values.get("encrypted").map(|value| value=="true"),"stdout":stdout,"stderr":stderr}),
            )
        }
        "clusters.up" => parse_up_result(&stdout).unwrap_or_else(|| {
            success_tool_result(
                format!("Brought cluster up for task `{task_id}`."),
                json!({ "stdout": stdout, "stderr": stderr }),
            )
        }),
        "clusters.down" => parse_simple_cluster_result(&stdout, "Stopped cluster", "cluster")
            .unwrap_or_else(|| {
                success_tool_result(
                    format!("Stopped cluster for task `{task_id}`."),
                    json!({ "stdout": stdout, "stderr": stderr }),
                )
            }),
        "clusters.destroy" => parse_simple_cluster_result(&stdout, "Destroyed cluster", "cluster")
            .unwrap_or_else(|| {
                success_tool_result(
                    format!("Destroyed cluster for task `{task_id}`."),
                    json!({ "stdout": stdout, "stderr": stderr }),
                )
            }),
        "clusters.scale" => parse_scale_result(&stdout).unwrap_or_else(|| {
            success_tool_result(
                format!("Scaled cluster for task `{task_id}`."),
                json!({ "stdout": stdout, "stderr": stderr }),
            )
        }),
        "addons.install" => parse_addon_install_result(&stdout).unwrap_or_else(|| {
            success_tool_result(
                format!("Installed add-on for task `{task_id}`."),
                json!({ "stdout": stdout, "stderr": stderr }),
            )
        }),
        "workloads.apply" => {
            let values = parse_key_value_lines(&stdout);
            let cluster = values
                .get("cluster")
                .cloned()
                .unwrap_or_else(|| "unknown".to_owned());
            success_tool_result(
                format!("Applied workload manifest to cluster `{cluster}`."),
                json!({ "cluster": cluster, "stdout": stdout, "stderr": stderr }),
            )
        }
        "bench.run" => parse_bench_result(&stdout).unwrap_or_else(|| {
            success_tool_result(
                format!("Completed benchmark task `{task_id}`."),
                json!({ "stdout": stdout, "stderr": stderr }),
            )
        }),
        "probes.macos.lima" | "probes.macos.lima.k3s" => parse_probe_result(&stdout)
            .unwrap_or_else(|| {
                success_tool_result(
                    format!("Completed probe task `{task_id}`."),
                    json!({ "stdout": stdout, "stderr": stderr }),
                )
            }),
        _ => success_tool_result(
            format!("Completed task `{task_id}`."),
            json!({ "stdout": stdout, "stderr": stderr }),
        ),
    }
}

fn parse_create_result(stdout: &str) -> Option<Value> {
    let values = parse_key_value_lines(stdout);
    Some(success_tool_result(
        format!("Created cluster `{}`.", values.get("cluster")?),
        json!({
            "cluster": values.get("cluster")?,
            "stateRoot": values.get("state root")?,
            "runtimeRoot": values.get("runtime root")?
        }),
    ))
}

fn parse_up_result(stdout: &str) -> Option<Value> {
    let values = parse_key_value_lines(stdout);
    Some(success_tool_result(
        format!("Cluster `{}` is ready.", values.get("cluster")?),
        json!({
            "cluster": values.get("cluster")?,
            "kubeconfigPath": values.get("kubeconfig")?,
            "controlPlaneReady": values.get("control-plane ready")?,
            "clusterReady": values.get("cluster ready")?
        }),
    ))
}

fn parse_scale_result(stdout: &str) -> Option<Value> {
    let values = parse_key_value_lines(stdout);
    let workers = values.get("workers")?;
    let (previous, current) = workers.split_once(" -> ")?;
    let added = values
        .get("added nodes")
        .map(|value| {
            if value == "none" {
                Vec::new()
            } else {
                value.split(", ").map(ToOwned::to_owned).collect::<Vec<_>>()
            }
        })
        .unwrap_or_default();
    Some(success_tool_result(
        format!(
            "Scaled cluster `{}` from {} to {} worker(s).",
            values.get("cluster")?,
            previous,
            current
        ),
        json!({
            "cluster": values.get("cluster")?,
            "previousWorkers": previous.parse::<usize>().ok()?,
            "currentWorkers": current.parse::<usize>().ok()?,
            "addedWorkers": added
        }),
    ))
}

fn parse_addon_install_result(stdout: &str) -> Option<Value> {
    let values = parse_key_value_lines(stdout);
    Some(success_tool_result(
        format!(
            "Installed add-on `{}` on cluster `{}`.",
            values.get("installed add-on")?,
            values.get("cluster")?
        ),
        json!({
            "cluster": values.get("cluster")?,
            "addon": values.get("installed add-on")?
        }),
    ))
}

fn parse_bench_result(stdout: &str) -> Option<Value> {
    let mut samples = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rest = line.strip_prefix("run ")?;
        let (run, tail) = rest.split_once(": ")?;
        let parts = parse_inline_assignments(tail);
        samples.push(json!({
            "run": run.parse::<usize>().ok()?,
            "cluster": parts.get("cluster")?,
            "controlPlaneReady": parts.get("control-plane-ready")?,
            "clusterReady": parts.get("cluster-ready")?
        }));
    }
    Some(success_tool_result(
        format!("Completed {} benchmark run(s).", samples.len()),
        json!({ "samples": samples }),
    ))
}

fn parse_probe_result(stdout: &str) -> Option<Value> {
    let mut results = Vec::new();
    let mut summary = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(body) = line.strip_prefix('[') {
            let (status, remainder) = body.split_once("] ")?;
            let (name, detail) = remainder.split_once(": ")?;
            results.push(json!({
                "status": status,
                "name": name,
                "detail": detail
            }));
        } else if line.starts_with("summary: ") {
            summary = Some(line.to_owned());
        }
    }
    let summary_text = summary
        .clone()
        .unwrap_or_else(|| "Probe completed.".to_owned());
    Some(success_tool_result(
        summary_text,
        json!({
            "results": results,
            "summary": summary
        }),
    ))
}

fn parse_simple_cluster_result(stdout: &str, prefix: &str, key: &str) -> Option<Value> {
    let line = stdout.lines().find(|line| line.contains(prefix))?;
    let name = line.split('`').nth(1)?;
    Some(success_tool_result(
        format!("{prefix} `{name}`."),
        json!({ key: name }),
    ))
}

fn parse_key_value_lines(stdout: &str) -> HashMap<String, String> {
    stdout
        .lines()
        .filter_map(|line| line.split_once(": "))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

fn parse_inline_assignments(input: &str) -> HashMap<String, String> {
    input
        .split_whitespace()
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

#[derive(Debug)]
struct TaskExecution {
    child: Mutex<Option<Child>>,
    cancel_requested: AtomicBool,
}

impl TaskExecution {
    fn detached() -> Self {
        Self {
            child: Mutex::new(None),
            cancel_requested: AtomicBool::new(false),
        }
    }
}

#[derive(Debug)]
struct TaskCompletion {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    exit_error: Option<String>,
    cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskRequestRecord {
    tool_name: String,
    arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskRecord {
    task_id: String,
    title: Option<String>,
    status: String,
    status_message: Option<String>,
    #[serde(default = "accepted_task_progress")]
    progress: TaskProgress,
    created_at: String,
    last_updated_at: String,
    ttl_millis: Option<u64>,
    poll_interval_millis: u64,
    request: TaskRequestRecord,
    result: Option<Value>,
    #[serde(default)]
    manifest_path: String,
    stdout_path: String,
    stderr_path: String,
    #[serde(default)]
    exit_path: String,
    child_id: Option<u32>,
}

impl TaskRecord {
    fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            TASK_STATUS_COMPLETED | TASK_STATUS_FAILED | TASK_STATUS_CANCELLED
        )
    }

    fn public_view(&self) -> Value {
        json!({
            "id": self.task_id,
            "taskId": self.task_id,
            "title": self.title,
            "status": self.status,
            "statusMessage": self.status_message,
            "progress": self.progress,
            "createdAt": self.created_at,
            "updatedAt": self.last_updated_at,
            "lastUpdatedAt": self.last_updated_at,
            "ttl": self.ttl_millis,
            "cancellable": !self.is_terminal() && self.child_id.is_some() && !matches!(self.request.tool_name.as_str(), "clusters.export" | "clusters.import"),
            "pollInterval": self.poll_interval_millis,
            "_meta": {
                "toolName": self.request.tool_name,
                "manifestPath": self.manifest_path,
                "stdoutPath": self.stdout_path,
                "stderrPath": self.stderr_path,
                "exitPath": self.exit_path,
                "childPid": self.child_id
            }
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskProgress {
    phase: String,
    current_step: u64,
    total_steps: u64,
    percent: u64,
    detail: Option<String>,
}

impl TaskProgress {
    fn new(phase: &str, current_step: u64, total_steps: u64, detail: Option<String>) -> Self {
        let bounded_total = total_steps.max(1);
        let bounded_current = current_step.min(bounded_total);
        let percent = ((bounded_current * 100) / bounded_total).min(100);
        Self {
            phase: phase.to_owned(),
            current_step: bounded_current,
            total_steps: bounded_total,
            percent,
            detail,
        }
    }
}

fn accepted_task_progress() -> TaskProgress {
    TaskProgress::new(
        "accepted",
        1,
        4,
        Some("Kalisto accepted the task request.".to_owned()),
    )
}

fn started_task_progress(program: &str, child_id: u32) -> TaskProgress {
    TaskProgress::new(
        "starting",
        2,
        4,
        Some(format!("Started `{program}` with pid {child_id}.")),
    )
}

fn running_task_progress(detail: Option<String>) -> TaskProgress {
    TaskProgress::new("running", 3, 4, detail)
}

fn terminal_task_progress(status: &str) -> TaskProgress {
    let detail = match status {
        TASK_STATUS_COMPLETED => Some("Kalisto task completed successfully.".to_owned()),
        TASK_STATUS_CANCELLED => Some("Kalisto task was cancelled.".to_owned()),
        _ => Some("Kalisto task finished with an error result.".to_owned()),
    };
    TaskProgress::new(status, 4, 4, detail)
}

fn progress_for_status(
    status: &str,
    detail: Option<String>,
    child_id: Option<u32>,
) -> TaskProgress {
    match status {
        TASK_STATUS_COMPLETED | TASK_STATUS_FAILED | TASK_STATUS_CANCELLED => {
            let mut progress = terminal_task_progress(status);
            if detail.is_some() {
                progress.detail = detail;
            }
            progress
        }
        TASK_STATUS_CANCELLING => TaskProgress::new("cancelling", 3, 4, detail),
        TASK_STATUS_WORKING if child_id.is_some() => running_task_progress(detail),
        TASK_STATUS_WORKING => TaskProgress::new("accepted", 1, 4, detail),
        _ => TaskProgress::new(status, 1, 4, detail),
    }
}

fn unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

fn iso8601_now() -> String {
    iso8601_from_system_time(SystemTime::now())
}

fn iso8601_from_system_time(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let seconds = duration.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

fn io_error(program: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |source| Error::CommandIo {
        program: program.to_owned(),
        source,
    }
}

fn poisoned(
    label: &'static str,
) -> impl Fn(std::sync::PoisonError<std::sync::MutexGuard<'_, BTreeMap<String, TaskRecord>>>) -> Error
{
    move |_| Error::InvalidArgs(format!("{label} mutex was poisoned"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use structopt::StructOpt;

    use crate::backend::BackendKind;
    use crate::cli::{Cli, Command};
    use crate::cluster::addon::{CniPlugin, DEFAULT_CILIUM_VERSION, MetalLbConfig, MetalLbMode};
    use crate::cluster::spec::NodeResources;
    use crate::cluster::state::{ClusterState, NodeState};
    use crate::k3s::{ComponentToggles, NodeRole, ServerMode};
    use crate::test_support::env_lock;

    use super::{
        McpServer, SUPPORTED_PROTOCOL_VERSION, TaskSupport, ToolCallRequest, execute_direct_tool,
        linux_firecracker_sudo_guidance_from_args, parse_tool_args, tail_lines, task_command,
        tool_by_name, tool_definitions,
    };

    #[test]
    fn pull_tool_requires_a_task_and_reuses_the_lifecycle_cli_flags() {
        assert!(matches!(
            tool_by_name("clusters.pull").unwrap().task_support,
            TaskSupport::Required
        ));
        let (_, args) = task_command(
            "clusters.pull",
            &json!({"addons": ["cni/cilium"], "workers": 2}),
            None,
        )
        .unwrap();
        assert_eq!(args.first().map(String::as_str), Some("pull"));
        let cli = Cli::from_iter_safe(std::iter::once("kalisto".to_owned()).chain(args)).unwrap();
        assert!(matches!(cli.command, Command::Pull(_)));
    }

    #[test]
    fn cold_archive_tools_require_tasks_and_expose_non_destructive_arguments() {
        for name in ["clusters.export", "clusters.import"] {
            assert_eq!(
                tool_by_name(name).unwrap().task_support,
                TaskSupport::Required
            );
            let schema = super::input_schema(name);
            assert_eq!(schema["additionalProperties"], json!(false));
            assert!(schema["properties"].get("force").is_none());
        }
        let (_, args) = task_command(
            "clusters.export",
            &json!({"name":"dev","output":"dev.kalisto","stop":true}),
            None,
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "export",
                "--name",
                "dev",
                "--output",
                "dev.kalisto",
                "--stop"
            ]
        );
        let (_, args) = task_command(
            "clusters.import",
            &json!({"input":"dev.kalisto","apiPort":16444}),
            None,
        )
        .unwrap();
        assert_eq!(args, vec!["import", "dev.kalisto", "--api-port", "16444"]);
    }

    #[test]
    fn archive_validation_rejects_inline_secrets_without_echoing_them() {
        let secret = "must-not-enter-task-record-or-error";
        for name in ["clusters.export", "clusters.import"] {
            let args = if name == "clusters.export" {
                json!({"name":"dev","output":"backup.age","passphrase":secret})
            } else {
                json!({"input":"backup.age","privateKey":secret})
            };
            let error = super::validate_archive_arguments(name, &args).unwrap_err();
            assert!(!error.message.contains(secret));
        }
        assert!(
            super::validate_archive_arguments(
                "clusters.export",
                &json!({"name":"dev","output":"backup.age","recipients":[secret]})
            )
            .is_err()
        );
        assert!(super::validate_archive_arguments("clusters.import",&json!({"input":"backup.age","identityFiles":["dev.agekey"],"passphraseFile":"secret-file"})).is_err());
        assert!(
            super::validate_archive_arguments(
                "clusters.export",
                &json!({"name":"dev","output":"backup.age","passphraseFile":"secret-file"})
            )
            .is_ok()
        );
        assert!(
            super::validate_archive_arguments(
                "clusters.import",
                &json!({"input":"backup.age","identityFiles":["dev.agekey"]})
            )
            .is_ok()
        );
    }

    #[test]
    fn literal_secrets_in_file_path_fields_fail_before_allocating_or_persisting_tasks() {
        let server = test_server();
        let secret = "AGE-SECRET-KEY-DUMMY-NOT-A-LOCAL-FILE";
        for (name, arguments) in [
            (
                "clusters.export",
                json!({"name":"dev","output":"backup.age","passphraseFile":secret}),
            ),
            (
                "clusters.import",
                json!({"input":"backup.age","identityFiles":[secret]}),
            ),
        ] {
            let error = server.spawn_task(name, arguments).err().unwrap();
            assert!(!error.message.contains(secret));
        }
        assert_eq!(
            server
                .next_task_counter
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(server.tasks.lock().unwrap().is_empty());
    }

    #[test]
    fn archive_task_arguments_contain_only_public_recipients_or_private_file_paths() {
        let key = age::x25519::Identity::generate();
        let public = key.to_public().to_string();
        let (_, args) = task_command(
            "clusters.export",
            &json!({"name":"dev","output":"backup.age","recipients":[public]}),
            None,
        )
        .unwrap();
        assert_eq!(args[args.len() - 2], "--recipient");
        let (_, args) = task_command(
            "clusters.import",
            &json!({"input":"backup.age","identityFiles":["dev.agekey"]}),
            None,
        )
        .unwrap();
        assert_eq!(&args[args.len() - 2..], ["--identity", "dev.agekey"]);
        assert!(
            super::input_schema("clusters.export")["properties"]
                .get("passphrase")
                .is_none()
        );
        assert!(
            super::input_schema("clusters.import")["properties"]
                .get("privateKey")
                .is_none()
        );
    }

    fn test_server() -> Arc<McpServer> {
        let _guard = env_lock();
        let root = std::env::temp_dir().join(format!(
            "kalisto-mcp-test-{}-{}",
            std::process::id(),
            super::unix_timestamp_millis()
        ));
        fs::create_dir_all(root.join("mcp/tasks")).unwrap();
        Arc::new(McpServer {
            default_project_root: None,
            initialized: Mutex::new(false),
            framing: Mutex::new(None),
            writer: Mutex::new(()),
            tasks_dir: root.join("mcp/tasks"),
            next_task_counter: AtomicU64::new(0),
            tasks: Mutex::new(BTreeMap::new()),
            executions: Mutex::new(HashMap::new()),
        })
    }

    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};

    #[test]
    fn mcp_command_parses_optional_project_root() {
        let cli = Cli::from_iter_safe(["kalisto", "mcp", "--project-root", "."]).expect("parse");
        match cli.command {
            Command::Mcp(args) => assert_eq!(
                args.project_root.as_deref(),
                Some(std::path::Path::new("."))
            ),
            other => panic!("expected mcp command, got {other:?}"),
        }
    }

    #[test]
    fn tool_list_includes_required_async_lifecycle_tools() {
        let definitions = tool_definitions();
        let up = definitions
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some("clusters.up"))
            .expect("clusters.up tool should be present");
        assert_eq!(
            up.get("execution")
                .and_then(|execution| execution.get("taskSupport"))
                .and_then(Value::as_str),
            Some("required")
        );
    }

    #[test]
    fn lifecycle_tool_schema_is_addon_only() {
        let definitions = tool_definitions();
        let up = definitions
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some("clusters.up"))
            .expect("clusters.up tool should be present");
        let properties = up
            .get("inputSchema")
            .and_then(|schema| schema.get("properties"))
            .and_then(Value::as_object)
            .expect("clusters.up schema should expose properties");

        assert!(properties.contains_key("addons"));
        assert!(!properties.contains_key("cni"));
    }

    #[test]
    fn tool_list_includes_stats_and_addons() {
        let definitions = tool_definitions();
        let names = definitions
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert!(names.contains(&"clusters.stats"));
        assert!(names.contains(&"clusters.state"));
        assert!(names.contains(&"addons.list"));
        assert!(names.contains(&"addons.install"));
        assert!(names.contains(&"workloads.apply"));
    }

    #[test]
    fn workloads_apply_requires_task_execution() {
        let tool = tool_by_name("workloads.apply").expect("tool spec");
        assert!(matches!(tool.task_support, TaskSupport::Required));
    }

    #[test]
    fn tail_lines_keeps_last_lines_and_trailing_newline() {
        let content = "line1\nline2\nline3\n";
        assert_eq!(tail_lines(content, 2), "line2\nline3\n");
        assert_eq!(tail_lines(content, 10), content);
        assert_eq!(tail_lines(content, 0), "");
        assert_eq!(tail_lines("line1", 1), "line1");
    }

    #[test]
    fn initialize_advertises_task_capability() {
        let server = test_server();
        let result = server.initialize_result();
        assert_eq!(
            result.get("protocolVersion").and_then(Value::as_str),
            Some(SUPPORTED_PROTOCOL_VERSION)
        );
        assert_eq!(
            result
                .get("capabilities")
                .and_then(|capabilities| capabilities.get("tasks"))
                .and_then(|tasks| tasks.get("requests"))
                .and_then(|requests| requests.get("tools/call"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn read_only_tool_rejects_task_augmentation() {
        let tool = tool_by_name("clusters.status").expect("tool should exist");
        assert_eq!(tool.task_support, TaskSupport::Forbidden);
    }

    #[test]
    fn optional_probe_tool_allows_task_requests() {
        let tool = tool_by_name("probes.macos.lima").expect("tool should exist");
        assert_eq!(tool.task_support, TaskSupport::Required);
    }

    #[test]
    fn tool_request_parses_task_field() {
        let request: ToolCallRequest = serde_json::from_value(json!({
            "name": "clusters.up",
            "arguments": {
                "name": "dev"
            },
            "task": {}
        }))
        .expect("request should parse");
        assert_eq!(request.name, "clusters.up");
        assert!(request.task.is_some());
    }

    #[test]
    fn create_tool_description_mentions_bridge_specific_linux_firecracker_sudo() {
        let tool = tool_by_name("clusters.create").expect("tool should exist");
        assert!(tool.description.contains("sudo"));
        assert!(tool.description.contains("firecracker"));
        assert!(tool.description.contains("bridge"));
        assert!(tool.description.contains("slirp4netns"));
    }

    #[test]
    fn linux_firecracker_sudo_guidance_is_emitted_for_bridge_mode() {
        let guidance = linux_firecracker_sudo_guidance_from_args(&json!({
            "backend": "firecracker",
            "network": "bridge"
        }));
        if cfg!(target_os = "linux") {
            assert!(guidance.is_some());
        } else {
            assert!(guidance.is_none());
        }
    }

    #[test]
    fn linux_firecracker_sudo_guidance_is_omitted_for_slirp_default_mode() {
        let explicit_slirp = linux_firecracker_sudo_guidance_from_args(&json!({
            "backend": "firecracker",
            "network": "slirp4netns"
        }));
        let implicit_default = linux_firecracker_sudo_guidance_from_args(&json!({
            "backend": "firecracker"
        }));
        assert!(explicit_slirp.is_none());
        assert!(implicit_default.is_none());
    }

    #[test]
    fn parse_empty_tool_args_defaults_to_empty_object() {
        let _: super::EmptyArgs = parse_tool_args(Value::Null).expect("null args should parse");
    }

    #[test]
    fn list_tool_executes_without_clusters() {
        let _guard = env_lock();
        let original = std::env::var_os("XDG_DATA_HOME");
        let temp = std::env::temp_dir().join(format!("kalisto-mcp-list-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        unsafe { std::env::set_var("XDG_DATA_HOME", &temp) };
        let result =
            execute_direct_tool("clusters.list", json!({}), None).expect("tool should execute");
        match original {
            Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        let _ = fs::remove_dir_all(temp);
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn addon_list_tool_executes() {
        let result =
            execute_direct_tool("addons.list", json!({}), None).expect("tool should execute");
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(false));
        assert!(
            result
                .get("structuredContent")
                .and_then(|value| value.get("addons"))
                .and_then(Value::as_array)
                .map(|addons| !addons.is_empty())
                .unwrap_or(false)
        );
    }

    #[test]
    fn cluster_state_tool_serializes_structured_cluster_view() {
        let _guard = env_lock();
        let root = std::env::temp_dir().join(format!(
            "kalisto-mcp-state-{}-{}",
            std::process::id(),
            super::unix_timestamp_millis()
        ));
        let clusters_root = root.join("kalisto").join("clusters");
        let state_root = clusters_root.join("dev");
        fs::create_dir_all(&state_root).unwrap();
        let state = ClusterState {
            name: "dev".to_owned(),
            backend: BackendKind::Lima,
            workspace_root: PathBuf::from("/tmp/workspace"),
            template: "/tmp/template.yaml".to_owned(),
            firecracker_kernel: Some(PathBuf::from("/tmp/vmlinux")),
            firecracker_rootfs: Some(PathBuf::from("/tmp/rootfs.ext4")),
            api_port: 16443,
            worker_count: 1,
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
            addons: vec!["lb/metallb".to_owned()],
            metallb: MetalLbConfig {
                mode: Some(MetalLbMode::Layer2),
                addresses: vec!["10.0.0.10-10.0.0.20".to_owned()],
                config_file: None,
            },
            server_mode: ServerMode::Standard,
            component_toggles: ComponentToggles::default(),
            kubeconfig_path: PathBuf::from("/tmp/kubeconfig.yaml"),
            token_path: PathBuf::from("/tmp/token"),
            nodes: vec![
                NodeState {
                    name: "cp".to_owned(),
                    role: NodeRole::ControlPlane,
                    artifacts_root: PathBuf::from("/tmp/cp"),
                },
                NodeState {
                    name: "w0".to_owned(),
                    role: NodeRole::Worker,
                    artifacts_root: PathBuf::from("/tmp/w0"),
                },
            ],
        };
        state.save(&state_root).unwrap();

        let original = std::env::var_os("XDG_DATA_HOME");
        unsafe { std::env::set_var("XDG_DATA_HOME", &root) };
        let result = execute_direct_tool("clusters.state", json!({ "name": "dev" }), None)
            .expect("clusters.state should execute");
        match original {
            Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        let _ = fs::remove_dir_all(root);

        let cluster = result
            .get("structuredContent")
            .and_then(|value| value.get("cluster"))
            .expect("clusters.state should return cluster content");
        assert_eq!(cluster.get("name").and_then(Value::as_str), Some("dev"));
        assert_eq!(cluster.get("backend").and_then(Value::as_str), Some("lima"));
        assert_eq!(
            cluster
                .get("controlPlaneResources")
                .and_then(|value| value.get("cpus"))
                .and_then(Value::as_u64),
            Some(4)
        );
        assert_eq!(
            cluster
                .get("componentToggles")
                .and_then(|value| value.get("disableTraefik"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            cluster
                .get("nodes")
                .and_then(Value::as_array)
                .map(|nodes| nodes.len()),
            Some(2)
        );
        assert_eq!(
            cluster
                .get("nodes")
                .and_then(Value::as_array)
                .and_then(|nodes| nodes.first())
                .and_then(|node| node.get("role"))
                .and_then(Value::as_str),
            Some("control-plane")
        );
        assert_eq!(
            cluster
                .get("metallb")
                .and_then(|value| value.get("mode"))
                .and_then(Value::as_str),
            Some("layer2")
        );
    }

    #[test]
    fn task_command_builds_addon_install_cli() {
        let (program, args) = task_command(
            "addons.install",
            &json!({ "name": "dev", "addon": "cni/cilium" }),
            None,
        )
        .expect("command should build");
        assert!(program.contains("kalisto"));
        assert_eq!(
            args,
            vec![
                "addon".to_owned(),
                "install".to_owned(),
                "--name".to_owned(),
                "dev".to_owned(),
                "cni/cilium".to_owned()
            ]
        );
    }

    #[test]
    fn task_command_builds_cluster_up_cli_with_addons() {
        let (program, args) = task_command(
            "clusters.up",
            &json!({
                "name": "dev",
                "addons": ["cni/flannel", "lb/metallb"],
                "workers": 0
            }),
            None,
        )
        .expect("command should build");

        assert!(program.contains("kalisto"));
        assert_eq!(
            args,
            vec![
                "up".to_owned(),
                "--name".to_owned(),
                "dev".to_owned(),
                "--workers".to_owned(),
                "0".to_owned(),
                "--addons".to_owned(),
                "cni/flannel".to_owned(),
                "--addons".to_owned(),
                "lb/metallb".to_owned(),
            ]
        );
    }

    #[test]
    fn task_command_rejects_legacy_cni_field_for_cluster_up() {
        let error = task_command(
            "clusters.up",
            &json!({
                "name": "dev",
                "cni": "flannel"
            }),
            None,
        )
        .unwrap_err();

        assert_eq!(error.code, -32602);
        assert!(error.message.contains("invalid tool arguments"));
        assert!(error.message.contains("unknown field `cni`"));
    }

    #[test]
    fn task_record_public_view_exposes_task_id() {
        let record = super::TaskRecord {
            task_id: "task-1".to_owned(),
            title: Some("title".to_owned()),
            status: "working".to_owned(),
            status_message: Some("message".to_owned()),
            progress: super::running_task_progress(Some("message".to_owned())),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            last_updated_at: "2026-01-01T00:00:01Z".to_owned(),
            ttl_millis: Some(1000),
            poll_interval_millis: 1000,
            request: super::TaskRequestRecord {
                tool_name: "clusters.up".to_owned(),
                arguments: json!({}),
            },
            result: None,
            manifest_path: "/tmp/manifest.json".to_owned(),
            stdout_path: "/tmp/stdout".to_owned(),
            stderr_path: "/tmp/stderr".to_owned(),
            exit_path: "/tmp/exit.json".to_owned(),
            child_id: Some(42),
        };
        let view = record.public_view();
        assert_eq!(view.get("taskId").and_then(Value::as_str), Some("task-1"));
        assert_eq!(
            view.get("progress")
                .and_then(|progress| progress.get("phase"))
                .and_then(Value::as_str),
            Some("running")
        );
        assert_eq!(view.get("ttl").and_then(Value::as_u64), Some(1000));
        assert_eq!(view.get("pollInterval").and_then(Value::as_u64), Some(1000));
    }

    #[test]
    fn persisted_task_without_progress_upgrades_on_load() {
        let _guard = env_lock();
        let root = std::env::temp_dir().join(format!(
            "kalisto-mcp-upgrade-{}-{}",
            std::process::id(),
            super::unix_timestamp_millis()
        ));
        let tasks_dir = root.join("kalisto/mcp/tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("task-legacy.json"),
            r#"{
  "task_id": "task-legacy",
  "title": "legacy",
  "status": "completed",
  "status_message": "legacy task finished",
  "created_at": "2026-01-01T00:00:00Z",
  "last_updated_at": "2026-01-01T00:00:01Z",
  "ttl_millis": 1000,
  "poll_interval_millis": 1000,
  "request": {
    "tool_name": "clusters.up",
    "arguments": {}
  },
  "result": null,
  "stdout_path": "/tmp/stdout",
  "stderr_path": "/tmp/stderr",
  "child_id": null
}"#,
        )
        .unwrap();

        let original = std::env::var_os("XDG_DATA_HOME");
        unsafe { std::env::set_var("XDG_DATA_HOME", &root) };
        let loaded = McpServer::load(None)
            .ok()
            .and_then(|server| server.task("task-legacy"));
        match original {
            Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        let _ = fs::remove_dir_all(root);
        let record = loaded.expect("task should be loaded");

        assert_eq!(record.progress.phase, "completed");
        assert_eq!(record.progress.percent, 100);
        assert_eq!(
            record.progress.detail.as_deref(),
            Some("legacy task finished")
        );
    }

    #[test]
    fn reads_content_length_messages() {
        let body = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1})).expect("json body");
        let payload = format!(
            "Content-Length: {}\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(&body)
        );
        let mut reader = Cursor::new(payload.into_bytes());
        let mut framing = None;
        let message = super::read_stdio_message(&mut reader, &mut framing)
            .expect("read message")
            .expect("message");

        assert_eq!(framing, Some(super::FramingMode::ContentLength));
        assert_eq!(message, json!({"jsonrpc":"2.0","id":1}));
    }

    #[test]
    fn writes_content_length_messages() {
        let mut output = Vec::new();
        let message = json!({"jsonrpc":"2.0","result":{}});
        super::write_stdio_message(&mut output, &message, super::FramingMode::ContentLength)
            .expect("write");

        let output = String::from_utf8(output).expect("utf8");
        let (headers, body) = output
            .split_once("\r\n\r\n")
            .expect("content-length separator");
        assert!(headers.starts_with("Content-Length: "));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).expect("parse body"),
            message
        );
    }

    #[test]
    fn mcp_load_ignores_manifest_and_exit_sidecars() {
        let _guard = env_lock();
        let root = std::env::temp_dir().join(format!(
            "kalisto-mcp-sidecars-{}-{}",
            std::process::id(),
            super::unix_timestamp_millis()
        ));
        let tasks_dir = root.join("kalisto/mcp/tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("task-good.json"),
            r#"{
  "task_id": "task-good",
  "title": "good",
  "status": "completed",
  "status_message": "done",
  "created_at": "2026-01-01T00:00:00Z",
  "last_updated_at": "2026-01-01T00:00:01Z",
  "ttl_millis": 1000,
  "poll_interval_millis": 1000,
  "request": {
    "tool_name": "clusters.up",
    "arguments": {}
  },
  "result": null,
  "stdout_path": "/tmp/stdout",
  "stderr_path": "/tmp/stderr",
  "child_id": null
}"#,
        )
        .unwrap();
        fs::write(
            tasks_dir.join("task-good.manifest.json"),
            r#"{
  "tool_name": "clusters.up",
  "arguments": {},
  "default_project_root": null
}"#,
        )
        .unwrap();
        fs::write(
            tasks_dir.join("task-good.exit.json"),
            r#"{
  "exit_code": 0,
  "error": null,
  "finished_at": "2026-01-01T00:00:02Z"
}"#,
        )
        .unwrap();

        let original = std::env::var_os("XDG_DATA_HOME");
        unsafe { std::env::set_var("XDG_DATA_HOME", &root) };
        let loaded = McpServer::load(None)
            .ok()
            .and_then(|server| server.task("task-good"));
        match original {
            Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        let _ = fs::remove_dir_all(root);

        let record = loaded.expect("task record should load while sidecars are ignored");
        assert_eq!(record.task_id, "task-good");
    }
}
