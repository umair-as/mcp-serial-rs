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

use mcp_serial_rs::config::{self, DeviceProfile};
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
/// duplex pair. The "device" half is stashed on a shared vec so read/
/// read_until tests can pop it and drive bytes from the device side. The
/// open-error path is also wired (`fail.store(true)`). Kept local to
/// this file because `src/serial/manager.rs::MockBackend` is hidden
/// behind `#[cfg(test)]` and intentionally not part of the public
/// surface.
#[derive(Clone)]
struct StubBackend {
    fail: Arc<AtomicBool>,
    devices: Arc<std::sync::Mutex<Vec<DuplexStream>>>,
}

impl StubBackend {
    fn new() -> Self {
        Self {
            fail: Arc::new(AtomicBool::new(false)),
            devices: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Pop the most recently opened device-side half. Tests drive this
    /// end (e.g. writing bytes the manager will see on its `read`).
    fn take_device(&self) -> DuplexStream {
        self.devices
            .lock()
            .expect("stub devices mutex poisoned")
            .pop()
            .expect("no device side available — was a session opened first?")
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
        let (manager_side, device_side) = tokio::io::duplex(4096);
        self.devices
            .lock()
            .expect("stub devices mutex poisoned")
            .push(device_side);
        Ok(manager_side)
    }
}

/// Build an `McpServer` backed by [`StubBackend`] with the supplied
/// device profiles. Returns the server and a handle to the backend so
/// read/read_until tests can pop the device side after `serial.open`.
fn stub_server_with_backend(
    profiles: Vec<DeviceProfile>,
) -> (McpServer<StubBackend>, StubBackend) {
    let backend = StubBackend::new();
    let sessions = Arc::new(SessionManager::new(backend.clone()));
    let server = McpServer::new(sessions, Arc::new(profiles), None);
    (server, backend)
}

/// Convenience for tests that do not need a handle to the backend.
fn stub_server(profiles: Vec<DeviceProfile>) -> McpServer<StubBackend> {
    stub_server_with_backend(profiles).0
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

// ---- serial.write / serial.read / serial.read_until ----------------------
//
// These tests open a session **out of band** by calling `SessionManager::open`
// directly, so the test retains a handle to the device side of the duplex
// pair (needed for `serial.read` to have anything to consume). The rmcp
// wire still drives the `write` / `read` / `read_until` calls under test;
// only the session setup bypasses it. The step-7 tests cover `serial.open`
// on the wire.

/// Shared setup: build a stub-backed server, open one session out-of-band
/// on `/dev/ttyUSB0` (matches default allowlist), pop its device-side
/// half. Returns the server, the session_id, and the device side so the
/// test can read what the manager wrote / write bytes the manager will
/// see on `serial.read`.
async fn server_with_open_session() -> (McpServer<StubBackend>, String, DuplexStream) {
    let (server, sessions, backend) = stub_setup(vec![]);
    let session_id = sessions
        .open("/dev/ttyUSB0", 115_200, 5_000)
        .await
        .expect("open out-of-band");
    let device = backend.take_device();
    (server, session_id, device)
}

/// Test-only triple: server, the `Arc<SessionManager>` shared with the
/// server, and the backend (also shared). Lets the test pre-open
/// sessions and access the device side. Kept here, not exposed from the
/// library.
fn stub_setup(
    profiles: Vec<DeviceProfile>,
) -> (
    McpServer<StubBackend>,
    Arc<SessionManager<StubBackend>>,
    StubBackend,
) {
    let backend = StubBackend::new();
    let sessions = Arc::new(SessionManager::new(backend.clone()));
    let server = McpServer::new(sessions.clone(), Arc::new(profiles), None);
    (server, sessions, backend)
}

#[tokio::test]
async fn tools_list_advertises_all_ported_tools() {
    // Acceptance criterion: every tool that has been migrated joins the
    // list under its dotted name. Updated each step as more tools port.
    let responses = roundtrip(
        tokio_server(),
        &[
            init_request(),
            initialized_notification(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ],
    )
    .await;
    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools array");
    for name in [
        "serial.list_ports",
        "serial.open",
        "serial.write",
        "serial.read",
        "serial.read_until",
        "serial.exec",
        "serial.close",
    ] {
        assert!(
            tools.iter().any(|t| t["name"] == name),
            "tools/list must contain `{name}`; got {tools:?}",
        );
    }
}

#[tokio::test]
async fn write_happy_path_returns_bytes_written() {
    let (server, session_id, mut device) = server_with_open_session().await;
    let payload = "hello\n";

    let resp = handshake_then_call(
        server,
        "serial.write",
        json!({"session_id": session_id, "data": payload}),
    )
    .await;
    let result = resp.get("result").expect("tools/call result");
    assert_ne!(result.get("isError"), Some(&json!(true)));
    assert_eq!(
        result["structuredContent"]["bytes_written"]
            .as_u64()
            .expect("bytes_written u64"),
        payload.len() as u64,
    );

    // Sanity check the bytes actually crossed the duplex.
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; payload.len()];
    timeout(Duration::from_secs(2), device.read_exact(&mut buf))
        .await
        .expect("read timeout")
        .expect("read");
    assert_eq!(buf, payload.as_bytes());
}

#[tokio::test]
async fn write_oversized_returns_invalid_param() {
    let (server, session_id, _device) = server_with_open_session().await;
    let oversize = "x".repeat(config::MAX_WRITE_CHUNK + 1);

    let resp = handshake_then_call(
        server,
        "serial.write",
        json!({"session_id": session_id, "data": oversize}),
    )
    .await;
    assert_eq!(
        rpc_error_code(&resp),
        -32008,
        "oversized writes must surface as the pinned InvalidParam code; got {resp}",
    );
    assert_eq!(
        resp["error"]["data"]["name"].as_str(),
        Some("data"),
        "structured error.data.name must point at the offending field",
    );
}

#[tokio::test]
async fn write_unknown_session_returns_session_not_found() {
    let resp = handshake_then_call(
        stub_server(vec![]),
        "serial.write",
        json!({"session_id": "0000000000000000", "data": "x"}),
    )
    .await;
    assert_eq!(rpc_error_code(&resp), -32003);
    assert_eq!(
        resp["error"]["data"]["session_id"].as_str(),
        Some("0000000000000000"),
    );
}

#[tokio::test]
async fn read_returns_device_side_bytes_as_utf8_lossy() {
    let (server, session_id, mut device) = server_with_open_session().await;

    // Push bytes from the device side; the rmcp `serial.read` call must
    // see them.
    use tokio::io::AsyncWriteExt as _;
    device
        .write_all(b"hello-from-device\n")
        .await
        .expect("device write");
    device.flush().await.expect("flush");

    let resp = handshake_then_call(
        server,
        "serial.read",
        json!({"session_id": session_id, "max_bytes": 64, "timeout_ms": 500}),
    )
    .await;
    let result = resp.get("result").expect("tools/call result");
    assert_ne!(result.get("isError"), Some(&json!(true)));
    let data = result["structuredContent"]["data"]
        .as_str()
        .expect("data must be a UTF-8 string");
    assert!(
        data.contains("hello-from-device"),
        "expected device bytes in `data`, got: {data:?}",
    );
}

#[tokio::test]
async fn read_unknown_session_returns_session_not_found() {
    let resp = handshake_then_call(
        stub_server(vec![]),
        "serial.read",
        json!({"session_id": "ffffffffffffffff"}),
    )
    .await;
    assert_eq!(rpc_error_code(&resp), -32003);
}

#[tokio::test]
async fn read_until_matches_across_chunks() {
    let (server, session_id, mut device) = server_with_open_session().await;

    // Feed bytes in pieces so the matcher must accumulate across chunks.
    use tokio::io::AsyncWriteExt as _;
    tokio::spawn(async move {
        device.write_all(b"noise ").await.ok();
        tokio::time::sleep(Duration::from_millis(10)).await;
        device.write_all(b"hello WORLD-").await.ok();
        device.flush().await.ok();
    });

    let resp = handshake_then_call(
        server,
        "serial.read_until",
        json!({"session_id": session_id, "pattern": "WORLD", "timeout_ms": 2000}),
    )
    .await;
    let result = resp.get("result").expect("tools/call result");
    assert_ne!(result.get("isError"), Some(&json!(true)));
    let structured = &result["structuredContent"];
    assert_eq!(structured["matched"], json!(true));
    let data = structured["data"].as_str().expect("data string");
    assert!(data.contains("WORLD"), "data should contain match: {data:?}");
}

#[tokio::test]
async fn read_until_timeout_returns_partial_with_matched_false_not_error() {
    let (server, session_id, _device) = server_with_open_session().await;

    // Device side held but never writes the pattern bytes; deadline
    // expires and the call must return a *successful* tool result with
    // `matched=false`, not a JSON-RPC error.
    let resp = handshake_then_call(
        server,
        "serial.read_until",
        json!({"session_id": session_id, "pattern": "PROMPT>", "timeout_ms": 100}),
    )
    .await;
    let result = resp.get("result").unwrap_or_else(|| {
        panic!("timeout must return a tool result (partial data is normal completion), got {resp}")
    });
    assert_ne!(
        result.get("isError"),
        Some(&json!(true)),
        "timeout must NOT set isError=true (spec §Error Semantics)",
    );
    assert!(resp.get("error").is_none(), "timeout must not be a JSON-RPC error");
    let structured = &result["structuredContent"];
    assert_eq!(structured["matched"], json!(false));
    assert!(structured["data"].is_string());
}

#[tokio::test]
async fn read_until_invalid_regex_returns_invalid_param_without_consuming_bytes() {
    let (server, session_id, mut device) = server_with_open_session().await;

    // Pre-fill the device side. If the handler bailed AFTER reading,
    // these bytes would be consumed. If it bailed BEFORE reading (the
    // contract), they remain readable via a follow-up `serial.read` on
    // the same session.
    use tokio::io::AsyncWriteExt as _;
    device.write_all(b"untouched\n").await.expect("device write");
    device.flush().await.expect("flush");

    // Single roundtrip drives both calls on the same server / session:
    // first the bad-regex `read_until`, then `serial.read` to prove the
    // pre-written bytes are still buffered for the next consumer.
    let responses = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "serial.read_until",
                    "arguments": {
                        "session_id": session_id,
                        "pattern": "[unclosed",
                        "timeout_ms": 1000,
                    },
                },
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "serial.read",
                    "arguments": {
                        "session_id": session_id,
                        "max_bytes": 64,
                        "timeout_ms": 500,
                    },
                },
            }),
        ],
    )
    .await;
    assert_eq!(responses.len(), 3, "init + 2 tool calls = 3 responses");

    // read_until response: -32008 InvalidParam, name = "pattern".
    let read_until_resp = &responses[1];
    assert_eq!(
        rpc_error_code(read_until_resp),
        -32008,
        "invalid regex must surface as InvalidParam (-32008); got {read_until_resp}",
    );
    assert_eq!(
        read_until_resp["error"]["data"]["name"].as_str(),
        Some("pattern"),
        "structured error.data.name must identify the offending field",
    );

    // read response: success, with the pre-written bytes intact —
    // proves the rejected read_until did NOT consume serial data.
    let read_resp = &responses[2];
    let result = read_resp
        .get("result")
        .unwrap_or_else(|| panic!("follow-up serial.read must succeed; got {read_resp}"));
    assert_ne!(result.get("isError"), Some(&json!(true)));
    let data = result["structuredContent"]["data"]
        .as_str()
        .expect("data must be a UTF-8 string");
    assert!(
        data.contains("untouched"),
        "pre-written bytes must still be readable after a rejected read_until; got {data:?}",
    );

    drop(device);
}

