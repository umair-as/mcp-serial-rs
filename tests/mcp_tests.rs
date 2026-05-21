// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-process integration tests for the rmcp adapter layer.
//!
//! These tests drive `crate::mcp::McpServer` over an in-memory
//! [`tokio::io::duplex`] pipe, send raw line-delimited JSON-RPC, and
//! assert on the responses. They exist to anchor parity of the rmcp
//! migration before the binary entry point switches over.
//!
//! Coverage:
//!
//! * `initialize` returns a usable server-info / capabilities object.
//! * `tools/list` advertises the dotted tool names with input schemas.
//! * `serial.list_ports` returns structured `{"ports": [...]}`.
//! * `serial.open` runtime validation: port XOR device, disallowed port,
//!   unknown device, port happy-path (stub backend), and device
//!   happy-path (hardware-conditional skip).
//!
//! The legacy hand-rolled stack tests in `tests/protocol_tests.rs` remain
//! authoritative for the binary's MCP wire surface; this file is additive.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::time::timeout;

use mcp_serial_rs::config::DeviceProfile;
use mcp_serial_rs::errors::SerialError;
use mcp_serial_rs::mcp::McpServer;
use mcp_serial_rs::serial::SerialBackend;
use mcp_serial_rs::serial::manager::{SessionManager, TokioSerialBackend};

// ---- helpers --------------------------------------------------------------

/// Build a server backed by the real `tokio-serial` opener. Used for the
/// read-only tests (`initialize`, `tools/list`, `serial.list_ports`) that
/// never call into the backend.
fn tokio_server() -> McpServer<TokioSerialBackend> {
    let sessions = Arc::new(SessionManager::new(TokioSerialBackend));
    let profiles = Arc::new(Vec::new());
    McpServer::new(sessions, profiles, None)
}

/// Inline stub backend: accepts any port/baud and returns one half of a
/// duplex pair. The "device" half is dropped immediately — the open
/// tests do not exercise read/write traffic. Kept local to this file
/// because the `MockBackend` in `src/serial/manager.rs` is hidden behind
/// `#[cfg(test)]` and intentionally not part of the public surface.
#[derive(Clone)]
struct StubBackend {
    fail: Arc<AtomicBool>,
}

impl StubBackend {
    fn new() -> Self {
        Self {
            fail: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl SerialBackend for StubBackend {
    type Port = DuplexStream;

    async fn open(&self, _port: &str, _baud: u32) -> Result<DuplexStream, SerialError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(SerialError::Io {
                message: "stub backend forced failure".into(),
            });
        }
        let (manager_side, _device_side) = tokio::io::duplex(4096);
        Ok(manager_side)
    }
}

/// Build an `McpServer` backed by [`StubBackend`] with the supplied
/// device profiles. Used by every `serial.open` test path.
fn stub_server(profiles: Vec<DeviceProfile>) -> McpServer<StubBackend> {
    let sessions = Arc::new(SessionManager::new(StubBackend::new()));
    McpServer::new(sessions, Arc::new(profiles), None)
}

/// Drive the rmcp server over a duplex pipe, send a sequence of MCP
/// request lines, read one JSON response per line. Returns responses in
/// order. The `initialized` notification, if needed, must be included
/// in `requests` by the caller — this helper makes no assumptions about
/// handshake order.
async fn roundtrip<B: SerialBackend>(server: McpServer<B>, requests: &[Value]) -> Vec<Value> {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);

    let server_task = tokio::spawn(async move {
        let svc = server.serve(server_io).await.expect("serve start");
        let _ = svc.waiting().await;
    });

    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut reader = BufReader::new(client_read);

    for req in requests {
        let mut line = serde_json::to_vec(req).expect("encode request");
        line.push(b'\n');
        client_write.write_all(&line).await.expect("write request");
    }
    client_write.flush().await.expect("flush");

    // Count expected responses: every request with an `id` (i.e. not a
    // notification) gets exactly one response back.
    let expected: usize = requests
        .iter()
        .filter(|r| r.get("id").is_some())
        .count();

    let mut responses = Vec::with_capacity(expected);
    for _ in 0..expected {
        let mut buf = String::new();
        // Cap each read so a regression cannot wedge the suite.
        let n = timeout(Duration::from_secs(5), reader.read_line(&mut buf))
            .await
            .expect("response timeout")
            .expect("read line");
        assert!(n > 0, "EOF before all responses arrived");
        let value: Value = serde_json::from_str(buf.trim_end()).expect("response JSON");
        responses.push(value);
    }

    drop(client_write);
    drop(reader);
    let _ = timeout(Duration::from_secs(2), server_task).await;

