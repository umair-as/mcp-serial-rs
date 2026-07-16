// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the rmcp protocol layer — the authoritative
//! wire coverage for the server.
//!
//! These tests drive `crate::mcp::McpServer` in-process over an in-memory
//! [`tokio::io::duplex`] pipe, send raw line-delimited JSON-RPC, and
//! assert on the responses — exercising the same MCP protocol surface the
//! binary speaks over stdio. The end-to-end PTY check through the release
//! binary lives in `tests/loopback.rs`.
//!
//! Coverage:
//!
//! * `initialize` / `tools/list` — server info, capabilities, and the
//!   dotted tool names with rmcp-generated input schemas.
//! * Every `serial.*` tool: happy paths, validation failures, and the
//!   partial-output (`matched=false` / `ok=false`) outcomes.
//! * The narrowed tool-call audit journal — call/result rows, lifecycle
//!   traffic skipped, large-field summaries, degraded mode.
//! * Concurrency under rmcp's parallel dispatch: independent sessions
//!   progress in parallel, same-session writes serialise, and a close
//!   racing an in-flight read resolves deterministically.

use std::collections::BTreeMap;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, ReadBuf,
};
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use mcp_serial_rs::config::{self, DeviceProfile};
use mcp_serial_rs::errors::SerialError;
use mcp_serial_rs::mcp::McpServer;
use mcp_serial_rs::serial::manager::{SessionManager, TokioSerialBackend};
use mcp_serial_rs::serial::{SerialBackend, WritePolicy};

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

#[derive(Clone, Copy)]
enum FaultBehavior {
    PartialWriteThenError,
    FlushError,
    ReadThenError,
    ReadPending,
    WritePending,
    FlushPending,
}

#[derive(Clone)]
struct FaultBackend {
    behavior: FaultBehavior,
    written: Arc<std::sync::Mutex<Vec<u8>>>,
    read_started: Arc<Notify>,
}

impl FaultBackend {
    fn new(behavior: FaultBehavior) -> Self {
        Self {
            behavior,
            written: Arc::new(std::sync::Mutex::new(Vec::new())),
            read_started: Arc::new(Notify::new()),
        }
    }

    fn written(&self) -> Vec<u8> {
        self.written
            .lock()
            .expect("fault backend write evidence mutex poisoned")
            .clone()
    }

    async fn wait_for_read(&self) {
        timeout(Duration::from_secs(1), self.read_started.notified())
            .await
            .expect("fault port was never polled for read");
    }
}

struct FaultPort {
    behavior: FaultBehavior,
    written: Arc<std::sync::Mutex<Vec<u8>>>,
    read_started: Arc<Notify>,
    write_calls: usize,
    read_calls: usize,
}

impl AsyncRead for FaultPort {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.read_started.notify_one();
        match self.behavior {
            FaultBehavior::ReadThenError if self.read_calls == 0 => {
                self.read_calls += 1;
                buf.put_slice(b"partial-device-output");
                Poll::Ready(Ok(()))
            }
            FaultBehavior::ReadThenError => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected read disconnect",
            ))),
            _ => Poll::Pending,
        }
    }
}