#[tokio::test]
async fn read_until_empty_pattern_returns_invalid_param() {
    let (server, session_id, _device) = server_with_open_session().await;
    let resp = handshake_then_call(
        server,
        "serial.read_until",
        json!({"session_id": session_id, "pattern": "", "timeout_ms": 100}),
    )
    .await;
    assert_eq!(rpc_error_code(&resp), -32008);
    assert_eq!(resp["error"]["data"]["name"].as_str(), Some("pattern"));
}

// ---- serial.exec ---------------------------------------------------------
//
// `serial.exec` composes `write` then `read_until`. The behavioural
// guard the spec pins down is "validation BEFORE write" — bad expect
// regex / empty expect / oversized command MUST NOT mutate device
// state. Several tests use the same "pre-fill device side, fail exec,
// then follow up with serial.read on the same session" pattern that
// step 8 introduced.

#[tokio::test]
async fn exec_happy_path_writes_command_verbatim_and_returns_ok_true() {
    let (server, session_id, mut device) = server_with_open_session().await;

    // Pre-stage the response the manager will read after writing the
    // command. The duplex is bidirectional; bytes written here arrive
    // on the manager side's read.
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    device
        .write_all(b"banner\nOK done\n")
        .await
        .expect("device pre-write");
    device.flush().await.expect("flush");

    // Drive the exec call via the wire. The command does NOT end in
    // `\n` — exec must NOT append one (spec §Non-Goals).
    let command = "run-thing";
    let resp = handshake_then_call(
        server,
        "serial.exec",
        json!({
            "session_id": session_id,
            "command": command,
            "expect": "OK",
            "timeout_ms": 2000,
        }),
    )
    .await;

    let result = resp.get("result").expect("tools/call result");
    assert_ne!(result.get("isError"), Some(&json!(true)));
    let structured = &result["structuredContent"];
    assert_eq!(structured["ok"], json!(true), "expect matched → ok=true");
    let output = structured["output"].as_str().expect("output string");
    assert!(
        output.contains("OK"),
        "output should include the matched marker, got {output:?}",
    );

    // Now read from the device side — these are the bytes the manager
    // wrote during the write phase. Must equal `command` verbatim with
    // NO trailing newline.
    let mut buf = vec![0u8; command.len()];
    timeout(Duration::from_secs(2), device.read_exact(&mut buf))
        .await
        .expect("device read timeout")
        .expect("device read");
    assert_eq!(
        buf,
        command.as_bytes(),
        "exec must write `command` verbatim with no implicit newline",
    );
}

