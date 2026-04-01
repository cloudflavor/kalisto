use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::backend::BackendKind;
use crate::cli::UpArgs;
use crate::cluster::addon::CniPlugin;
use crate::cluster::manager::{
    create as cluster_create, destroy as cluster_destroy, down as cluster_down,
    kubeconfig_path as cluster_kubeconfig_path, list as cluster_list, logs as cluster_logs,
    scale as cluster_scale, status as cluster_status, up as cluster_up,
};
use crate::cluster::spec::ImageArch;
use crate::error::{Error, Result};

const SUPPORTED_PROTOCOL_VERSION: &str = "2025-11-25";
const JSONRPC_VERSION: &str = "2.0";
const DEFAULT_TASK_TTL_MILLIS: u64 = 3_600_000;
const DEFAULT_TASK_POLL_INTERVAL_MILLIS: u64 = 1_000;
const META_RELATED_TASK: &str = "io.modelcontextprotocol/related-task";
const META_IMMEDIATE_RESPONSE: &str = "io.modelcontextprotocol/model-immediate-response";
const TASK_STATUS_WORKING: &str = "working";
const TASK_STATUS_COMPLETED: &str = "completed";
const TASK_STATUS_FAILED: &str = "failed";

pub fn run(project_root: Option<PathBuf>) -> Result<()> {
    let server = McpServer::new(project_root);
    server.serve()
}

struct McpServer {
    default_project_root: Option<PathBuf>,
    initialized: Mutex<bool>,
    writer: Mutex<()>,
    tasks: TaskRegistry,
}