impl AsyncWrite for FaultPort {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.behavior {
            FaultBehavior::WritePending => Poll::Pending,
            FaultBehavior::PartialWriteThenError if self.write_calls > 0 => Poll::Ready(Err(
                io::Error::new(io::ErrorKind::BrokenPipe, "injected partial-write failure"),
            )),
            FaultBehavior::PartialWriteThenError => {
                self.write_calls += 1;
                let written = buf.len().min(3);
                self.written
                    .lock()
                    .expect("fault backend write evidence mutex poisoned")
                    .extend_from_slice(&buf[..written]);
                Poll::Ready(Ok(written))
            }
            _ => {
                self.written
                    .lock()
                    .expect("fault backend write evidence mutex poisoned")
                    .extend_from_slice(buf);
                Poll::Ready(Ok(buf.len()))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.behavior {
            FaultBehavior::FlushError => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected flush failure",
            ))),
            FaultBehavior::FlushPending => Poll::Pending,
            _ => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl SerialBackend for FaultBackend {
    type Port = FaultPort;

    async fn open(&self, _port: &str, _baud: u32) -> Result<FaultPort, SerialError> {
        Ok(FaultPort {
            behavior: self.behavior,
            written: Arc::clone(&self.written),
            read_started: Arc::clone(&self.read_started),
            write_calls: 0,
            read_calls: 0,
        })
    }
}

fn fault_setup(
    behavior: FaultBehavior,
) -> (
    McpServer<FaultBackend>,
    Arc<SessionManager<FaultBackend>>,
    FaultBackend,
) {
    let backend = FaultBackend::new(behavior);
    let sessions = Arc::new(SessionManager::new(backend.clone()));
    let server = McpServer::new(sessions.clone(), Arc::new(Vec::new()), None);
    (server, sessions, backend)
}

async fn fault_server_with_open_session(
    behavior: FaultBehavior,
) -> (
    McpServer<FaultBackend>,
    Arc<SessionManager<FaultBackend>>,
    FaultBackend,
    String,
) {
    let (server, sessions, backend) = fault_setup(behavior);
    let session_id = sessions
        .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
        .await
        .expect("open fault-backed session");
    (server, sessions, backend, session_id)
}

/// Build an `McpServer` backed by [`StubBackend`] with the supplied
/// device profiles. Returns the server and a handle to the backend so
/// read/read_until tests can pop the device side after `serial.open`.
fn stub_server_with_backend(profiles: Vec<DeviceProfile>) -> (McpServer<StubBackend>, StubBackend) {
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
    let expected: usize = requests.iter().filter(|r| r.get("id").is_some()).count();

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

/// Extract the serial-domain error code from a tool error result. Falls back
/// to JSON-RPC error.code only for protocol-level failures.
fn rpc_error_code(resp: &Value) -> i64 {
    resp.pointer("/result/structuredContent/error/code")
        .or_else(|| resp.get("error").and_then(|e| e.get("code")))
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("expected tool error or JSON-RPC error.code in {resp}"))
}

fn tool_error_data(resp: &Value) -> &Value {
    resp.pointer("/result/structuredContent/error/data")
        .or_else(|| resp.get("error").and_then(|e| e.get("data")))
        .unwrap_or_else(|| panic!("expected structured tool error data in {resp}"))
}

fn assert_tool_io_error(
    resp: &Value,
    session_id: &str,
    command_written: bool,
    bytes_consumed: bool,
) {
    let result = resp
        .get("result")
        .expect("I/O failure must be a tool result");
    assert_eq!(result["isError"], true, "tool error must set isError=true");
    let error = &result["structuredContent"]["error"];
    assert_eq!(error["type"], "io_error");
    assert_eq!(error["code"], -32006);
    assert_eq!(error["retryable"], true);
    assert_eq!(error["session_id"], session_id);
    assert_eq!(error["command_written"], command_written);
    assert_eq!(error["bytes_consumed"], bytes_consumed);
    assert_eq!(error["session_usable"], false);
}

fn assert_tool_timeout_error(resp: &Value, session_id: &str, timeout_ms: u64) {
    let result = resp
        .get("result")
        .expect("phase timeout must be a tool result");
    assert_eq!(
        result["isError"], true,
        "tool timeout must set isError=true"
    );
    let error = &result["structuredContent"]["error"];
    assert_eq!(error["type"], "timeout");
    assert_eq!(error["code"], -32005);
    assert_eq!(error["data"]["timeout_ms"], timeout_ms);
    assert_eq!(error["retryable"], false);
    assert_eq!(error["session_id"], session_id);
    assert_eq!(error["command_written"], true);
    assert_eq!(error["bytes_consumed"], true);
    assert_eq!(error["session_usable"], true);
}

fn validate_tool_output(schemas: &BTreeMap<String, Value>, tool: &str, response: &Value) {
    let schema = schemas
        .get(tool)
        .unwrap_or_else(|| panic!("missing advertised output schema for {tool}"));
    jsonschema::draft202012::meta::validate(schema)
        .unwrap_or_else(|error| panic!("{tool} advertises an invalid output schema: {error}"));
    let validator = jsonschema::draft202012::new(schema)
        .unwrap_or_else(|error| panic!("failed to compile {tool} output schema: {error}"));
    let structured = response
        .pointer("/result/structuredContent")
        .unwrap_or_else(|| panic!("{tool} response lacks structuredContent: {response}"));
    if let Err(error) = validator.validate(structured) {
        panic!("{tool} structuredContent violates its output schema: {error}; response={response}");
    }
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

    for name in [
        "serial.list_ports",
        "serial.open",
        "serial.sessions",
        "serial.get_session",
        "serial.write",
        "serial.read",
        "serial.drain",
        "serial.clear_input",
        "serial.read_until",
        "serial.exec",
        "serial.close",
    ] {
        let t = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("`{name}` must be listed under its dotted name"));
        assert!(
            t.get("inputSchema").is_some(),
            "rmcp must generate an input schema for `{name}`",
        );
        assert!(
            t.get("outputSchema").is_some(),
            "tool `{name}` must advertise an output schema",
        );
        assert_eq!(
            t["outputSchema"]["type"], "object",
            "tool `{name}` outputSchema root must be an object",
        );
        let output_schema = &t["outputSchema"];
        let any_of = output_schema
            .get("anyOf")
            .and_then(Value::as_array)
            .unwrap_or_else(|| {
                panic!("tool `{name}` outputSchema must expose success/error branches")
            });
        assert!(
            !any_of.is_empty(),
            "tool `{name}` outputSchema anyOf must not be empty",
        );
        for branch in any_of {
            assert!(
                branch.get("required").and_then(Value::as_array).is_some(),
                "tool `{name}` outputSchema branch must preserve required fields: {branch}",
            );
        }
        let defs = output_schema.get("$defs").and_then(Value::as_object);
        let error_ref = any_of
            .iter()
            .find_map(|branch| branch.pointer("/properties/error/$ref"))
            .and_then(Value::as_str);
        if let Some(error_ref) = error_ref {
            let def_name = error_ref.trim_start_matches("#/$defs/");
            assert!(
                defs.and_then(|defs| defs.get(def_name)).is_some(),
                "tool `{name}` outputSchema has unresolved error ref `{error_ref}`",
            );
        }
        assert!(
            t.get("annotations").is_some(),
            "tool `{name}` should have annotations"
        );
        let destructive = t
            .pointer("/annotations/destructiveHint")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| panic!("tool `{name}` must set destructiveHint"));
        let expected_destructive = matches!(
            name,
            "serial.write"
                | "serial.read"
                | "serial.drain"
                | "serial.clear_input"
                | "serial.read_until"
                | "serial.exec"
                | "serial.close"
        );
        assert_eq!(
            destructive, expected_destructive,
            "tool `{name}` destructiveHint mismatch",
        );
    }
}