#[tokio::test]
async fn exec_oversized_command_returns_invalid_param_without_writing() {
    let (server, session_id, mut device) = server_with_open_session().await;
    // Pre-fill device side; a follow-up read must still see these
    // bytes intact, proving the failed exec did not consume them.
    use tokio::io::AsyncWriteExt as _;
    device.write_all(b"untouched\n").await.expect("device write");
    device.flush().await.expect("flush");

    let oversize = "x".repeat(config::MAX_WRITE_CHUNK + 1);
    let responses = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "serial.exec",
                    "arguments": {
                        "session_id": session_id,
                        "command": oversize,
                        "expect": "OK",
                        "timeout_ms": 1000,
                    },
                },
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "serial.read",
                    "arguments": {"session_id": session_id, "max_bytes": 64, "timeout_ms": 500},
                },
            }),
        ],
    )
    .await;

    let exec_resp = &responses[1];
    assert_eq!(rpc_error_code(exec_resp), -32008);
    assert_eq!(exec_resp["error"]["data"]["name"].as_str(), Some("command"));

    let read_data = responses[2]["result"]["structuredContent"]["data"]
        .as_str()
        .expect("follow-up read data");
    assert!(
        read_data.contains("untouched"),
        "pre-written bytes must survive a rejected exec; got {read_data:?}",
    );
    drop(device);
}

