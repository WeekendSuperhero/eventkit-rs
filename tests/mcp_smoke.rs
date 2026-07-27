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