#[tokio::test]
async fn advertised_output_schemas_validate_real_success_timeout_and_error_results() {
    let list_response = roundtrip(
        tokio_server(),
        &[
            init_request(),
            initialized_notification(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ],
    )
    .await;
    let schemas: BTreeMap<String, Value> = list_response[1]["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().expect("tool name").to_string(),
                tool["outputSchema"].clone(),
            )
        })
        .collect();
    assert_eq!(schemas.len(), 11, "every advertised tool must be validated");

    let list_ports = handshake_then_call(tokio_server(), "serial.list_ports", json!({})).await;
    validate_tool_output(&schemas, "serial.list_ports", &list_ports);

    let open = handshake_then_call(
        stub_server(vec![]),
        "serial.open",
        json!({"port": "/dev/ttyUSB0"}),
    )
    .await;
    validate_tool_output(&schemas, "serial.open", &open);

    let (server, _session_id, _device) = server_with_open_session().await;
    let sessions = handshake_then_call(server, "serial.sessions", json!({})).await;
    validate_tool_output(&schemas, "serial.sessions", &sessions);

    let (server, session_id, _device) = server_with_open_session().await;
    let get_session = handshake_then_call(
        server,
        "serial.get_session",
        json!({"session_id": session_id}),
    )
    .await;
    validate_tool_output(&schemas, "serial.get_session", &get_session);

    let (server, session_id, _device) = server_with_open_session().await;
    let write = handshake_then_call(
        server,
        "serial.write",
        json!({"session_id": session_id, "data": "schema-write"}),
    )
    .await;
    validate_tool_output(&schemas, "serial.write", &write);

    let (server, session_id, _device) = server_with_open_session().await;
    let read_timeout = handshake_then_call(
        server,
        "serial.read",
        json!({"session_id": session_id, "max_bytes": 16, "timeout_ms": 20}),
    )
    .await;
    validate_tool_output(&schemas, "serial.read", &read_timeout);

    let (server, session_id, _device) = server_with_open_session().await;
    let drain = handshake_then_call(
        server,
        "serial.drain",
        json!({"session_id": session_id, "max_bytes": 16}),
    )
    .await;
    validate_tool_output(&schemas, "serial.drain", &drain);

    let (server, session_id, _device) = server_with_open_session().await;
    let clear = handshake_then_call(
        server,
        "serial.clear_input",
        json!({"session_id": session_id, "max_bytes": 16}),
    )
    .await;
    validate_tool_output(&schemas, "serial.clear_input", &clear);

    let (server, session_id, _device) = server_with_open_session().await;
    let read_until_timeout = handshake_then_call(
        server,
        "serial.read_until",
        json!({"session_id": session_id, "pattern": "NEVER", "timeout_ms": 20}),
    )
    .await;
    validate_tool_output(&schemas, "serial.read_until", &read_until_timeout);

    let (server, session_id, _device) = server_with_open_session().await;
    let exec_timeout = handshake_then_call(
        server,
        "serial.exec",
        json!({
            "session_id": session_id,
            "command": "schema-exec",
            "expect": "NEVER",
            "timeout_ms": 20,
        }),
    )
    .await;
    validate_tool_output(&schemas, "serial.exec", &exec_timeout);

    let (server, session_id, _device) = server_with_open_session().await;
    let close =
        handshake_then_call(server, "serial.close", json!({"session_id": session_id})).await;
    validate_tool_output(&schemas, "serial.close", &close);

    let structured_error = handshake_then_call(
        stub_server(vec![]),
        "serial.get_session",
        json!({"session_id": "missing-session"}),
    )
    .await;
    assert_eq!(structured_error["result"]["isError"], true);
    validate_tool_output(&schemas, "serial.get_session", &structured_error);
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
async fn open_by_port_succeeds_returns_32_char_hex_session_id() {
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
        32,
        "session_id must be 32-char hex; got '{session_id}'",
    );
    assert!(
        session_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
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
        tool_error_data(&resp)["name"].as_str(),
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
    assert_eq!(tool_error_data(&resp)["name"].as_str(), Some("port/device"),);
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
    let data = tool_error_data(&resp)
        .as_object()
        .expect("error.data object");
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
        tool_error_data(&resp)["device"].as_str(),
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
        privileged: false,
    };

    let resp = handshake_then_call(
        stub_server(vec![profile]),
        "serial.open",
        json!({"device": "step7-device"}),
    )
    .await;
    let result = resp
        .get("result")
        .unwrap_or_else(|| panic!("expected tools/call result for device-open, got {resp}"));
    assert_ne!(result.get("isError"), Some(&json!(true)));
    let structured = result
        .get("structuredContent")
        .and_then(Value::as_object)
        .expect("structuredContent must be a JSON object");
    let session_id = structured
        .get("session_id")
        .and_then(Value::as_str)
        .expect("session_id field");
    assert_eq!(session_id.len(), 32);
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
    server_with_open_session_policy(WritePolicy::Allow).await
}

