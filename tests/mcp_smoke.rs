//! MCP smoke tests — spawn the `eventkit --mcp` binary as a child process and
//! drive it over JSON-RPC on stdio, the same way a real MCP client would.
//!
//! These catch the class of bug that unit tests miss: runtime configuration
//! issues, tool registration regressions, JSON shape regressions in tool
//! responses.
//!
//! Only `auth_status` is exercised end-to-end here — it's the one tool that
//! never triggers a TCC dialog or mutates state, so it's safe to run in CI
//! and on developer machines with any authorization state.

#![cfg(target_os = "macos")]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// Path to the binary cargo built for the integration test.
fn bin_path() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by Cargo for integration tests.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_eventkit"))
}

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    fn spawn() -> Self {
        let mut child = Command::new(bin_path())
            .arg("--mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn eventkit --mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn send(&mut self, msg: &Value) {
        let line = serde_json::to_string(msg).unwrap();
        writeln!(self.stdin, "{line}").expect("write to MCP stdin");
        self.stdin.flush().ok();
    }

    /// Read JSON-RPC messages until one with the given id arrives, or timeout.
    fn recv_response(&mut self, id: i64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                panic!("timed out waiting for response id={id}");
            }
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read MCP stdout");
            if n == 0 {
                panic!("MCP server closed stdout before response id={id}");
            }
            let v: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("non-JSON line from MCP server: {line:?} ({e})"));
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
        }
    }

    fn initialize(&mut self) {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "eventkit-mcp-smoke", "version": "0"},
            },
        }));
        let resp = self.recv_response(id, Duration::from_secs(5));
        assert!(resp.get("result").is_some(), "initialize returned: {resp}");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    /// Send an arbitrary JSON-RPC request and return the response.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.recv_response(id, Duration::from_secs(10))
    }

    /// Read lines until a NOTIFICATION with `method` arrives, or timeout.
    /// Responses seen along the way are discarded.
    fn wait_for_notification(&mut self, method: &str, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if v.get("id").is_none() && v.get("method").and_then(Value::as_str) == Some(method) {
                return Some(v);
            }
        }
        None
    }

    fn list_tools(&mut self) -> Vec<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"}));
        let resp = self.recv_response(id, Duration::from_secs(5));
        resp["result"]["tools"]
            .as_array()
            .expect("tools array")
            .clone()
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": args},
        }));
        self.recv_response(id, Duration::from_secs(5))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Closing stdin signals EOF; the server should exit cleanly. Don't
        // wait forever if it doesn't.
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_initialize_and_list_tools_does_not_panic() {
    let mut c = McpClient::spawn();
    c.initialize();
    let tools = c.list_tools();
    assert!(
        tools.len() >= 32,
        "expected at least 32 tools, got {}: {:?}",
        tools.len(),
        tools.iter().map(|t| t["name"].as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn mcp_auth_status_tool_is_registered() {
    let mut c = McpClient::spawn();
    c.initialize();
    let tools = c.list_tools();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"auth_status"),
        "auth_status not in tools/list. Got: {names:?}"
    );
}

#[test]
fn mcp_auth_status_returns_valid_structured_response() {
    // Calls auth_status, which is read-only and never fires a TCC dialog.
    // Asserts the response shape, not the values — values depend on the
    // developer's local TCC state.
    let mut c = McpClient::spawn();
    c.initialize();
    let resp = c.call_tool("auth_status", json!({}));
    let structured = &resp["result"]["structuredContent"];
    assert!(
        structured.is_object(),
        "auth_status missing structuredContent: {resp}"
    );
    let valid = [
        "FullAccess",
        "WriteOnly",
        "Denied",
        "NotDetermined",
        "Restricted",
    ];
    for field in ["reminders", "events"] {
        let v = structured[field]
            .as_str()
            .unwrap_or_else(|| panic!("auth_status.{field} missing or not a string: {structured}"));
        assert!(
            valid.contains(&v),
            "auth_status.{field} has unexpected value {v:?}; want one of {valid:?}"
        );
    }
    // remediation is Option<String>; either absent or a non-empty string.
    if let Some(r) = structured.get("remediation").and_then(Value::as_str) {
        assert!(!r.is_empty(), "remediation present but empty");
    }
}

#[test]
fn mcp_event_tools_are_registered() {
    // Event-side parity check: every new event tool from the 1–8 plan must
    // show up in tools/list. Catches accidental tool deletion or rename.
    let mut c = McpClient::spawn();
    c.initialize();
    let tools = c.list_tools();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in [
        "list_events",
        "create_event",
        "update_event",
        "delete_event",
        "get_event",
        "list_calendars",
        "create_event_calendar",
        "update_event_calendar",
        "delete_event_calendar",
        "set_event_availability",
        "get_default_event_calendar",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool {expected:?} in tools/list. Got: {names:?}"
        );
    }
}