impl McpServer {
    fn new(project_root: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            default_project_root: project_root,
            initialized: Mutex::new(false),
            writer: Mutex::new(()),
            tasks: TaskRegistry::default(),
        })
    }

    fn serve(self: Arc<Self>) -> Result<()> {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = line.map_err(|source| Error::CommandIo {
                program: "stdin".to_owned(),
                source,
            })?;
            if line.trim().is_empty() {
                continue;
            }
            self.handle_line(&line)?;
        }
        Ok(())
    }

    fn handle_line(self: &Arc<Self>, line: &str) -> Result<()> {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                self.write_error(Value::Null, -32700, format!("parse error: {error}"))?;
                return Ok(());
            }
        };
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
            None => self.handle_notification(method, params),
        }
    }

    fn handle_request(self: &Arc<Self>, id: Value, method: &str, params: Value) -> Result<()> {
        match method {
            "initialize" => self.write_result(id, self.initialize_result()),
            "ping" => self.write_result(id, json!({})),
            "tools/list" => self.write_result(id, json!({ "tools": tool_definitions() })),
            "tools/call" => self.handle_tools_call(id, params),
            "tasks/get" => self.handle_task_get(id, params),
            "tasks/list" => self.handle_task_list(id),
            "tasks/result" => self.handle_task_result(id, params),
            _ => self.write_error(id, -32601, format!("method not found: {method}")),
        }
    }

    fn handle_notification(self: &Arc<Self>, method: &str, _params: Value) -> Result<()> {
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
                "experimental": {
                    "tasks": {
                        "listChanged": false,
                        "requests": {
                            "tools/call": true
                        }
                    }
                }
            },
            "serverInfo": {
                "name": "kalisto",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": concat!(
                "Kalisto exposes cluster lifecycle operations over MCP stdio. ",
                "Use task-augmented `tools/call` for mutating operations such as create, up, down, destroy, and scale. ",
                "On Linux with the firecracker backend, start the MCP server with sudo, for example `sudo -E kalisto mcp`, ",
                "because `clusters.create` configures TAP and bridge networking. ",
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

        if wants_task {
            let task = match self.spawn_task(&request.name, request.arguments) {
                Ok(task) => task,
                Err(error) => return self.write_error(id, error.code, error.message),
            };
            let response = json!({
                "task": task.public_view(),
                "_meta": {
                    META_RELATED_TASK: { "taskId": task.id.clone() },
                    META_IMMEDIATE_RESPONSE: format!(
                        "Task `{}` accepted for tool `{}`. Poll `tasks/get` and fetch the final result with `tasks/result`.",
                        task.id,
                        request.name
                    )
                }
            });
            return self.write_result(id, response);
        }

        let result = match execute_tool(
            &request.name,
            request.arguments.unwrap_or_else(|| json!({})),
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
        arguments: Option<Value>,
    ) -> std::result::Result<Arc<TaskHandle>, JsonRpcError> {
        let ttl = DEFAULT_TASK_TTL_MILLIS;
        let task = self.tasks.insert(TaskSeed {
            title: Some(format!("kalisto {}", tool_name.replace('.', " "))),
            ttl_millis: Some(ttl),
        });
        if let Err(error) = self.write_task_status_notification(&task) {
            return Err(jsonrpc_error(-32000, error.to_string()));
        }

        let server = Arc::clone(self);
        let task_for_thread = Arc::clone(&task);
        let tool_name = tool_name.to_owned();
        let default_project_root = self.default_project_root.clone();
        thread::spawn(move || {
            let result = execute_tool(
                &tool_name,
                arguments.unwrap_or_else(|| json!({})),
                default_project_root,
            )
            .unwrap_or_else(|error| tool_internal_error_result(&error.message));
            let status = if result_is_error(&result) {
                TASK_STATUS_FAILED
            } else {
                TASK_STATUS_COMPLETED
            };
            task_for_thread.finish(status, result);
            if let Err(error) = server.write_task_status_notification(&task_for_thread) {
                eprintln!("failed to write MCP task status notification: {error}");
            }
        });

        Ok(task)
    }

    fn handle_task_get(&self, id: Value, params: Value) -> Result<()> {
        let params: TaskLookupParams = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return self.write_error(id, error.code, error.message),
        };
        let Some(task) = self.tasks.get(&params.task_id) else {
            return self.write_error(id, -32602, format!("unknown task `{}`", params.task_id));
        };
        self.write_result(id, task.public_view())
    }

    fn handle_task_list(&self, id: Value) -> Result<()> {
        let tasks = self
            .tasks
            .list()
            .into_iter()
            .map(|task| task.public_view())
            .collect::<Vec<_>>();
        self.write_result(
            id,
            json!({
                "tasks": tasks
            }),
        )
    }

    fn handle_task_result(&self, id: Value, params: Value) -> Result<()> {
        let params: TaskLookupParams = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return self.write_error(id, error.code, error.message),
        };
        let Some(task) = self.tasks.get(&params.task_id) else {
            return self.write_error(id, -32602, format!("unknown task `{}`", params.task_id));
        };
        let result = task.wait_for_result();
        self.write_result(id, attach_related_task_meta(result, &params.task_id))
    }

    fn write_task_status_notification(&self, task: &TaskHandle) -> Result<()> {
        self.write_message(&json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": "notifications/tasks/status",
            "params": {
                "task": task.public_view()
            }
        }))
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
        serde_json::to_writer(&mut stdout, message).map_err(|error| {
            Error::InvalidArgs(format!("failed to serialize MCP message: {error}"))
        })?;
        stdout.write_all(b"\n").map_err(|source| Error::CommandIo {
            program: "stdout".to_owned(),
            source,
        })?;
        stdout.flush().map_err(|source| Error::CommandIo {
            program: "stdout".to_owned(),
            source,
        })?;
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
        name: "clusters.kubeconfig",
        description: "Return the persisted kubeconfig path for a specific Kalisto cluster.",
        task_support: TaskSupport::Forbidden,
    },
    ToolSpec {
        name: "clusters.logs",
        description: "Read locally persisted node log files for a specific cluster node.",
        task_support: TaskSupport::Forbidden,
    },
    ToolSpec {
        name: "clusters.create",
        description: "Create Kalisto cluster state and backend networking. On Linux with the firecracker backend, run the MCP server itself with sudo because create configures TAP and bridge networking.",
        task_support: TaskSupport::Required,
    },
    ToolSpec {
        name: "clusters.up",
        description: "Create the cluster if needed and boot the control-plane and worker nodes. On Linux with the firecracker backend, run the MCP server itself with sudo because up may implicitly call create and configure TAP and bridge networking.",
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
        match tool.task_support {
            TaskSupport::Forbidden => {
                object.insert(
                    "execution".to_owned(),
                    json!({
                        "taskSupport": "forbidden"
                    }),
                );
            }
            TaskSupport::Required => {
                object.insert(
                    "execution".to_owned(),
                    json!({
                        "taskSupport": "required"
                    }),
                );
            }
        }
        if matches!(
            tool.name,
            "clusters.list" | "clusters.status" | "clusters.kubeconfig" | "clusters.logs"
        ) {
            object.insert(
                "annotations".to_owned(),
                json!({
                    "readOnlyHint": true
                }),
            );
        }
        if matches!(tool.name, "clusters.down" | "clusters.destroy") {
            object.insert(
                "annotations".to_owned(),
                json!({
                    "destructiveHint": true
                }),
            );
        }
    }
    definition
}