/// Like [`server_with_open_session`] but opens the out-of-band session with an
/// explicit write policy, so wire tests can exercise the `deny` / `confirm`
/// gate in the `write` / `exec` handlers.
async fn server_with_open_session_policy(
    policy: WritePolicy,
) -> (McpServer<StubBackend>, String, DuplexStream) {
    let (server, sessions, backend) = stub_setup(vec![]);
    let session_id = sessions
        .open("/dev/ttyUSB0", 115_200, 5_000, policy)
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
        "serial.sessions",
        "serial.get_session",
        "serial.write",
        "serial.read",
        "serial.drain",
        "serial.clear_input",
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

// ---- write policy (deny / confirm) gating -------------------------------
//
// The gate lives in the `write` / `exec` handlers and runs before any bytes
// reach the device. `deny` is a hard refusal; `confirm` needs `confirm=true`.

/// Assert no byte crossed the duplex to the device. `handshake_then_call`
/// drops the server (and the manager's port half) when it returns, so the
/// device side is at EOF: `read` returns `Ok(0)` when nothing was written,
/// or the written bytes first if the gate had leaked. Either way `n == 0`
/// proves the write never reached the device.
async fn assert_device_untouched(device: &mut DuplexStream) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8];
    let n = timeout(Duration::from_millis(200), device.read(&mut buf))
        .await
        .expect("device read should not hang")
        .expect("device read");
    assert_eq!(n, 0, "a gated write must not send any bytes to the device");
}

#[tokio::test]
async fn write_denied_on_read_only_session_writes_no_bytes() {
    let (server, session_id, mut device) = server_with_open_session_policy(WritePolicy::Deny).await;
    let resp = handshake_then_call(
        server,
        "serial.write",
        json!({"session_id": session_id, "data": "reboot\n"}),
    )
    .await;
    assert_eq!(
        rpc_error_code(&resp),
        -32012,
        "deny → write_forbidden; got {resp}"
    );
    let error = &resp["result"]["structuredContent"]["error"];
    assert_eq!(error["type"], "write_forbidden");
    assert_eq!(error["retryable"], false);
    assert_eq!(error["command_written"], false);
    assert_eq!(
        error["session_usable"], true,
        "session stays usable — only the write was gated"
    );
    assert_eq!(tool_error_data(&resp)["write_policy"], "deny");
    assert_device_untouched(&mut device).await;
}

#[tokio::test]
async fn write_deny_ignores_confirm_true() {
    // `confirm=true` must NOT override a `deny` session — deny is a hard gate.
    let (server, session_id, mut device) = server_with_open_session_policy(WritePolicy::Deny).await;
    let resp = handshake_then_call(
        server,
        "serial.write",
        json!({"session_id": session_id, "data": "x", "confirm": true}),
    )
    .await;
    assert_eq!(
        rpc_error_code(&resp),
        -32012,
        "deny ignores confirm; got {resp}"
    );
    assert_device_untouched(&mut device).await;
}