#[tokio::test]
async fn exec_empty_expect_returns_invalid_param() {
    let (server, session_id, _device) = server_with_open_session().await;
    let resp = handshake_then_call(
        server,
        "serial.exec",
        json!({
            "session_id": session_id,
            "command": "cmd",
            "expect": "",
            "timeout_ms": 1000,
        }),
    )
    .await;
    assert_eq!(rpc_error_code(&resp), -32008);
    assert_eq!(resp["error"]["data"]["name"].as_str(), Some("expect"));
}

#[tokio::test]
async fn exec_invalid_regex_expect_returns_invalid_param_without_writing() {
    let (server, session_id, mut device) = server_with_open_session().await;
    use tokio::io::AsyncWriteExt as _;
    device.write_all(b"still-there\n").await.expect("device write");
    device.flush().await.expect("flush");

    // Single roundtrip: bad-expect exec, then serial.read to prove
    // no command bytes leaked through.
    let responses = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "serial.exec",
                    "arguments": {
                        "session_id": session_id,
                        "command": "MUST_NOT_REACH_DEVICE",
                        "expect": "[unclosed",
                        "timeout_ms": 1000,
                    },
                },
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "serial.read",
                    "arguments": {"session_id": session_id, "max_bytes": 256, "timeout_ms": 500},
                },
            }),
        ],
    )
    .await;

    let exec_resp = &responses[1];
    assert_eq!(rpc_error_code(exec_resp), -32008);
    assert_eq!(exec_resp["error"]["data"]["name"].as_str(), Some("expect"));

    let read_data = responses[2]["result"]["structuredContent"]["data"]
        .as_str()
        .expect("follow-up read data");
    assert!(
        read_data.contains("still-there"),
        "pre-written bytes must survive a rejected exec; got {read_data:?}",
    );
    // Most importantly: the command we sent must NOT appear on the
    // manager-side read stream — proving the write did not occur.
    assert!(
        !read_data.contains("MUST_NOT_REACH_DEVICE"),
        "exec must validate `expect` BEFORE writing; command bytes leaked: {read_data:?}",
    );

    // Independent check from the device side: nothing should have
    // arrived there either.
    use tokio::io::AsyncReadExt as _;
    let mut buf = [0u8; 32];
    let n = timeout(Duration::from_millis(100), device.read(&mut buf))
        .await
        .map(|r| r.unwrap_or(0))
        .unwrap_or(0);
    assert_eq!(
        n, 0,
        "device side received {n} bytes after a rejected exec: {:?}",
        &buf[..n],
    );
}

#[tokio::test]
async fn exec_timeout_returns_partial_output_ok_false_not_error() {
    let (server, session_id, _device) = server_with_open_session().await;

    // Device side never writes the expect pattern — exec must time out
    // and return a *successful* tool result with ok=false (spec
    // §Error Semantics: partial output is normal completion).
    let resp = handshake_then_call(
        server,
        "serial.exec",
        json!({
            "session_id": session_id,
            "command": "ping",
            "expect": "NEVER_THIS_PATTERN",
            "timeout_ms": 100,
        }),
    )
    .await;
    let result = resp.get("result").unwrap_or_else(|| {
        panic!("timeout must return a tool result, not a JSON-RPC error; got {resp}")
    });
    assert_ne!(result.get("isError"), Some(&json!(true)));
    assert!(resp.get("error").is_none(), "timeout must not be a JSON-RPC error");
    let structured = &result["structuredContent"];
    assert_eq!(structured["ok"], json!(false));
    assert!(structured["output"].is_string());
}

#[tokio::test]
async fn exec_unknown_session_returns_session_not_found() {
    let resp = handshake_then_call(
        stub_server(vec![]),
        "serial.exec",
        json!({
            "session_id": "deadbeefdeadbeef",
            "command": "cmd",
            "expect": "OK",
            "timeout_ms": 100,
        }),
    )
    .await;
    assert_eq!(rpc_error_code(&resp), -32003);
    assert_eq!(
        resp["error"]["data"]["session_id"].as_str(),
        Some("deadbeefdeadbeef"),
    );
}

// ---- serial.close --------------------------------------------------------

#[tokio::test]
async fn close_happy_path_returns_ok_true() {
    let (server, session_id, _device) = server_with_open_session().await;
    let resp = handshake_then_call(
        server,
        "serial.close",
        json!({"session_id": session_id}),
    )
    .await;
    let result = resp.get("result").expect("tools/call result");
    assert_ne!(result.get("isError"), Some(&json!(true)));
    assert_eq!(result["structuredContent"]["ok"], json!(true));
}