    responses
}

fn init_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "mcp-tests", "version": "0.0.1"},
        }
    })
}

fn initialized_notification() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
}

/// Standard handshake (`initialize` + `notifications/initialized`)
/// followed by a single `tools/call` for the named tool with the
/// supplied arguments. Returns the call response (skipping the
/// `initialize` reply).
async fn handshake_then_call<B: SerialBackend>(
    server: McpServer<B>,
    tool: &str,
    arguments: Value,
) -> Value {
    let responses = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }),
        ],
    )
    .await;
    assert_eq!(responses.len(), 2, "expected initialize + tools/call");
    responses.into_iter().nth(1).expect("tools/call response")
}

/// Extract the JSON-RPC server-defined error code from a response.
fn rpc_error_code(resp: &Value) -> i64 {
    resp.get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("expected JSON-RPC error.code in {resp}"))
}

// ---- lifecycle / list_ports ----------------------------------------------

#[tokio::test]
async fn initialize_advertises_tools_capability_and_server_info() {
    let responses = roundtrip(tokio_server(), &[init_request()]).await;
    assert_eq!(responses.len(), 1);
    let result = responses[0].get("result").expect("initialize result");

    assert_eq!(
        result["serverInfo"]["name"], "mcp-serial-rs",
        "serverInfo.name should be the crate name",
    );
    assert!(
        result["capabilities"].get("tools").is_some(),
        "capabilities.tools must be present so clients know tools/list is supported; got {result:?}",
    );
}

#[tokio::test]
async fn tools_list_contains_dotted_tools_with_input_schema() {
    let responses = roundtrip(
        tokio_server(),
        &[
            init_request(),
            initialized_notification(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ],
    )
    .await;
    assert_eq!(responses.len(), 2);
    let result = responses[1].get("result").expect("tools/list result");
    let tools = result["tools"].as_array().expect("tools array");

    for name in ["serial.list_ports", "serial.open"] {
        let t = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("`{name}` must be listed under its dotted name"));
        assert!(
            t.get("inputSchema").is_some(),
            "rmcp must generate an input schema for `{name}`",
        );
        // Spec §Structured Result Requirements: output schemas are
        // deferred to a later migration pass.
        assert!(
            t.get("outputSchema").is_none(),
            "outputSchema for `{name}` should NOT be set yet",
        );
    }
}

#[tokio::test]
async fn call_serial_list_ports_returns_structured_object() {
    let resp = handshake_then_call(tokio_server(), "serial.list_ports", json!({})).await;
    let result = resp.get("result").expect("tools/call result");

    let structured = result
        .get("structuredContent")
        .expect("rmcp adapter must emit structuredContent for object results");
    // MCP defines structuredContent as a JSON object, not a bare array.
    // serial.list_ports therefore wraps its result as `{"ports": [...]}`.
    assert!(
        structured.is_object(),
        "structuredContent must be an object per MCP 2025-11-25 schema, got: {structured}",
    );
    let arr = structured
        .get("ports")
        .and_then(Value::as_array)
        .expect("structuredContent.ports must be the array of descriptors");

    // The CI host may have zero matching ports; that is a valid result.
    for entry in arr {
        assert!(entry.get("port").is_some(), "missing `port` in {entry}");
        for key in ["vid", "pid", "serial", "device", "description"] {
            assert!(entry.get(key).is_some(), "missing `{key}` in {entry}");
        }
    }

    assert_ne!(
        result.get("isError"),
        Some(&json!(true)),
        "successful tools/call must not set isError=true",
    );
}

// ---- serial.open ----------------------------------------------------------

#[tokio::test]
async fn open_by_port_succeeds_returns_16_char_hex_session_id() {
    let resp = handshake_then_call(
        stub_server(vec![]),
        "serial.open",
        json!({"port": "/dev/ttyUSB0", "baud": 115_200}),
    )
    .await;
    let result = resp.get("result").expect("tools/call result");
    assert_ne!(result.get("isError"), Some(&json!(true)));

    let structured = result
        .get("structuredContent")
        .and_then(Value::as_object)
        .expect("structuredContent must be a JSON object");
    let session_id = structured
        .get("session_id")
        .and_then(Value::as_str)
        .expect("session_id field must be a string");
    assert_eq!(
        session_id.len(),
        16,
        "session_id must be 16-char hex; got '{session_id}'",
    );
    assert!(
        session_id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "session_id must be lowercase hex; got '{session_id}'",
    );
}