#[tokio::test]
async fn exec_denied_on_read_only_session_writes_no_command() {
    let (server, session_id, mut device) = server_with_open_session_policy(WritePolicy::Deny).await;
    let resp = handshake_then_call(
        server,
        "serial.exec",
        json!({"session_id": session_id, "command": "ls\n", "expect": "\\$"}),
    )
    .await;
    assert_eq!(
        rpc_error_code(&resp),
        -32012,
        "deny → write_forbidden; got {resp}"
    );
    let error = &resp["result"]["structuredContent"]["error"];
    assert_eq!(error["type"], "write_forbidden");
    assert_eq!(error["command_written"], false);
    assert_device_untouched(&mut device).await;
}

#[tokio::test]
async fn write_confirm_required_without_confirm() {
    let (server, session_id, mut device) =
        server_with_open_session_policy(WritePolicy::Confirm).await;
    let resp = handshake_then_call(
        server,
        "serial.write",
        json!({"session_id": session_id, "data": "x"}),
    )
    .await;
    assert_eq!(
        rpc_error_code(&resp),
        -32013,
        "confirm w/o confirm → -32013; got {resp}"
    );
    let error = &resp["result"]["structuredContent"]["error"];
    assert_eq!(error["type"], "confirmation_required");
    assert_eq!(error["retryable"], false);
    assert_eq!(error["session_usable"], true);
    assert_device_untouched(&mut device).await;
}

#[tokio::test]
async fn write_confirm_with_confirm_true_succeeds() {
    let (server, session_id, mut device) =
        server_with_open_session_policy(WritePolicy::Confirm).await;
    let payload = "hello\n";
    let resp = handshake_then_call(
        server,
        "serial.write",
        json!({"session_id": session_id, "data": payload, "confirm": true}),
    )
    .await;
    let result = resp.get("result").expect("tools/call result");
    assert_ne!(
        result.get("isError"),
        Some(&json!(true)),
        "confirmed write must succeed; got {resp}"
    );
    assert_eq!(
        result["structuredContent"]["bytes_written"].as_u64(),
        Some(payload.len() as u64),
    );
    // The bytes actually crossed the duplex.
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; payload.len()];
    timeout(Duration::from_secs(2), device.read_exact(&mut buf))
        .await
        .expect("read timeout")
        .expect("read");
    assert_eq!(buf, payload.as_bytes());
}

#[tokio::test]
async fn open_applies_write_policy_param_end_to_end() {
    // Full wire `serial.open` with a `write_policy` override, verified via the
    // shared session manager (no hardware: StubBackend opens any allowlisted
    // path). Covers the open handler's policy computation + snapshot exposure.
    let (server, sessions, _backend) = stub_setup(vec![]);
    let resp = handshake_then_call(
        server.clone(),
        "serial.open",
        json!({"port": "/dev/ttyUSB0", "write_policy": "deny"}),
    )
    .await;
    let session_id = resp["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    assert_eq!(
        sessions.write_policy(&session_id).unwrap(),
        WritePolicy::Deny
    );
    assert_eq!(
        sessions.get_session(&session_id).unwrap().write_policy,
        WritePolicy::Deny,
        "get_session snapshot surfaces the policy",
    );
}

#[tokio::test]
async fn sessions_and_get_session_report_open_session_metadata() {
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
                "params": {"name": "serial.sessions", "arguments": {}},
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "serial.get_session",
                    "arguments": {"session_id": session_id},
                },
            }),
        ],
    )
    .await;

    let sessions = responses[1]["result"]["structuredContent"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["state"], "Ready");
    assert_eq!(sessions[0]["port"], "/dev/ttyUSB0");

    let session = &responses[2]["result"]["structuredContent"]["session"];
    assert_eq!(session["state"], "Ready");
    assert_eq!(session["baud"], 115_200);
}

#[tokio::test]
async fn drain_returns_raw_buffer() {
    let (server, session_id, mut device) = server_with_open_session().await;
    use tokio::io::AsyncWriteExt as _;
    device
        .write_all(b"\x1b]3008;start=test\x1b\\prompt> stale\n")
        .await
        .expect("device write");
    device.flush().await.expect("flush");

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
                    "name": "serial.drain",
                    "arguments": {"session_id": session_id, "max_bytes": 256},
                },
            }),
        ],
    )
    .await;
    let drained = responses[1]["result"]["structuredContent"]["data"]
        .as_str()
        .expect("drain data");
    assert!(
        drained.contains("\u{1b}]3008"),
        "raw OSC sequence must be preserved"
    );
}