fn input_schema(tool_name: &str) -> Value {
    match tool_name {
        "clusters.list" => empty_object_schema(),
        "clusters.status" | "clusters.kubeconfig" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        "clusters.logs" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "node": { "type": "string" }
            },
            "required": ["name", "node"],
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
        "clusters.destroy" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        "clusters.scale" => json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "workers": { "type": "integer", "minimum": 0 }
            },
            "required": ["name", "workers"],
            "additionalProperties": false
        }),
        "clusters.create" | "clusters.up" => json!({
            "type": "object",
            "properties": {
                "projectRoot": { "type": "string" },
                "name": { "type": "string" },
                "backend": { "type": "string", "enum": ["firecracker", "lima"] },
                "template": { "type": "string" },
                "arch": { "type": "string", "enum": ["amd64", "x86_64", "arm64", "aarch64"] },
                "firecrackerKernel": { "type": "string" },
                "firecrackerRootfs": { "type": "string" },
                "workers": { "type": "integer", "minimum": 0 },
                "controlPlaneCpus": { "type": "integer", "minimum": 1 },
                "controlPlaneMemoryGib": { "type": "integer", "minimum": 1 },
                "workerCpus": { "type": "integer", "minimum": 1 },
                "workerMemoryGib": { "type": "integer", "minimum": 1 },
                "network": { "type": "string" },
                "apiPort": { "type": "integer", "minimum": 1, "maximum": 65535 },
                "k3sVersion": { "type": "string" },
                "cni": { "type": "string", "enum": ["none", "flannel", "cilium"] },
                "ciliumVersion": { "type": "string" },
                "keepFailedRuntime": { "type": "boolean" },
                "agentless": { "type": "boolean" },
                "noRosetta": { "type": "boolean" }
            },
            "additionalProperties": false
        }),
        _ => empty_object_schema(),
    }
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

