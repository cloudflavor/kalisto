use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

fn unique_temp_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kalisto-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis()
    ))
}

struct McpHarness {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpHarness {
    fn spawn(xdg_data_home: &PathBuf) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kalisto"))
            .args(["mcp"])
            .env("XDG_DATA_HOME", xdg_data_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("kalisto mcp should spawn");
        let stdin = child.stdin.take().expect("stdin should be piped");
        let stdout = child.stdout.take().expect("stdout should be piped");
        Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").expect("request should be written");
        self.stdin.flush().expect("request should be flushed");
    }

    fn read_message(&mut self) -> Value {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("response line should be readable");
        serde_json::from_str(line.trim()).expect("response should be valid json")
    }

    fn read_until_response(&mut self, id: i64) -> Value {
        loop {
            let message = self.read_message();
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
        }
    }

    fn wait_for_terminal_task(&mut self, task_id: &str) -> Value {
        for request_id in 100..140 {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tasks/get",
                "params": {
                    "taskId": task_id
                }
            }));
            let response = self.read_until_response(request_id);
            let task = response
                .get("result")
                .cloned()
                .expect("tasks/get should return a result");
            let status = task
                .get("status")
                .and_then(Value::as_str)
                .expect("task status should be present");
            if matches!(status, "completed" | "failed" | "cancelled") {
                return task;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("task `{task_id}` did not reach a terminal state in time");
    }
}

impl Drop for McpHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_stdio_supports_task_backed_calls() {
    let xdg_data_home = unique_temp_dir("mcp-stdio");
    fs::create_dir_all(&xdg_data_home).expect("temp xdg dir should exist");
    let mut harness = McpHarness::spawn(&xdg_data_home);

    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    }));
    let initialize = harness.read_until_response(1);
    assert_eq!(
        initialize
            .get("result")
            .and_then(|result| result.get("serverInfo"))
            .and_then(|server| server.get("name"))
            .and_then(Value::as_str),
        Some("kalisto")
    );

    harness.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));

    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let tools = harness.read_until_response(2);
    let names = tools
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .expect("tools/list should return tools")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(names.contains(&"clusters.destroy"));
    assert!(names.contains(&"addons.list"));

    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "clusters.destroy",
            "arguments": {
                "name": "missing-mcp-stdio-cluster"
            },
            "task": {}
        }
    }));
    let tool_call = harness.read_until_response(3);
    let task_id = tool_call
        .get("result")
        .and_then(|result| result.get("task"))
        .and_then(|task| task.get("taskId"))
        .and_then(Value::as_str)
        .expect("task-backed tool call should return task id")
        .to_owned();

    let terminal_task = harness.wait_for_terminal_task(&task_id);
    assert_eq!(
        terminal_task.get("taskId").and_then(Value::as_str),
        Some(task_id.as_str())
    );
    assert_eq!(
        terminal_task
            .get("progress")
            .and_then(|progress| progress.get("percent"))
            .and_then(Value::as_u64),
        Some(100)
    );
    assert_eq!(
        terminal_task
            .get("progress")
            .and_then(|progress| progress.get("phase"))
            .and_then(Value::as_str),
        Some("failed")
    );

    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tasks/result",
        "params": {
            "taskId": task_id
        }
    }));
    let result = harness.read_until_response(4);
    assert_eq!(
        result
            .get("result")
            .and_then(|value| value.get("isError"))
            .and_then(Value::as_bool),
        Some(true)
    );

    let _ = fs::remove_dir_all(xdg_data_home);
}

#[test]
fn mcp_stdio_clusters_list_keeps_stdout_protocol_clean() {
    let xdg_data_home = unique_temp_dir("mcp-stdio-clusters-list");
    fs::create_dir_all(&xdg_data_home).expect("temp xdg dir should exist");
    let mut harness = McpHarness::spawn(&xdg_data_home);

    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    }));
    let _ = harness.read_until_response(1);

    harness.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));

    harness.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "clusters.list",
            "arguments": {}
        }
    }));
    let response = harness.read_until_response(2);
    assert_eq!(
        response
            .get("result")
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        response
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("clusters"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );

    let _ = fs::remove_dir_all(xdg_data_home);
}