#[tokio::test]
async fn clear_input_discards_buffer_and_reports_counts() {
    let (server, session_id, mut device) = server_with_open_session().await;
    use tokio::io::AsyncWriteExt as _;
    device
        .write_all(b"discard-me\n")
        .await
        .expect("device write");
    device.flush().await.expect("flush");

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
                    "name": "serial.clear_input",
                    "arguments": {"session_id": session_id, "max_bytes": 256},
                },
            }),
        ],
    )
    .await;
    let structured = &responses[1]["result"]["structuredContent"];
    assert_eq!(structured["discarded_bytes"], b"discard-me\n".len());
    assert_eq!(structured["bytes_read"], b"discard-me\n".len());
    assert_eq!(structured["session_usable"], true);
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
        tool_error_data(&resp)["name"].as_str(),
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
        tool_error_data(&resp)["session_id"].as_str(),
        Some("0000000000000000"),
    );
}

#[tokio::test]
async fn partial_write_error_reports_ambiguous_device_side_effect() {
    let (server, _sessions, backend, session_id) =
        fault_server_with_open_session(FaultBehavior::PartialWriteThenError).await;

    let resp = handshake_then_call(
        server,
        "serial.write",
        json!({"session_id": session_id, "data": "abcdef"}),
    )
    .await;

    assert_tool_io_error(&resp, &session_id, true, false);
    assert_eq!(
        backend.written(),
        b"abc",
        "fault must occur only after a real partial write"
    );
}

#[tokio::test]
async fn flush_error_after_write_reports_ambiguous_device_side_effect() {
    let (server, _sessions, backend, session_id) =
        fault_server_with_open_session(FaultBehavior::FlushError).await;

    let resp = handshake_then_call(
        server,
        "serial.write",
        json!({"session_id": session_id, "data": "write-before-flush"}),
    )
    .await;

    assert_tool_io_error(&resp, &session_id, true, false);
    assert_eq!(
        backend.written(),
        b"write-before-flush",
        "the complete payload must reach the port before flush fails"
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
async fn read_error_after_consumption_reports_ambiguous_consumption() {
    let (server, _sessions, _backend, session_id) =
        fault_server_with_open_session(FaultBehavior::ReadThenError).await;

    let resp = handshake_then_call(
        server,
        "serial.read",
        json!({"session_id": session_id, "max_bytes": 64, "timeout_ms": 500}),
    )
    .await;

    assert_tool_io_error(&resp, &session_id, false, true);
}

#[tokio::test]
async fn drain_and_clear_errors_after_consumption_report_ambiguous_consumption() {
    for tool in ["serial.drain", "serial.clear_input"] {
        let (server, _sessions, _backend, session_id) =
            fault_server_with_open_session(FaultBehavior::ReadThenError).await;

        let resp = handshake_then_call(
            server,
            tool,
            json!({"session_id": session_id, "max_bytes": 64}),
        )
        .await;

        assert_tool_io_error(&resp, &session_id, false, true);
    }
}

#[tokio::test]
async fn read_timeout_returns_empty_data_with_timed_out_true_not_error() {
    // Issue #4: an idle-port deadline is a domain outcome (parity with
    // serial.read_until), NOT a JSON-RPC error. The call must return a
    // successful tool result with `data=""` and `timed_out=true`, just
    // like read_until returns `matched=false`.
    let (server, session_id, _device) = server_with_open_session().await;

    let resp = handshake_then_call(
        server,
        "serial.read",
        json!({"session_id": session_id, "max_bytes": 64, "timeout_ms": 100}),
    )
    .await;

    let result = resp.get("result").unwrap_or_else(|| {
        panic!("read timeout must return a tool result, not a JSON-RPC error: {resp}")
    });
    assert_ne!(
        result.get("isError"),
        Some(&json!(true)),
        "read timeout must NOT set isError=true",
    );
    assert!(
        resp.get("error").is_none(),
        "read timeout must not be a JSON-RPC error"
    );
    let structured = &result["structuredContent"];
    assert_eq!(structured["data"], json!(""));
    assert_eq!(structured["timed_out"], json!(true));
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
    assert!(
        data.contains("WORLD"),
        "data should contain match: {data:?}"
    );
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
    assert!(
        resp.get("error").is_none(),
        "timeout must not be a JSON-RPC error"
    );
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
    device
        .write_all(b"untouched\n")
        .await
        .expect("device write");
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
        tool_error_data(read_until_resp)["name"].as_str(),
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
    assert_eq!(tool_error_data(&resp)["name"].as_str(), Some("pattern"));
}

// ---- serial.exec ---------------------------------------------------------
//
// `serial.exec` composes `write` then `read_until`. The behavioural
// guard the spec pins down is "validation BEFORE write" — bad expect
// regex / empty expect / oversized command MUST NOT mutate device
// state. Several tests use the same "pre-fill device side, fail exec,
// then follow up with serial.read on the same session" pattern to
// prove no bytes leaked through.

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
    device
        .write_all(b"untouched\n")
        .await
        .expect("device write");
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
    assert_eq!(tool_error_data(exec_resp)["name"].as_str(), Some("command"));

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
    assert_eq!(tool_error_data(&resp)["name"].as_str(), Some("expect"));
}

#[tokio::test]
async fn exec_invalid_regex_expect_returns_invalid_param_without_writing() {
    let (server, session_id, mut device) = server_with_open_session().await;
    use tokio::io::AsyncWriteExt as _;
    device
        .write_all(b"still-there\n")
        .await
        .expect("device write");
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
    assert_eq!(tool_error_data(exec_resp)["name"].as_str(), Some("expect"));

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
        n,
        0,
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
    assert!(
        resp.get("error").is_none(),
        "timeout must not be a JSON-RPC error"
    );
    let structured = &result["structuredContent"];
    assert_eq!(structured["ok"], json!(false));
    assert!(structured["output"].is_string());
}

#[tokio::test]
async fn exec_uses_session_default_timeout_when_omitted() {
    let (server, sessions, backend) = stub_setup(vec![]);
    let session_id = sessions
        .open("/dev/ttyUSB0", 115_200, 75, WritePolicy::Allow)
        .await
        .expect("open out-of-band");
    let _device = backend.take_device();

    let started = Instant::now();
    let resp = handshake_then_call(
        server,
        "serial.exec",
        json!({
            "session_id": session_id,
            "command": "ping",
            "expect": "NEVER_THIS_PATTERN"
        }),
    )
    .await;

    assert!(
        started.elapsed() < Duration::from_millis(800),
        "omitted exec timeout should use the short session default, not the global default"
    );
    let result = resp.get("result").expect("tools/call result");
    assert_ne!(result.get("isError"), Some(&json!(true)));
    assert_eq!(result["structuredContent"]["status"], json!("timed_out"));
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
        tool_error_data(&resp)["session_id"].as_str(),
        Some("deadbeefdeadbeef"),
    );
}