#[test]
fn mcp_update_event_schema_includes_new_fields() {
    // Schema drift catcher: if someone renames a field on UpdateEventRequest
    // the input schema changes and this fires.
    let mut c = McpClient::spawn();
    c.initialize();
    let tools = c.list_tools();
    let update_event = tools
        .iter()
        .find(|t| t["name"].as_str() == Some("update_event"))
        .expect("update_event tool missing");
    let props = &update_event["inputSchema"]["properties"];
    assert!(
        props.is_object(),
        "update_event inputSchema.properties missing: {update_event}"
    );
    for field in [
        "title",
        "notes",
        "location",
        "start",
        "end",
        "all_day",
        "calendar_name",
        "URL",
        "availability",
        "structured_location",
        "span",
        "alarms",
        "recurrence",
    ] {
        assert!(
            props.get(field).is_some(),
            "update_event inputSchema missing field {field:?}; got: {props}"
        );
    }
}

#[test]
fn mcp_handles_multiple_sequential_requests_without_panic() {
    let mut c = McpClient::spawn();
    c.initialize();
    let _ = c.list_tools();
    let _ = c.call_tool("auth_status", json!({}));
    let _ = c.list_tools();
    let _ = c.call_tool("auth_status", json!({}));
}

/// EVERY tool must carry annotations, and the safety hints must be coherent.
///
/// The host groups a backend's tools by annotation
/// (`BackendNutritionCard.tsx::groupToolsByAnnotation`): `readOnlyHint` →
/// "Read-Only", `destructiveHint` → "Destructive", neither → "Safe Write",
/// and **no annotations at all → "Other"**. Before this, all 32 EventKit tools
/// had none, so the entire server collapsed into one undifferentiated "Other"
/// bucket and the user got no read-vs-destroy signal anywhere in the UI.
///
/// This fails if a new tool ships unannotated, which would silently put it
/// back in "Other".
#[test]
fn mcp_every_tool_is_annotated_and_coherent() {
    let mut c = McpClient::spawn();
    c.initialize();
    let tools = c.list_tools();
    assert!(!tools.is_empty(), "tools/list must not be empty");

    let mut unannotated = Vec::new();
    let mut incoherent = Vec::new();
    let (mut read_only, mut destructive, mut safe_write) = (0, 0, 0);

    for t in &tools {
        let name = t["name"].as_str().unwrap_or("<unnamed>").to_string();
        let Some(ann) = t.get("annotations").filter(|a| a.is_object()) else {
            unannotated.push(name);
            continue;
        };
        let ro = ann["readOnlyHint"].as_bool().unwrap_or(false);
        let de = ann["destructiveHint"].as_bool().unwrap_or(false);

        // A read-only tool cannot also be destructive — that is a contradiction,
        // and the grouping would silently prefer "Read-Only" and hide the risk.
        if ro && de {
            incoherent.push(name.clone());
        }
        // Every tool should carry a human title for the UI.
        if ann["title"].as_str().unwrap_or("").is_empty() {
            incoherent.push(format!("{name} (no title)"));
        }

        if ro {
            read_only += 1;
        } else if de {
            destructive += 1;
        } else {
            safe_write += 1;
        }
    }

    assert!(
        unannotated.is_empty(),
        "these tools have NO annotations and would fall into the host's \"Other\" \
         bucket: {unannotated:?}"
    );
    assert!(
        incoherent.is_empty(),
        "incoherent or untitled annotations: {incoherent:?}"
    );

    // Sanity: this server genuinely spans all three groups. If a whole class
    // vanished, the classification was probably flattened by accident.
    assert!(read_only > 0, "expected some read-only tools");
    assert!(destructive > 0, "expected some destructive tools (deletes)");
    assert!(
        safe_write > 0,
        "expected some safe-write tools (creates/updates)"
    );
}