#[tokio::test]
async fn open_with_both_port_and_device_returns_invalid_param() {
    // Spec §Error Semantics pins SerialError::InvalidParam to -32008.
    // The XOR check is tool-contract validation (JSON deserialised
    // cleanly), so it must surface with the project code, NOT the
    // JSON-RPC envelope code -32602.
    let resp = handshake_then_call(
        stub_server(vec![]),
        "serial.open",
        json!({"port": "/dev/ttyUSB0", "device": "esp32c6"}),
    )
    .await;
    assert_eq!(
        rpc_error_code(&resp),
        -32008,
        "InvalidParam must use the project's pinned code; got {resp}",
    );
    assert_eq!(
        resp["error"]["data"]["name"].as_str(),
        Some("port/device"),
        "structured error.data.name must identify the offending field",
    );
}

#[tokio::test]
async fn open_with_neither_port_nor_device_returns_invalid_param() {
    let resp = handshake_then_call(stub_server(vec![]), "serial.open", json!({})).await;
    assert_eq!(
        rpc_error_code(&resp),
        -32008,
        "missing-both-fields is the same tool-contract failure → -32008",
    );
    assert_eq!(
        resp["error"]["data"]["name"].as_str(),
        Some("port/device"),
    );
}

#[tokio::test]
async fn open_disallowed_port_returns_port_not_allowed() {
    // `/etc/passwd` is not in the default allowlist (`/dev/ttyUSB*` /
    // `/dev/ttyACM*`) and is extraordinarily unlikely to be in any
    // overriding `MCP_SERIAL_ALLOWLIST`. The allowlist check happens
    // inside `SessionManager::open`, before the backend is invoked.
    let resp = handshake_then_call(
        stub_server(vec![]),
        "serial.open",
        json!({"port": "/etc/passwd"}),
    )
    .await;
    assert_eq!(
        rpc_error_code(&resp),
        -32001,
        "PortNotAllowed must surface with the project's pinned code; got {resp}",
    );
    let data = resp["error"]["data"].as_object().expect("error.data object");
    assert_eq!(
        data.get("port").and_then(Value::as_str),
        Some("/etc/passwd"),
        "structured error.data must include the offending port",
    );
}

#[tokio::test]
async fn open_unknown_device_returns_device_not_found() {
    // Empty profile list → no `esp32c6` profile → DeviceNotFound BEFORE
    // any host-side enumeration.
    let resp = handshake_then_call(
        stub_server(vec![]),
        "serial.open",
        json!({"device": "esp32c6"}),
    )
    .await;
    assert_eq!(
        rpc_error_code(&resp),
        -32009,
        "DeviceNotFound must surface with the project's pinned code; got {resp}",
    );
    assert_eq!(
        resp["error"]["data"]["device"].as_str(),
        Some("esp32c6"),
        "structured error.data must include the offending device name",
    );
}

#[tokio::test]
async fn open_by_device_resolves_through_profile_succeeds() {
    // Happy-path device resolution exercises the real OS port enumerator
    // (`tokio_serial::available_ports`), which we cannot mock from the
    // domain layer without an architectural detour. We probe the host
    // and skip cleanly if no USB-serial port with a non-empty serial
    // string is attached.
    let raw = match tokio_serial::available_ports() {
        Ok(ports) => ports,
        Err(_) => {
            eprintln!("skip: tokio_serial::available_ports failed on this host");
            return;
        }
    };
    let allowlisted = mcp_serial_rs::serial::filter_allowlisted(raw);
    let Some(target) = allowlisted
        .into_iter()
        .find(|p| p.serial.is_some() && p.vid.is_some() && p.pid.is_some())
    else {
        eprintln!(
            "skip: no allowlisted USB-serial port with full VID/PID/serial currently attached"
        );
        return;
    };

    let profile = DeviceProfile {
        name: "step7-device".into(),
        match_serial: target.serial.clone().unwrap(),
        match_vid: target.vid,
        match_pid: target.pid,
        baud: 9_600,
        description: "step-7 device-resolution probe".into(),
        probe: None,
        tags: vec![],
    };

    let resp = handshake_then_call(
        stub_server(vec![profile]),
        "serial.open",
        json!({"device": "step7-device"}),
    )
    .await;
    let result = resp.get("result").unwrap_or_else(|| {
        panic!("expected tools/call result for device-open, got {resp}")
    });
    assert_ne!(result.get("isError"), Some(&json!(true)));
    let structured = result
        .get("structuredContent")
        .and_then(Value::as_object)
        .expect("structuredContent must be a JSON object");
    let session_id = structured
        .get("session_id")
        .and_then(Value::as_str)
        .expect("session_id field");
    assert_eq!(session_id.len(), 16);
}