#[tokio::test]
async fn close_unknown_session_returns_session_not_found() {
    let resp = handshake_then_call(
        stub_server(vec![]),
        "serial.close",
        json!({"session_id": "0000000000000000"}),
    )
    .await;
    assert_eq!(rpc_error_code(&resp), -32003);
    assert_eq!(
        resp["error"]["data"]["session_id"].as_str(),
        Some("0000000000000000"),
    );
}

#[tokio::test]
async fn close_releases_session_and_subsequent_read_returns_session_not_found() {
    let (server, session_id, _device) = server_with_open_session().await;

    // close → then attempt a read on the same id in one roundtrip.
    // The follow-up read must hit -32003: the session is removed from
    // the map and writes/reads on a removed id are SessionNotFound.
    let responses = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "serial.close", "arguments": {"session_id": session_id}},
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "serial.read",
                    "arguments": {"session_id": session_id, "max_bytes": 16, "timeout_ms": 100},
                },
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "serial.write",
                    "arguments": {"session_id": session_id, "data": "x"},
                },
            }),
        ],
    )
    .await;
    assert_eq!(responses.len(), 4, "init + close + read + write = 4 responses");

    let close_resp = &responses[1];
    assert_eq!(
        close_resp["result"]["structuredContent"]["ok"],
        json!(true),
        "close itself must succeed; got {close_resp}",
    );

    let read_resp = &responses[2];
    assert_eq!(
        rpc_error_code(read_resp),
        -32003,
        "read on a closed session must be SessionNotFound; got {read_resp}",
    );

    let write_resp = &responses[3];
    assert_eq!(
        rpc_error_code(write_resp),
        -32003,
        "write on a closed session must be SessionNotFound; got {write_resp}",
    );
}

// ---- tool-call journal (step 11) -----------------------------------------
//
// Narrowing semantics:
//   * only `tools/call` invocations are journaled — one `call` row and
//     one `result` row each; `initialize` / `tools/list` / the
//     `notifications/initialized` notification produce NO rows.
//   * `JournalWriter` failure during startup leaves the server in
//     degraded mode (`journal: None`) and dispatch proceeds unchanged.
//
// The tests open a journal file, drive the rmcp wire, then read the
// JSONL file back and assert on the row shapes.

fn read_journal(path: &std::path::Path) -> Vec<serde_json::Value> {
    let contents = std::fs::read_to_string(path).expect("read journal");
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse jsonl line"))
        .collect()
}

async fn stub_server_with_journal(
    path: &std::path::Path,
) -> (
    McpServer<StubBackend>,
    Arc<SessionManager<StubBackend>>,
    StubBackend,
) {
    let backend = StubBackend::new();
    let sessions = Arc::new(SessionManager::new(backend.clone()));
    let writer = mcp_serial_rs::serial::journal::JournalWriter::try_open_arc(path)
        .await
        .expect("journal open in test must succeed");
    let server = McpServer::new(sessions.clone(), Arc::new(Vec::new()), Some(writer));
    (server, sessions, backend)
}

#[tokio::test]
async fn journal_records_tools_call_pairs_and_skips_lifecycle() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();
    drop(tmp); // try_open_arc opens in append mode; we own the path now.

    let (server, sessions, _backend) = stub_server_with_journal(&path).await;
    let session_id = sessions
        .open("/dev/ttyUSB0", 115_200, 5_000)
        .await
        .expect("open out-of-band");

    // The OOB open will NOT appear in the journal — the journal hook
    // lives in the rmcp `call_tool` path and that's only invoked by
    // `tools/call` over the wire.
    let _ = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "serial.list_ports", "arguments": {}},
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "serial.write",
                    "arguments": {"session_id": session_id, "data": "hello\n"},
                },
            }),
        ],
    )
    .await;

    let entries = read_journal(&path);
    // Two tools/call invocations × (call + result) = 4 rows. The
    // initialize/initialized/tools-list traffic must NOT contribute.
    // Row ORDER is intentionally not asserted: rmcp dispatches tool
    // calls concurrently (via tokio::task::JoinSet), so a call/result
    // pair for one tool can interleave with the call/result pair of
    // another. We only assert per-tool counts and direction balance.
    assert_eq!(
        entries.len(),
        4,
        "expected exactly 4 journal rows (2 tools/call × call+result), got {}: {entries:?}",
        entries.len(),
    );
    let counts = |tool: &str, direction: &str| {
        entries
            .iter()
            .filter(|e| e["tool"] == tool && e["direction"] == direction)
            .count()
    };
    assert_eq!(counts("serial.list_ports", "call"), 1);
    assert_eq!(counts("serial.list_ports", "result"), 1);
    assert_eq!(counts("serial.write", "call"), 1);
    assert_eq!(counts("serial.write", "result"), 1);

    // No row mentions `initialize` / `tools/list` / `notifications/initialized`.
    for entry in &entries {
        let t = entry["tool"].as_str().unwrap();
        assert!(
            !t.starts_with("initialize") && t != "tools/list" && !t.starts_with("notifications/"),
            "lifecycle traffic must not be journaled; saw `{t}`",
        );
    }
}