/// The server must ADVERTISE tasks with the shape it actually implements.
///
/// `enable_tasks()` alone serializes an empty `tasks: {}`, which under-declares
/// the surface — a client can't tell whether `tasks/list` or `tasks/cancel`
/// exist. `TasksCapability::server_default()` declares
/// `requests.tools.call` + `list` + `cancel`, which is exactly what this
/// server implements.
#[test]
fn mcp_advertises_the_task_capability_it_implements() {
    let mut c = McpClient::spawn();
    c.next_id += 1;
    let id = c.next_id;
    c.send(&json!({
        "jsonrpc": "2.0", "id": id, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "eventkit-mcp-smoke", "version": "0"},
        },
    }));
    let resp = c.recv_response(id, Duration::from_secs(5));
    let tasks = &resp["result"]["capabilities"]["tasks"];
    assert!(
        tasks.is_object(),
        "tasks capability must be advertised: {resp}"
    );
    assert!(
        tasks["requests"]["tools"]["call"].is_object(),
        "task-augmented tools/call must be declared: {tasks}"
    );
    assert!(
        tasks["list"].is_object(),
        "tasks/list must be declared: {tasks}"
    );
    assert!(
        tasks["cancel"].is_object(),
        "tasks/cancel must be declared: {tasks}"
    );
}

/// Slow tools must declare `execution.taskSupport`, or a host has no way to
/// know it may run them task-augmented and will keep timing them out.
#[test]
fn mcp_slow_tools_declare_task_support() {
    let mut c = McpClient::spawn();
    c.initialize();
    let tools = c.list_tools();
    let capable: Vec<&str> = tools
        .iter()
        .filter(|t| !t["execution"]["taskSupport"].is_null())
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in ["search", "batch_delete", "batch_move", "batch_update"] {
        assert!(
            capable.contains(&expected),
            "`{expected}` is slow enough to need taskSupport; declared: {capable:?}"
        );
    }
}

/// End-to-end: a task-augmented call returns a task id, the task reaches a
/// terminal state, `tasks/result` yields the payload — and the server PUSHES
/// `notifications/tasks/status` rather than making the client poll.
///
/// The push is the part worth guarding. A task server that only answers polls
/// is half a task server, and the notification is invisible to any test that
/// just calls `tasks/get` in a loop.
#[test]
fn mcp_task_roundtrip_emits_status_notification() {
    let mut c = McpClient::spawn();
    c.initialize();

    // MUST be a tool that DECLARES taskSupport — rmcp rejects task-augmenting
    // one that doesn't ("Tool does not support task-based invocation"), which
    // is the correct advertised==invokable behaviour. `search` is declared.
    //
    // Works on an unauthorized host too: the tool fails inside the worker, the
    // task goes to `failed`, and the status push still fires — which is the
    // property under test.
    let created = c.request(
        "tools/call",
        json!({
            "name": "search",
            "arguments": {"query": "eventkit-task-smoke-probe"},
            "task": {}
        }),
    );
    let task_id = created["result"]["task"]["taskId"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a created task: {created}"))
        .to_string();
    assert_eq!(
        created["result"]["task"]["status"], "working",
        "a fresh task starts working: {created}"
    );

    // THE ASSERTION THAT MATTERS: the terminal transition arrives unprompted.
    let note = c
        .wait_for_notification("notifications/tasks/status", Duration::from_secs(10))
        .expect("server must PUSH tasks/status on the terminal transition, not force a poll");
    assert_eq!(note["params"]["taskId"], task_id.as_str());
    let pushed = note["params"]["status"].as_str().unwrap_or("");
    assert!(
        matches!(pushed, "completed" | "failed"),
        "pushed status must be terminal, got {pushed:?}"
    );

    // And the result is retrievable afterwards.
    let payload = c.request("tasks/result", json!({"taskId": task_id}));
    assert!(
        payload.get("result").is_some(),
        "tasks/result must yield the payload: {payload}"
    );

    // tasks/list must include it.
    let listed = c.request("tasks/list", json!({}));
    let ids: Vec<&str> = listed["result"]["tasks"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["taskId"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        ids.contains(&task_id.as_str()),
        "tasks/list must include it: {listed}"
    );
}