#[tokio::test]
async fn exec_disconnect_after_command_write_reports_ambiguous_write_and_consumption() {
    let (server, _sessions, backend, session_id) =
        fault_server_with_open_session(FaultBehavior::ReadThenError).await;
    let command = "run-before-disconnect";

    let resp = handshake_then_call(
        server,
        "serial.exec",
        json!({
            "session_id": session_id,
            "command": command,
            "expect": "NEVER_MATCH",
            "timeout_ms": 500,
        }),
    )
    .await;

    assert_tool_io_error(&resp, &session_id, true, true);
    assert_eq!(
        backend.written(),
        command.as_bytes(),
        "exec command must have been written before the injected disconnect"
    );
}

#[tokio::test]
async fn exec_write_and_flush_deadlines_are_bounded_and_report_configured_timeout() {
    for behavior in [FaultBehavior::WritePending, FaultBehavior::FlushPending] {
        let (server, _sessions, backend, session_id) =
            fault_server_with_open_session(behavior).await;
        let configured_timeout_ms = 25;
        let started = Instant::now();

        let resp = handshake_then_call(
            server,
            "serial.exec",
            json!({
                "session_id": session_id,
                "command": "deadline-command",
                "expect": "NEVER_MATCH",
                "timeout_ms": configured_timeout_ms,
            }),
        )
        .await;

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "pending write/flush phase exceeded its bounded operation deadline"
        );
        assert_tool_timeout_error(&resp, &session_id, configured_timeout_ms);

        if matches!(behavior, FaultBehavior::FlushPending) {
            assert_eq!(
                backend.written(),
                b"deadline-command",
                "flush timeout must occur after the command write"
            );
        } else {
            assert!(
                backend.written().is_empty(),
                "pending write must not claim observed port progress"
            );
        }
    }
}