#[tokio::test]
async fn journal_write_summary_clips_data_head_to_128_chars() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();
    drop(tmp);

    let (server, sessions, _backend) = stub_server_with_journal(&path).await;
    let session_id = sessions
        .open("/dev/ttyUSB0", 115_200, 5_000)
        .await
        .expect("open");

    // Build a `data` payload larger than JOURNAL_HEAD_CHARS but
    // under MAX_WRITE_CHUNK so the call itself succeeds.
    let big = "y".repeat(200);
    let _ = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "serial.write",
                    "arguments": {"session_id": session_id, "data": big.clone()},
                },
            }),
        ],
    )
    .await;

    let entries = read_journal(&path);
    assert_eq!(entries.len(), 2);
    let call_summary = &entries[0]["summary"];
    assert_eq!(call_summary["bytes"], 200, "byte count preserved");
    let head = call_summary["head"].as_str().expect("head");
    assert_eq!(
        head.chars().count(),
        128,
        "head must clip to JOURNAL_HEAD_CHARS",
    );
}

#[tokio::test]
async fn journal_records_error_code_for_failed_tool_call() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();
    drop(tmp);

    let (server, _sessions, _backend) = stub_server_with_journal(&path).await;

    // Unknown session → -32003 SessionNotFound.
    let _ = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "serial.write",
                    "arguments": {"session_id": "ffffffffffffffff", "data": "x"},
                },
            }),
        ],
    )
    .await;

    let entries = read_journal(&path);
    assert_eq!(entries.len(), 2);
    let result = &entries[1];
    assert_eq!(result["direction"], "result");
    assert_eq!(result["summary"]["ok"], json!(false));
    assert_eq!(result["summary"]["error_code"], json!(-32003));
}

#[tokio::test]
async fn journal_open_result_uses_freshly_minted_session_id() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();
    drop(tmp);

    let (server, _sessions, _backend) = stub_server_with_journal(&path).await;
    let _ = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "serial.open",
                    "arguments": {"port": "/dev/ttyUSB0"},
                },
            }),
        ],
    )
    .await;

    let entries = read_journal(&path);
    assert_eq!(entries.len(), 2);
    // call row has no session yet — sentinel applies.
    assert_eq!(
        entries[0]["session_id"],
        mcp_serial_rs::serial::journal::JournalEntry::NO_SESSION,
    );
    // result row carries the actual id so analysts can pair it up
    // with subsequent write/read/close rows on that session.
    let sid = entries[1]["session_id"].as_str().expect("session_id");
    assert_eq!(sid.len(), 16, "expected 16-char hex session_id, got {sid:?}");
    assert!(sid.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn degraded_mode_journal_none_still_dispatches() {
    // No journal → call_tool must still run the tool router and
    // return a successful response. Asserts the journal is genuinely
    // optional, not a soft requirement.
    let resp = handshake_then_call(stub_server(vec![]), "serial.list_ports", json!({})).await;
    assert_ne!(resp["result"].get("isError"), Some(&json!(true)));
    assert!(resp["result"]["structuredContent"]["ports"].is_array());
}

#[tokio::test]
async fn double_close_returns_session_not_found() {
    let (server, session_id, _device) = server_with_open_session().await;
    let responses = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "serial.close", "arguments": {"session_id": session_id}},
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "serial.close", "arguments": {"session_id": session_id}},
            }),
        ],
    )
    .await;
    assert_eq!(
        responses[1]["result"]["structuredContent"]["ok"],
        json!(true),
    );
    assert_eq!(
        rpc_error_code(&responses[2]),
        -32003,
        "second close on the same id must be SessionNotFound, not a different code",
    );
}

// ---- concurrency hardening (step 12) -------------------------------------
//
// These tests exercise rmcp's concurrent dispatch — the SDK runs each
// `tools/call` on its own tokio::task::JoinSet entry, so requests in a
// single client batch can be IN-FLIGHT simultaneously on the server.
// The hand-rolled stack's stdin line-loop never had this property; the
// migration introduces it for the first time and these tests pin the
// behaviour:
//
//   1. work on session A does not block work on session B;
//   2. concurrent writes on ONE session serialise byte-cleanly through
//      the per-port mutex (no torn payloads, no interleave);
//   3. close racing with an in-flight read terminates deterministically
//      (no hang, no panic) and leaves the session unusable;
//   4. journal JSONL stays one-record-per-line under concurrent calls
//      — the writer's tokio::sync::Mutex serialises each `log` call.

/// Helper: open `n` sessions on the supplied SessionManager, returning
/// their ids in the order created. The device-side halves are stashed
/// inside the StubBackend; tests pop with `backend.take_device()` if
/// they need them (LIFO order — most recently opened first).
async fn open_n_sessions(
    sessions: &SessionManager<StubBackend>,
    n: usize,
) -> Vec<String> {
    let mut ids = Vec::with_capacity(n);
    for _ in 0..n {
        let sid = sessions
            .open("/dev/ttyUSB0", 115_200, 5_000)
            .await
            .expect("open OOB");
        ids.push(sid);
    }
    ids
}

