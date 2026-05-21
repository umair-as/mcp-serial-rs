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