fn execute_tool(
    tool_name: &str,
    arguments: Value,
    default_project_root: Option<PathBuf>,
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
            let args: NamedClusterToolArgs = parse_tool_args(arguments)?;
            let status = cluster_status(&args.name).map_err(cluster_error_result)?;
            Ok(success_tool_result(
                format!("Loaded status for cluster `{}`.", status.name),
                json!({
                    "cluster": cluster_status_payload(&status)
                }),
            ))
        }
        "clusters.kubeconfig" => {
            let args: NamedClusterToolArgs = parse_tool_args(arguments)?;
            let path = cluster_kubeconfig_path(&args.name).map_err(cluster_error_result)?;
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
            let logs = cluster_logs(&args.name, &args.node).map_err(cluster_error_result)?;
            let sections = logs
                .sections
                .iter()
                .map(|section| {
                    json!({
                        "label": section.label,
                        "path": section.path.display().to_string(),
                        "content": section.content
                    })
                })
                .collect::<Vec<_>>();
            Ok(success_tool_result(
                format!(
                    "Loaded {} log section(s) for node `{}` in cluster `{}`.",
                    sections.len(),
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
        "clusters.create" => {
            let args: ClusterLifecycleArgs = parse_tool_args(arguments)?;
            let spec = args.into_up_args(default_project_root)?.into_cluster_spec();
            let spec = match spec {
                Ok(spec) => spec,
                Err(error) => return Ok(tool_execution_error_result(&error.to_string(), None)),
            };
            let sudo_guidance = linux_firecracker_sudo_guidance(&spec);
            match cluster_create(&spec) {
                Ok(outcome) => Ok(success_tool_result(
                    format!("Created cluster `{}`.", outcome.state.name),
                    json!({
                        "cluster": outcome.state.name,
                        "stateRoot": spec.state_root().display().to_string(),
                        "runtimeRoot": spec.runtime_root().display().to_string(),
                        "backend": spec.backend.as_str(),
                        "network": spec.network,
                        "requiresSudoOnLinuxFirecracker": sudo_guidance.is_some()
                    }),
                )),
                Err(error) => Ok(tool_execution_error_result(
                    &error.to_string(),
                    sudo_guidance,
                )),
            }
        }
        "clusters.up" => {
            let args: ClusterLifecycleArgs = parse_tool_args(arguments)?;
            let spec = args.into_up_args(default_project_root)?.into_cluster_spec();
            let spec = match spec {
                Ok(spec) => spec,
                Err(error) => return Ok(tool_execution_error_result(&error.to_string(), None)),
            };
            let sudo_guidance = linux_firecracker_sudo_guidance(&spec);
            match cluster_up(&spec) {
                Ok(outcome) => Ok(success_tool_result(
                    format!("Cluster `{}` is ready.", outcome.state.name),
                    json!({
                        "cluster": outcome.state.name,
                        "kubeconfigPath": outcome.state.kubeconfig_path.display().to_string(),
                        "controlPlaneReady": outcome.control_plane_ready,
                        "clusterReady": outcome.cluster_ready,
                        "backend": spec.backend.as_str(),
                        "requiresSudoOnLinuxFirecracker": sudo_guidance.is_some()
                    }),
                )),
                Err(error) => Ok(tool_execution_error_result(
                    &error.to_string(),
                    sudo_guidance,
                )),
            }
        }
        "clusters.down" => {
            let args: ClusterDownArgs = parse_tool_args(arguments)?;
            match cluster_down(&args.name, args.force) {
                Ok(()) => Ok(success_tool_result(
                    format!("Stopped cluster `{}`.", args.name),
                    json!({
                        "cluster": args.name,
                        "force": args.force
                    }),
                )),
                Err(error) => Ok(tool_execution_error_result(&error.to_string(), None)),
            }
        }
        "clusters.destroy" => {
            let args: NamedClusterToolArgs = parse_tool_args(arguments)?;
            match cluster_destroy(&args.name) {
                Ok(()) => Ok(success_tool_result(
                    format!("Destroyed cluster `{}`.", args.name),
                    json!({
                        "cluster": args.name
                    }),
                )),
                Err(error) => Ok(tool_execution_error_result(&error.to_string(), None)),
            }
        }
        "clusters.scale" => {
            let args: ClusterScaleArgs = parse_tool_args(arguments)?;
            match cluster_scale(&args.name, args.workers) {
                Ok(outcome) => Ok(success_tool_result(
                    format!(
                        "Scaled cluster `{}` from {} to {} worker(s).",
                        outcome.name, outcome.previous_workers, outcome.current_workers
                    ),
                    json!({
                        "cluster": outcome.name,
                        "previousWorkers": outcome.previous_workers,
                        "currentWorkers": outcome.current_workers,
                        "addedWorkers": outcome.added_workers
                    }),
                )),
                Err(error) => Ok(tool_execution_error_result(&error.to_string(), None)),
            }
        }
        _ => Err(jsonrpc_error(-32601, format!("unknown tool `{tool_name}`"))),
    }
}

fn cluster_status_payload(status: &crate::cluster::manager::ClusterStatus) -> Value {
    json!({
        "name": status.name,
        "stateRoot": status.state_root.display().to_string(),
        "kubeconfigPath": status.kubeconfig_path.display().to_string(),
        "nodes": status.nodes.iter().map(|node| {
            json!({
                "name": node.name,
                "role": format!("{:?}", node.role).to_ascii_lowercase(),
                "backendStatus": node.backend_status
            })
        }).collect::<Vec<_>>()
    })
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

fn tool_internal_error_result(message: &str) -> Value {
    tool_execution_error_result(message, None)
}

fn result_is_error(result: &Value) -> bool {
    result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn linux_firecracker_sudo_guidance(spec: &crate::cluster::spec::ClusterSpec) -> Option<String> {
    if cfg!(target_os = "linux") && spec.backend == BackendKind::Firecracker {
        Some(
            "This operation uses the firecracker backend on Linux. Run the MCP server itself with sudo, for example `sudo -E kalisto mcp`, because cluster creation configures TAP and bridge networking.".to_owned(),
        )
    } else {
        None
    }
}

fn attach_related_task_meta(mut result: Value, task_id: &str) -> Value {
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    let meta = object
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(meta_object) = meta.as_object_mut() {
        meta_object.insert(
            META_RELATED_TASK.to_owned(),
            json!({
                "taskId": task_id
            }),
        );
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
struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NamedClusterToolArgs {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterLogsArgs {
    name: String,
    node: String,
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

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    cni: Option<String>,
    cilium_version: Option<String>,
    keep_failed_runtime: Option<bool>,
    agentless: Option<bool>,
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
        let cni = match self.cni.as_deref() {
            Some(value) => Some(parse_cni(value)?),
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
            cni,
            cilium_version: self.cilium_version,
            keep_failed_runtime: self.keep_failed_runtime.unwrap_or(false),
            stream_vm_logs: false,
            agentless: self.agentless.unwrap_or(false),
            no_rosetta: self.no_rosetta.unwrap_or(false),
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

fn parse_cni(value: &str) -> std::result::Result<CniPlugin, JsonRpcError> {
    CniPlugin::parse(value).ok_or_else(|| {
        jsonrpc_error(
            -32602,
            format!("unsupported cni `{value}`; expected one of: none, flannel, cilium"),
        )
    })
}

#[derive(Debug, Default)]
struct TaskRegistry {
    counter: AtomicU64,
    tasks: Mutex<HashMap<String, Arc<TaskHandle>>>,
}

impl TaskRegistry {
    fn insert(&self, seed: TaskSeed) -> Arc<TaskHandle> {
        let task_id = format!(
            "task-{}-{}-{}",
            std::process::id(),
            unix_timestamp_millis(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        let task = Arc::new(TaskHandle::new(task_id, seed));
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.insert(task.id.clone(), Arc::clone(&task));
        }
        task
    }

    fn get(&self, task_id: &str) -> Option<Arc<TaskHandle>> {
        self.tasks
            .lock()
            .ok()
            .and_then(|tasks| tasks.get(task_id).cloned())
    }

    fn list(&self) -> Vec<Arc<TaskHandle>> {
        let mut tasks = self
            .tasks
            .lock()
            .map(|tasks| tasks.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        tasks.sort_by(|left, right| left.id.cmp(&right.id));
        tasks
    }
}

#[derive(Debug)]
struct TaskHandle {
    id: String,
    created_at: String,
    inner: Mutex<TaskState>,
    finished: Condvar,
}

impl TaskHandle {
    fn new(id: String, seed: TaskSeed) -> Self {
        Self {
            id,
            created_at: iso8601_now(),
            inner: Mutex::new(TaskState {
                title: seed.title,
                status: TASK_STATUS_WORKING.to_owned(),
                status_message: Some("Kalisto task is running.".to_owned()),
                ttl_millis: seed.ttl_millis,
                updated_at: iso8601_now(),
                result: None,
            }),
            finished: Condvar::new(),
        }
    }

    fn finish(&self, status: &str, result: Value) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.status = status.to_owned();
            inner.status_message = Some(match status {
                TASK_STATUS_COMPLETED => "Kalisto task completed successfully.".to_owned(),
                TASK_STATUS_FAILED => "Kalisto task finished with an error result.".to_owned(),
                _ => "Kalisto task finished.".to_owned(),
            });
            inner.updated_at = iso8601_now();
            inner.result = Some(result);
            self.finished.notify_all();
        }
    }

    fn public_view(&self) -> Value {
        let inner = self
            .inner
            .lock()
            .expect("task mutex should not be poisoned");
        json!({
            "id": self.id,
            "title": inner.title,
            "status": inner.status,
            "statusMessage": inner.status_message,
            "createdAt": self.created_at,
            "updatedAt": inner.updated_at,
            "ttl": inner.ttl_millis.map(|ttl| {
                json!({
                    "seconds": ttl / 1000,
                    "nanos": (ttl % 1000) * 1_000_000
                })
            }),
            "cancellable": false,
            "notificationConfig": {
                "pollIntervalMs": DEFAULT_TASK_POLL_INTERVAL_MILLIS
            }
        })
    }

    fn wait_for_result(&self) -> Value {
        let mut inner = self
            .inner
            .lock()
            .expect("task mutex should not be poisoned");
        while inner.result.is_none() {
            inner = self
                .finished
                .wait(inner)
                .expect("task condvar should not be poisoned");
        }
        inner
            .result
            .clone()
            .expect("task result should be present after wait")
    }
}

#[derive(Debug)]
struct TaskState {
    title: Option<String>,
    status: String,
    status_message: Option<String>,
    ttl_millis: Option<u64>,
    updated_at: String,
    result: Option<Value>,
}

struct TaskSeed {
    title: Option<String>,
    ttl_millis: Option<u64>,
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

#[cfg(test)]
mod tests {
    use crate::test_support::env_lock;
    use serde_json::{Value, json};
    use structopt::StructOpt;

    use crate::cli::{Cli, Command};

    use super::{
        META_RELATED_TASK, McpServer, SUPPORTED_PROTOCOL_VERSION, TaskSupport, ToolCallRequest,
        execute_tool, parse_tool_args, tool_by_name, tool_definitions,
    };

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
    fn initialize_advertises_task_capability() {
        let server = McpServer::new(None);
        let result = server.initialize_result();
        assert_eq!(
            result.get("protocolVersion").and_then(Value::as_str),
            Some(SUPPORTED_PROTOCOL_VERSION)
        );
        assert_eq!(
            result
                .get("capabilities")
                .and_then(|capabilities| capabilities.get("experimental"))
                .and_then(|experimental| experimental.get("tasks"))
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
    fn destroy_task_result_includes_related_task_meta() {
        let server = McpServer::new(None);
        let task = server
            .spawn_task(
                "clusters.destroy",
                Some(json!({ "name": "missing-cluster" })),
            )
            .expect("task should spawn");
        let result = task.wait_for_result();
        let related = super::attach_related_task_meta(result, &task.id);
        assert_eq!(
            related
                .get("_meta")
                .and_then(|meta| meta.get(META_RELATED_TASK))
                .and_then(|related_task| related_task.get("taskId"))
                .and_then(Value::as_str),
            Some(task.id.as_str())
        );
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
    fn create_tool_description_mentions_linux_firecracker_sudo() {
        let tool = tool_by_name("clusters.create").expect("tool should exist");
        assert!(tool.description.contains("sudo"));
        assert!(tool.description.contains("firecracker"));
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
        std::fs::create_dir_all(&temp).unwrap();
        unsafe { std::env::set_var("XDG_DATA_HOME", &temp) };
        let result = execute_tool("clusters.list", json!({}), None).expect("tool should execute");
        match original {
            Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        let _ = std::fs::remove_dir_all(temp);
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(false));
    }
}