#[tokio::test]
async fn concurrent_calls_on_different_sessions_progress_independently() {
    // Two sessions; on session A start a `read_until` that will time
    // out (no matching bytes will arrive); on session B do a fast
    // `serial.list_ports`. Under sequential dispatch session B would
    // be blocked waiting for A's 1500ms timeout. Under rmcp's
    // concurrent dispatch B should answer first.
    let (server, sessions, _backend) = stub_setup(vec![]);
    let ids = open_n_sessions(&sessions, 2).await;
    let slow_id = ids[0].clone();

    let responses = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 100,
                "method": "tools/call",
                "params": {
                    "name": "serial.read_until",
                    "arguments": {
                        "session_id": slow_id,
                        "pattern": "NEVER_GONNA_MATCH",
                        "timeout_ms": 1500,
                    },
                },
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 200,
                "method": "tools/call",
                "params": {"name": "serial.list_ports", "arguments": {}},
            }),
        ],
    )
    .await;
    assert_eq!(responses.len(), 3, "init + 2 tool calls = 3 responses");

    // The two tool-call responses (ids 100 and 200) follow `init`.
    // Under concurrent dispatch, the fast `list_ports` (id=200)
    // completes long before the slow `read_until` (id=100) hits its
    // timeout, so we expect 200 BEFORE 100 in arrival order.
    let id_order: Vec<i64> = responses[1..]
        .iter()
        .map(|r| r["id"].as_i64().expect("id"))
        .collect();
    assert_eq!(
        id_order,
        vec![200, 100],
        "fast list_ports must answer before slow read_until — proves \
         requests dispatch concurrently and session B is not blocked \
         by session A's timeout",
    );

    // Sanity: the slow one timed out as a *successful* partial-result
    // tool call (matched=false), not a JSON-RPC error.
    let slow_resp = responses.iter().find(|r| r["id"] == 100).unwrap();
    assert_eq!(
        slow_resp["result"]["structuredContent"]["matched"],
        json!(false),
    );
}

#[tokio::test]
async fn concurrent_writes_on_same_session_serialize_without_byte_interleaving() {
    // Two writes on ONE session in a single batch. The per-port
    // tokio::sync::Mutex inside `SessionManager::write` is the only
    // thing standing between rmcp's concurrent task dispatch and a
    // torn payload on the wire. The test asserts: the device side
    // sees one complete payload followed by the other — never an
    // interleave. Direction (A-first vs B-first) is intentionally
    // not asserted: which task acquires the mutex first is racy.
    let (server, sessions, backend) = stub_setup(vec![]);
    let ids = open_n_sessions(&sessions, 1).await;
    let session_id = ids[0].clone();
    let mut device = backend.take_device();

    // Pick sizes that span multiple syscalls / buffer fills so an
    // interleave would actually surface. 1024 bytes each fits inside
    // MAX_WRITE_CHUNK (4 KiB) so neither call is rejected.
    let payload_a = "A".repeat(1024);
    let payload_b = "B".repeat(1024);

    let _ = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "serial.write",
                    "arguments": {"session_id": session_id, "data": payload_a.clone()},
                },
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "serial.write",
                    "arguments": {"session_id": session_id, "data": payload_b.clone()},
                },
            }),
        ],
    )
    .await;

    // Read everything the manager wrote — total bytes is exact.
    use tokio::io::AsyncReadExt as _;
    let mut buf = vec![0u8; payload_a.len() + payload_b.len()];
    timeout(Duration::from_secs(2), device.read_exact(&mut buf))
        .await
        .expect("read_exact timeout")
        .expect("device read");

    // Acceptable: AAA...BBB or BBB...AAA. Any other shape is an
    // interleave — fail loudly with the actual sequence so a
    // regression is debuggable.
    let a_then_b = {
        let mut expected = payload_a.as_bytes().to_vec();
        expected.extend_from_slice(payload_b.as_bytes());
        expected
    };
    let b_then_a = {
        let mut expected = payload_b.as_bytes().to_vec();
        expected.extend_from_slice(payload_a.as_bytes());
        expected
    };
    assert!(
        buf == a_then_b || buf == b_then_a,
        "concurrent writes on one session must serialise; saw torn output \
         (first 64 bytes: {:?}, last 64 bytes: {:?})",
        String::from_utf8_lossy(&buf[..64]),
        String::from_utf8_lossy(&buf[buf.len() - 64..]),
    );
}