#[tokio::test]
async fn exec_lock_deadline_is_bounded_and_reports_configured_timeout() {
    let (server, sessions, backend, session_id) =
        fault_server_with_open_session(FaultBehavior::ReadPending).await;
    let cancellation = CancellationToken::new();
    let read_sessions = sessions.clone();
    let read_session_id = session_id.clone();
    let read_cancellation = cancellation.clone();
    let read_task = tokio::spawn(async move {
        read_sessions
            .read_with_cancel(&read_session_id, 64, 1_000, read_cancellation)
            .await
    });
    backend.wait_for_read().await;

    let configured_timeout_ms = 25;
    let started = Instant::now();
    let resp = handshake_then_call(
        server,
        "serial.exec",
        json!({
            "session_id": session_id,
            "command": "must-wait-for-lock",
            "expect": "NEVER_MATCH",
            "timeout_ms": configured_timeout_ms,
        }),
    )
    .await;

    assert!(
        started.elapsed() < Duration::from_millis(500),
        "lock wait exceeded its bounded operation deadline"
    );
    assert_tool_timeout_error(&resp, &session_id, configured_timeout_ms);
    assert!(
        backend.written().is_empty(),
        "lock timeout must occur before any port write"
    );

    cancellation.cancel();
    read_task
        .await
        .expect("read task join")
        .expect("cancelled read returns a normal outcome");
}

// ---- serial.close --------------------------------------------------------

#[tokio::test]
async fn close_happy_path_returns_ok_true() {
    let (server, session_id, _device) = server_with_open_session().await;
    let resp = handshake_then_call(server, "serial.close", json!({"session_id": session_id})).await;
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
        tool_error_data(&resp)["session_id"].as_str(),
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
    assert_eq!(
        responses.len(),
        4,
        "init + close + read + write = 4 responses"
    );

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

// ---- tool-call journal ---------------------------------------------------
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
        .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
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
async fn journal_write_summary_records_size_without_payload() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();
    drop(tmp);

    let (server, sessions, _backend) = stub_server_with_journal(&path).await;
    let session_id = sessions
        .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
        .await
        .expect("open");

    // Build a non-trivial `data` payload under MAX_WRITE_CHUNK so the call
    // itself succeeds while the journal records only metadata.
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
                    "arguments": {"session_id": session_id, "data": big.clone(), "confirm": true},
                },
            }),
        ],
    )
    .await;

    let entries = read_journal(&path);
    assert_eq!(entries.len(), 2);
    let call_summary = &entries[0]["summary"];
    assert_eq!(call_summary["bytes"], 200, "byte count preserved");
    assert_eq!(
        call_summary["confirm"], true,
        "confirm flag recorded as metadata",
    );
    assert_eq!(
        call_summary.get("head"),
        None,
        "default journal must not store command/data payload heads",
    );
    assert!(
        !call_summary.to_string().contains(&big),
        "journal summary must not contain the payload"
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
    assert_eq!(
        sid.len(),
        32,
        "expected 32-char hex session_id, got {sid:?}"
    );
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

// ---- concurrency hardening -----------------------------------------------
//
// These tests exercise rmcp's concurrent dispatch — the SDK runs each
// `tools/call` on its own tokio::task::JoinSet entry, so requests in a
// single client batch can be IN-FLIGHT simultaneously on the server.
// A plain serial stdin line-loop would not have this property; rmcp's
// dispatch does, so these tests pin the behaviour:
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
async fn open_n_sessions(sessions: &SessionManager<StubBackend>, n: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(n);
    for idx in 0..n {
        let sid = sessions
            .open(
                &format!("/dev/ttyUSB{idx}"),
                115_200,
                5_000,
                WritePolicy::Allow,
            )
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

    // close: there is exactly ONE close call here, against a Ready
    // session. The competing read_until checks out the port via
    // Arc clone but does NOT remove the session from the map. So
    // close has no legitimate reason to fail — a -32003 here would
    // indicate a real manager bug, not acceptable race-nondeterminism.
    let close_resp = responses.iter().find(|r| r["id"] == 12).unwrap();
    assert!(
        close_resp.get("error").is_none(),
        "close on a Ready session must succeed even with an in-flight \
         read_until; got error response {close_resp}",
    );
    assert_eq!(
        close_resp["result"]["structuredContent"]["ok"],
        json!(true),
        "close result must report ok=true; got {close_resp}",
    );

    // Post-race read in the same batch is deterministic in safety terms:
    // it must not perform I/O. Depending on dispatch ordering it may observe
    // either Closing or the already-removed session.
    let post_read = responses.iter().find(|r| r["id"] == 13).unwrap();
    assert!(
        matches!(rpc_error_code(post_read), -32003 | -32004),
        "read queued after close begins must fail without I/O; got {post_read}",
    );
    assert_eq!(sessions.session_count(), 0);
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
            panic!("line {i} is not valid JSON — likely torn/interleaved: {e}\nLINE: {line:?}",)
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
    let calls = entries.iter().filter(|e| e["direction"] == "call").count();
    let results = entries
        .iter()
        .filter(|e| e["direction"] == "result")
        .count();
    assert_eq!(calls, N);
    assert_eq!(results, N);
}