#[tokio::test]
async fn close_racing_with_in_flight_read_has_deterministic_outcome() {
    // Send `read_until` (will time out — device never writes the
    // pattern) and `serial.close` in the same batch. The race is
    // intentionally not asserted to pick a specific winner — only
    // that:
    //   * both calls return (no hang),
    //   * the read terminates either successfully with matched=false
    //     OR with SessionNotFound (-32003) — both are valid given the
    //     race; we do not pin which one,
    //   * after the dust settles a fresh read on the same session id
    //     returns SessionNotFound, proving the close did remove the
    //     session.
    let (server, sessions, _backend) = stub_setup(vec![]);
    let ids = open_n_sessions(&sessions, 1).await;
    let session_id = ids[0].clone();

    let responses = roundtrip(
        server,
        &[
            init_request(),
            initialized_notification(),
            json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tools/call",
                "params": {
                    "name": "serial.read_until",
                    "arguments": {
                        "session_id": session_id,
                        "pattern": "NEVER",
                        "timeout_ms": 800,
                    },
                },
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "tools/call",
                "params": {"name": "serial.close", "arguments": {"session_id": session_id}},
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 13,
                "method": "tools/call",
                "params": {
                    "name": "serial.read",
                    "arguments": {"session_id": session_id, "max_bytes": 16, "timeout_ms": 100},
                },
            }),
        ],
    )
    .await;
    assert_eq!(responses.len(), 4, "init + 3 tool calls = 4 responses");

    // read_until: either matched=false (timed out before close
    // observed it) or -32003 (close removed the session mid-read).
    // Ids start at 11 to avoid colliding with `initialize`'s id=1.
    let read_until_resp = responses.iter().find(|r| r["id"] == 11).unwrap();
    if let Some(err) = read_until_resp.get("error") {
        assert_eq!(
            err["code"].as_i64(),
            Some(-32003),
            "if read_until errors during close race it must be -32003 SessionNotFound; got {err}",
        );
    } else {
        assert_eq!(
            read_until_resp["result"]["structuredContent"]["matched"],
            json!(false),
            "if read_until succeeds during close race it must be matched=false (timeout); got {read_until_resp}",
        );
    }

    // close: must succeed exactly once (the legacy semantics — close
    // on Ready or Opening is fine, close on Closed is -32003).
    // Under the race, it could also lose the racer and be -32003 if
    // the session was somehow already gone, but with a single close
    // call against a Ready session, it should win.
    let close_resp = responses.iter().find(|r| r["id"] == 12).unwrap();
    if close_resp.get("error").is_some() {
        // Acceptable only if it lost the race to nothing else here;
        // since we only call close once, anything other than success
        // is suspect. Surface for debugging but do not fail — the
        // close test in step 10 covers the deterministic path.
        eprintln!(
            "note: close lost the race in this run: {close_resp}; \
             that is acceptable as long as the post-race read is -32003",
        );
    }

    // Post-race read: deterministic. Whether close or read_until
    // finished first, the session id must be gone by now.
    let post_read = responses.iter().find(|r| r["id"] == 13).unwrap();
    assert_eq!(
        rpc_error_code(post_read),
        -32003,
        "after close + read_until both settle, the session id must \
         no longer be valid; got {post_read}",
    );
}

#[tokio::test]
async fn concurrent_tool_calls_produce_valid_non_interleaved_journal_rows() {
    // Stress the journal under concurrency: fire 8 `serial.list_ports`
    // tool calls in one batch (sessionless, all dispatched in
    // parallel by rmcp). Then parse every line of the resulting
    // JSONL file. Each line must parse as JSON on its own — proving
    // the JournalWriter's tokio::sync::Mutex genuinely serialises
    // line writes and no two `log` calls produce a partial / torn
    // line.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();
    drop(tmp);

    let (server, _sessions, _backend) = stub_server_with_journal(&path).await;

    const N: usize = 8;
    let mut reqs = vec![init_request(), initialized_notification()];
    for i in 0..N {
        reqs.push(json!({
            "jsonrpc": "2.0",
            "id": 1000 + i,
            "method": "tools/call",
            "params": {"name": "serial.list_ports", "arguments": {}},
        }));
    }

    let responses = roundtrip(server, &reqs).await;
    // 1 init response + N tool-call responses
    assert_eq!(responses.len(), 1 + N);
    for resp in &responses[1..] {
        assert!(
            resp.get("error").is_none(),
            "every list_ports call must succeed under concurrency; got {resp}",
        );
    }

    // Parse the journal. We expect 2*N lines (call + result per
    // tool call). Each one must be standalone-parseable JSON.
    let raw = std::fs::read_to_string(&path).expect("read journal");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        2 * N,
        "expected {} JSONL rows ({N} calls × call+result), got {}",
        2 * N,
        lines.len(),
    );
    for (i, line) in lines.iter().enumerate() {
        serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| {
            panic!(
                "line {i} is not valid JSON — likely torn/interleaved: {e}\nLINE: {line:?}",
            )
        });
    }

    // Aggregate by direction to prove call/result balance even when
    // individual rows arrived interleaved (ordering between distinct
    // calls is intentionally not asserted — that is the user-facing
    // nondeterminism).
    let entries: Vec<serde_json::Value> = lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let calls = entries
        .iter()
        .filter(|e| e["direction"] == "call")
        .count();
    let results = entries
        .iter()
        .filter(|e| e["direction"] == "result")
        .count();
    assert_eq!(calls, N);
    assert_eq!(results, N);
}
