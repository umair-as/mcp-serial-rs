//! MCP method dispatch.
//!
//! Routes a parsed JSON-RPC [`Request`] to its handler. Lifecycle methods
//! (`initialize`, `tools/list`, `notifications/initialized`) are pure and
//! synchronous; `serial.*` handlers consult [`State`], which bundles the
//! [`SessionManager`] and the loaded device profiles.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::instrument;

use crate::config::{self, DeviceProfile};
use crate::errors::SerialError;
use crate::protocol::{self, Request, Response};
use crate::serial::SerialBackend;
use crate::serial::manager::SessionManager;

const SERVER_NAME: &str = "mcp-serial-rs";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Per-process state passed to every handler. Cloning a `State` clones two
/// `Arc`s — cheap, safe to share across tokio tasks.
pub struct State<B: SerialBackend> {
    pub sessions: Arc<SessionManager<B>>,
    pub profiles: Arc<Vec<DeviceProfile>>,
}

impl<B: SerialBackend> State<B> {
    pub fn new(sessions: Arc<SessionManager<B>>, profiles: Arc<Vec<DeviceProfile>>) -> Self {
        Self { sessions, profiles }
    }
}

/// Dispatch a single request. Returns `None` for notifications and other
/// id-less inputs (no JSON-RPC reply by spec).
#[instrument(skip(state, req), fields(method = %req.method))]
pub async fn dispatch<B: SerialBackend>(state: &State<B>, req: Request) -> Option<Response> {
    let id = req.id.clone();

    if req.method == "notifications/initialized" {
        return None;
    }

    // `serial.*` methods mutate server/device state, so id-less invocations are
    // ignored before any side effects run.
    if id.is_none() && req.method.starts_with("serial.") {
        return None;
    }

    let result: Result<Value, protocol::Error> = if req.jsonrpc != protocol::JSONRPC_VERSION {
        Err(protocol::Error::new(
            protocol::INVALID_REQUEST,
            format!("unsupported jsonrpc version '{}', expected '2.0'", req.jsonrpc),
        ))
    } else {
        match req.method.as_str() {
            "initialize" => Ok(initialize()),
            "tools/list" => Ok(tools_list()),
            "serial.list_ports" => handle_list_ports(state, req.params),
            "serial.open" => handle_open(state, req.params).await,
            "serial.close" => handle_close(state, req.params),
            "serial.write" => handle_write(state, req.params).await,
            "serial.read" => handle_read(state, req.params).await,
            "serial.read_until" => handle_read_until(state, req.params).await,
            "serial.exec" => handle_exec(state, req.params).await,
            other => Err(protocol::Error::new(
                protocol::METHOD_NOT_FOUND,
                format!("method '{other}' not implemented"),
            )),
        }
    };

    // Notifications (no `id`) get no reply by JSON-RPC 2.0 — the method's
    // side effects (if any) still ran, we just suppress the response object.
    let response_id = id?;

    Some(match result {
        Ok(value) => Response::success(response_id, value),
        Err(err) => Response::failure(response_id, err),
    })
}

fn initialize() -> Value {
    json!({
        "name": SERVER_NAME,
        "version": SERVER_VERSION,
        "capabilities": { "tools": {} },
    })
}

fn tools_list() -> Value {
    json!([
        {
            "name": "serial.list_ports",
            "description": "Enumerate available serial ports filtered by the configured allowlist, enriched with device profile matches.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "serial.open",
            "description": "Open an allowlisted serial port and return a session_id. Accepts either a raw 'port' path or a profile 'device' name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "port": { "type": "string", "description": "Device path, e.g. /dev/ttyUSB1" },
                    "device": { "type": "string", "description": "Profile name from devices.toml, e.g. esp32c6" },
                    "baud": { "type": "integer", "minimum": 1, "default": 115200 },
                    "timeout_ms": { "type": "integer", "minimum": 0, "default": 5000 }
                },
                "additionalProperties": false,
                "oneOf": [
                    { "required": ["port"] },
                    { "required": ["device"] }
                ]
            }
        },
        {
            "name": "serial.write",
            "description": "Write UTF-8 data to an open session.",
            "inputSchema": {
                "type": "object",
                "required": ["session_id", "data"],
                "properties": {
                    "session_id": { "type": "string" },
                    "data": { "type": "string" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "serial.read",
            "description": "Read up to max_bytes from the session, returning UTF-8 data.",
            "inputSchema": {
                "type": "object",
                "required": ["session_id"],
                "properties": {
                    "session_id": { "type": "string" },
                    "max_bytes": { "type": "integer", "minimum": 1, "default": 4096 },
                    "timeout_ms": { "type": "integer", "minimum": 0, "default": 5000 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "serial.read_until",
            "description": "Read until the regex pattern matches, or timeout.",
            "inputSchema": {
                "type": "object",
                "required": ["session_id", "pattern"],
                "properties": {
                    "session_id": { "type": "string" },
                    "pattern": { "type": "string", "description": "Rust regex" },
                    "timeout_ms": { "type": "integer", "minimum": 0, "default": 5000 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "serial.exec",
            "description": "Write a command to the session, then read until the expect regex matches or timeout. Returns {output, ok}.",
            "inputSchema": {
                "type": "object",
                "required": ["session_id", "command", "expect"],
                "properties": {
                    "session_id": { "type": "string" },
                    "command": { "type": "string", "description": "UTF-8 bytes to write; caller controls any trailing newline" },
                    "expect": { "type": "string", "description": "Rust regex pattern matched against the accumulating read buffer" },
                    "timeout_ms": { "type": "integer", "minimum": 0, "default": 5000 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "serial.close",
            "description": "Close the session and release the port.",
            "inputSchema": {
                "type": "object",
                "required": ["session_id"],
                "properties": {
                    "session_id": { "type": "string" }
                },
                "additionalProperties": false
            }
        }
    ])
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenParams {
    #[serde(default)]
    port: Option<String>,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    baud: Option<u32>,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    config::DEFAULT_TIMEOUT_MS
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseParams {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteParams {
    session_id: String,
    data: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadParams {
    session_id: String,
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadUntilParams {
    session_id: String,
    pattern: String,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecParams {
    session_id: String,
    command: String,
    expect: String,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

fn default_max_bytes() -> usize {
    4096
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, protocol::Error> {
    serde_json::from_value(params).map_err(|e| {
        protocol::Error::new(protocol::INVALID_PARAMS, format!("invalid params: {e}"))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

fn handle_list_ports<B: SerialBackend>(
    state: &State<B>,
    params: Value,
) -> Result<Value, protocol::Error> {
    let _: EmptyParams = match params {
        Value::Null => EmptyParams {},
        other => parse_params(other)?,
    };
    let ports = crate::serial::list_ports(&state.profiles).map_err(serial_error_to_protocol)?;
    Ok(json!(ports))
}

async fn handle_open<B: SerialBackend>(
    state: &State<B>,
    params: Value,
) -> Result<Value, protocol::Error> {
    let p: OpenParams = parse_params(params)?;

    let (port_path, baud) = match (p.port, p.device) {
        (Some(_), Some(_)) => {
            return Err(protocol::Error::new(
                protocol::INVALID_PARAMS,
                "specify 'device' or 'port', not both",
            ));
        }
        (None, None) => {
            return Err(protocol::Error::new(
                protocol::INVALID_PARAMS,
                "missing required parameter: 'port' or 'device'",
            ));
        }
        (Some(port), None) => (port, p.baud.unwrap_or(config::DEFAULT_BAUD)),
        (None, Some(device_name)) => {
            let (path, profile_baud) = resolve_device(&device_name, &state.profiles)?;
            (path, p.baud.unwrap_or(profile_baud))
        }
    };

    let timeout_ms = p.timeout_ms.min(config::MAX_TIMEOUT_MS);
    let session_id = state
        .sessions
        .open(&port_path, baud, timeout_ms)
        .await
        .map_err(serial_error_to_protocol)?;
    Ok(json!({ "session_id": session_id }))
}

/// Resolve a device-profile name to (port_path, default_baud). Profile name
/// lookup is checked first — unknown names short-circuit before touching
/// `available_ports`, so the error path stays cheap and testable.
fn resolve_device(
    device_name: &str,
    profiles: &[DeviceProfile],
) -> Result<(String, u32), protocol::Error> {
    let profile = profiles.iter().find(|p| p.name == device_name).ok_or_else(|| {
        serial_error_to_protocol(SerialError::DeviceNotFound {
            device: device_name.to_string(),
        })
    })?;
    let raw = tokio_serial::available_ports().map_err(|e| {
        serial_error_to_protocol(SerialError::Io {
            message: format!("available_ports: {e}"),
        })
    })?;
    let ports = crate::serial::filter_allowlisted(raw);
    let port_path = ports
        .iter()
        .find(|port| crate::serial::profile_matches_port(profile, port))
        .map(|port| port.port.clone())
        .ok_or_else(|| {
            serial_error_to_protocol(SerialError::DeviceNotFound {
                device: device_name.to_string(),
            })
        })?;
    Ok((port_path, profile.baud))
}

fn handle_close<B: SerialBackend>(
    state: &State<B>,
    params: Value,
) -> Result<Value, protocol::Error> {
    let p: CloseParams = parse_params(params)?;
    state
        .sessions
        .close(&p.session_id)
        .map_err(serial_error_to_protocol)?;
    Ok(json!({ "ok": true }))
}

async fn handle_write<B: SerialBackend>(
    state: &State<B>,
    params: Value,
) -> Result<Value, protocol::Error> {
    let p: WriteParams = parse_params(params)?;
    if p.data.len() > config::MAX_WRITE_CHUNK {
        return Err(protocol::Error::new(
            protocol::INVALID_PARAMS,
            format!(
                "'data' is {} bytes; max per write is {}",
                p.data.len(),
                config::MAX_WRITE_CHUNK
            ),
        ));
    }
    let bytes = state
        .sessions
        .write(&p.session_id, p.data.as_bytes())
        .await
        .map_err(serial_error_to_protocol)?;
    Ok(json!({ "bytes_written": bytes }))
}

async fn handle_read<B: SerialBackend>(
    state: &State<B>,
    params: Value,
) -> Result<Value, protocol::Error> {
    let p: ReadParams = parse_params(params)?;
    let timeout_ms = p.timeout_ms.min(config::MAX_TIMEOUT_MS);
    let max_bytes = p.max_bytes.min(config::MAX_READ_BUFFER);
    let data = state
        .sessions
        .read(&p.session_id, max_bytes, timeout_ms)
        .await
        .map_err(serial_error_to_protocol)?;
    Ok(json!({ "data": String::from_utf8_lossy(&data) }))
}

async fn handle_read_until<B: SerialBackend>(
    state: &State<B>,
    params: Value,
) -> Result<Value, protocol::Error> {
    let p: ReadUntilParams = parse_params(params)?;
    let timeout_ms = p.timeout_ms.min(config::MAX_TIMEOUT_MS);
    let (data, matched) = state
        .sessions
        .read_until(&p.session_id, &p.pattern, timeout_ms)
        .await
        .map_err(serial_error_to_protocol)?;
    Ok(json!({
        "data": String::from_utf8_lossy(&data),
        "matched": matched,
    }))
}

async fn handle_exec<B: SerialBackend>(
    state: &State<B>,
    params: Value,
) -> Result<Value, protocol::Error> {
    let p: ExecParams = parse_params(params)?;
    if p.command.len() > config::MAX_WRITE_CHUNK {
        return Err(protocol::Error::new(
            protocol::INVALID_PARAMS,
            format!(
                "'command' is {} bytes; max per write is {}",
                p.command.len(),
                config::MAX_WRITE_CHUNK
            ),
        ));
    }
    // Pre-validate `expect` BEFORE writing — read_until's regex compile happens
    // post-write, so without this guard an exec with bad regex would mutate the
    // device (or remote shell) and only then surface InvalidParam.
    if p.expect.is_empty() {
        return Err(serial_error_to_protocol(SerialError::InvalidParam {
            name: "expect".into(),
            reason: "must not be empty".into(),
        }));
    }
    if let Err(e) = regex::Regex::new(&p.expect) {
        return Err(serial_error_to_protocol(SerialError::InvalidParam {
            name: "expect".into(),
            reason: format!("invalid regex: {e}"),
        }));
    }
    let timeout_ms = p.timeout_ms.min(config::MAX_TIMEOUT_MS);

    // Write first; on failure, surface immediately rather than swallowing
    // into a timed-out read_until. The session-state check is duplicated
    // across the two calls but is cheap, and keeps composition honest —
    // a concurrent close between write and read_until is reported, not hidden.
    state
        .sessions
        .write(&p.session_id, p.command.as_bytes())
        .await
        .map_err(serial_error_to_protocol)?;

    let (data, matched) = state
        .sessions
        .read_until(&p.session_id, &p.expect, timeout_ms)
        .await
        .map_err(serial_error_to_protocol)?;
    Ok(json!({
        "output": String::from_utf8_lossy(&data),
        "ok": matched,
    }))
}

fn serial_error_to_protocol(err: SerialError) -> protocol::Error {
    err.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::serial::manager::TokioSerialBackend;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;
    use std::sync::PoisonError;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    fn req(method: &str, id: Option<Value>) -> Request {
        Request {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params: json!({}),
        }
    }

    /// State for tests that don't actually need to open a port.
    fn mock_state() -> State<TokioSerialBackend> {
        mock_state_with_profiles(vec![])
    }

    fn mock_state_with_profiles(profiles: Vec<DeviceProfile>) -> State<TokioSerialBackend> {
        State {
            sessions: Arc::new(SessionManager::with_seed(TokioSerialBackend, 0xCAFE)),
            profiles: Arc::new(profiles),
        }
    }

    struct CountingBackend {
        opens: StdArc<AtomicUsize>,
    }

    impl crate::serial::SerialBackend for CountingBackend {
        type Port = tokio::io::Join<tokio::io::Empty, tokio::io::Sink>;

        async fn open(&self, _port: &str, _baud: u32) -> Result<Self::Port, SerialError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(tokio::io::join(tokio::io::empty(), tokio::io::sink()))
        }
    }

    /// Backend that returns a real `DuplexStream` so tests can drive the
    /// "device" side. Each open stashes the device half on a shared vec;
    /// `take_device` pops the most-recently-opened half.
    #[derive(Clone)]
    struct DuplexBackend {
        sides: StdArc<StdMutex<Vec<DuplexStream>>>,
    }

    impl DuplexBackend {
        fn new() -> Self {
            Self {
                sides: StdArc::new(StdMutex::new(Vec::new())),
            }
        }

        fn take_device(&self) -> DuplexStream {
            self.sides
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop()
                .expect("no device side available")
        }
    }

    impl crate::serial::SerialBackend for DuplexBackend {
        type Port = DuplexStream;

        async fn open(&self, _port: &str, _baud: u32) -> Result<DuplexStream, SerialError> {
            let (manager_side, device_side) = tokio::io::duplex(4096);
            self.sides
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(device_side);
            Ok(manager_side)
        }
    }

    #[tokio::test]
    async fn initialize_returns_name_version_capabilities() {
        let s = mock_state();
        let resp = dispatch(&s, req("initialize", Some(json!(1))))
            .await
            .expect("response");
        let result = resp.result.expect("result present");
        assert_eq!(result["name"], "mcp-serial-rs");
        assert_eq!(result["version"], SERVER_VERSION);
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_lists_all_six_mvp_tools() {
        let s = mock_state();
        let resp = dispatch(&s, req("tools/list", Some(json!(2))))
            .await
            .expect("response");
        let result = resp.result.expect("result present");
        let arr = result.as_array().expect("array");
        let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 7);
        for expected in [
            "serial.list_ports",
            "serial.open",
            "serial.write",
            "serial.read",
            "serial.read_until",
            "serial.exec",
            "serial.close",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    #[tokio::test]
    async fn notifications_initialized_returns_none() {
        let s = mock_state();
        assert!(
            dispatch(&s, req("notifications/initialized", None))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn id_less_initialize_returns_none() {
        let s = mock_state();
        assert!(dispatch(&s, req("initialize", None)).await.is_none());
    }

    #[tokio::test]
    async fn id_less_tools_list_returns_none() {
        let s = mock_state();
        assert!(dispatch(&s, req("tools/list", None)).await.is_none());
    }

    #[tokio::test]
    async fn id_less_unknown_method_returns_none() {
        let s = mock_state();
        assert!(dispatch(&s, req("nope", None)).await.is_none());
    }

    #[tokio::test]
    async fn id_less_serial_open_is_suppressed_before_side_effects() {
        let opens = StdArc::new(AtomicUsize::new(0));
        let s = State {
            sessions: Arc::new(SessionManager::with_seed(
                CountingBackend {
                    opens: opens.clone(),
                },
                0xCAFE,
            )),
            profiles: Arc::new(vec![]),
        };
        let mut r = req("serial.open", None);
        r.params = json!({ "port": "/dev/ttyUSB0", "baud": 115200, "timeout_ms": 1000 });

        assert!(dispatch(&s, r).await.is_none());
        assert_eq!(opens.load(Ordering::SeqCst), 0, "backend open must not be called");
        assert_eq!(s.sessions.session_count(), 0, "no session should be created");
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let s = mock_state();
        let resp = dispatch(&s, req("nope", Some(json!(3))))
            .await
            .expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn wrong_jsonrpc_version_returns_invalid_request() {
        let s = mock_state();
        let mut r = req("initialize", Some(json!(5)));
        r.jsonrpc = "1.0".into();
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn wrong_jsonrpc_version_notification_is_silent() {
        let s = mock_state();
        let mut r = req("initialize", None);
        r.jsonrpc = "1.0".into();
        assert!(dispatch(&s, r).await.is_none());
    }

    // --- serial.open params --- //

    #[tokio::test]
    async fn serial_open_with_missing_port_and_device_returns_invalid_params() {
        let s = mock_state();
        let r = req("serial.open", Some(json!(6)));
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert!(
            err.message.contains("'port' or 'device'"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn serial_open_with_both_port_and_device_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.open", Some(json!(7)));
        r.params = json!({ "port": "/dev/ttyUSB0", "device": "esp32c6" });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert!(
            err.message.contains("not both"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn serial_open_with_unknown_device_returns_device_not_found() {
        // No profiles loaded → any device name is unknown → -32009.
        let s = mock_state();
        let mut r = req("serial.open", Some(json!(8)));
        r.params = json!({ "device": "esp32c6" });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, -32009, "DeviceNotFound code");
        assert_eq!(err.data.as_ref().unwrap()["device"], "esp32c6");
    }

    // --- the rest --- //

    #[tokio::test]
    async fn serial_close_with_extra_field_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.close", Some(json!(9)));
        r.params = json!({ "session_id": "abc", "rogue": 1 });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn serial_write_with_missing_data_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.write", Some(json!(10)));
        r.params = json!({ "session_id": "abc" });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn serial_write_oversized_data_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.write", Some(json!(11)));
        r.params = json!({
            "session_id": "abc",
            "data": "x".repeat(config::MAX_WRITE_CHUNK + 1),
        });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert!(err.message.contains("max per write"));
    }

    #[tokio::test]
    async fn serial_read_with_extra_field_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.read", Some(json!(12)));
        r.params = json!({ "session_id": "abc", "bogus": true });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn serial_read_unknown_session_returns_session_not_found_code() {
        let s = mock_state();
        let mut r = req("serial.read", Some(json!(13)));
        r.params = json!({ "session_id": "deadbeefdeadbeef", "max_bytes": 16, "timeout_ms": 5 });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, -32003);
    }

    #[tokio::test]
    async fn serial_list_ports_with_extra_field_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.list_ports", Some(json!(20)));
        r.params = json!({ "rogue": true });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn serial_list_ports_returns_array_shape() {
        let s = mock_state();
        let r = req("serial.list_ports", Some(json!(21)));
        let resp = dispatch(&s, r).await.expect("response");
        let result = resp.result.expect("result present");
        assert!(result.is_array(), "expected array, got: {result}");
    }

    #[tokio::test]
    async fn serial_read_until_with_missing_pattern_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.read_until", Some(json!(14)));
        r.params = json!({ "session_id": "abc" });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn serial_read_until_unknown_session_returns_session_not_found_code() {
        let s = mock_state();
        let mut r = req("serial.read_until", Some(json!(15)));
        r.params = json!({ "session_id": "deadbeefdeadbeef", "pattern": "x", "timeout_ms": 5 });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, -32003);
    }

    // --- serial.exec --- //

    #[tokio::test]
    async fn serial_exec_with_missing_expect_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.exec", Some(json!(30)));
        r.params = json!({ "session_id": "abc", "command": "help\n" });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn serial_exec_with_missing_command_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.exec", Some(json!(31)));
        r.params = json!({ "session_id": "abc", "expect": "prompt> " });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn serial_exec_with_extra_field_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.exec", Some(json!(32)));
        r.params = json!({
            "session_id": "abc",
            "command": "help\n",
            "expect": "prompt> ",
            "rogue": 1,
        });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn serial_exec_oversized_command_returns_invalid_params() {
        let s = mock_state();
        let mut r = req("serial.exec", Some(json!(33)));
        r.params = json!({
            "session_id": "abc",
            "command": "x".repeat(config::MAX_WRITE_CHUNK + 1),
            "expect": "prompt> ",
        });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert!(err.message.contains("max per write"));
    }

    #[tokio::test]
    async fn serial_exec_unknown_session_returns_session_not_found_code() {
        let s = mock_state();
        let mut r = req("serial.exec", Some(json!(34)));
        r.params = json!({
            "session_id": "deadbeefdeadbeef",
            "command": "help\n",
            "expect": "prompt> ",
            "timeout_ms": 5,
        });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, -32003);
    }

    #[tokio::test]
    async fn serial_exec_writes_command_and_returns_matched_output() {
        // End-to-end: open via dispatch on a duplex backend, spawn a task on
        // the device side that drains the command and writes a prompt, then
        // dispatch serial.exec and assert {output, ok} reflect read_until's
        // matched buffer.
        let backend = DuplexBackend::new();
        let s = State {
            sessions: Arc::new(SessionManager::with_seed(backend.clone(), 0xCAFE)),
            profiles: Arc::new(vec![]),
        };

        let mut open_req = req("serial.open", Some(json!(40)));
        open_req.params = json!({ "port": "/dev/ttyUSB0", "baud": 115200, "timeout_ms": 1000 });
        let open_resp = dispatch(&s, open_req).await.expect("response");
        let session_id = open_resp
            .result
            .expect("open result")
            .get("session_id")
            .and_then(Value::as_str)
            .expect("session_id string")
            .to_string();

        let mut device = backend.take_device();
        let device_task = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let n = device.read(&mut buf).await.unwrap();
            let received = buf[..n].to_vec();
            device.write_all(b"hello world\nprompt> ").await.unwrap();
            // Hold the device side open until the manager-side read finishes,
            // otherwise an early EOF could race the match.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            received
        });

        let mut exec_req = req("serial.exec", Some(json!(41)));
        exec_req.params = json!({
            "session_id": session_id,
            "command": "help\n",
            "expect": "prompt> ",
            "timeout_ms": 2000,
        });
        let exec_resp = dispatch(&s, exec_req).await.expect("response");
        let result = exec_resp.result.expect("exec result");
        assert_eq!(result["ok"], true, "matched flag should be true: {result}");
        let output = result["output"].as_str().expect("output string");
        assert!(
            output.contains("prompt> "),
            "output should contain the expect pattern: {output:?}"
        );

        let received = device_task.await.unwrap();
        assert_eq!(&received, b"help\n", "device should receive the verbatim command");
    }

    #[tokio::test]
    async fn serial_exec_invalid_regex_rejects_before_writing_command() {
        // Bad expect must NOT mutate device state — pre-validate before write.
        let backend = DuplexBackend::new();
        let s = State {
            sessions: Arc::new(SessionManager::with_seed(backend.clone(), 0xCAFE)),
            profiles: Arc::new(vec![]),
        };

        let mut open_req = req("serial.open", Some(json!(50)));
        open_req.params = json!({ "port": "/dev/ttyUSB0", "baud": 115200, "timeout_ms": 1000 });
        let open_resp = dispatch(&s, open_req).await.expect("response");
        let session_id = open_resp
            .result
            .expect("open result")
            .get("session_id")
            .and_then(Value::as_str)
            .expect("session_id string")
            .to_string();

        let mut device = backend.take_device();

        let mut exec_req = req("serial.exec", Some(json!(51)));
        exec_req.params = json!({
            "session_id": session_id,
            "command": "DESTRUCTIVE_OP\n",
            "expect": "(",          // unbalanced — regex compile error
            "timeout_ms": 200,
        });
        let exec_resp = dispatch(&s, exec_req).await.expect("response");
        let err = exec_resp.error.expect("error");
        assert_eq!(err.code, -32008, "InvalidParam code");
        assert_eq!(err.data.as_ref().unwrap()["name"], "expect");

        // Critical assertion: the device must not have received the command.
        // A short timed read on the device side should yield 0 bytes.
        let mut buf = [0u8; 64];
        let read = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            device.read(&mut buf),
        )
        .await;
        match read {
            Err(_) => { /* timeout — expected, nothing was written */ }
            Ok(Ok(0)) => { /* EOF — also fine, nothing written */ }
            Ok(Ok(n)) => panic!(
                "device received {n} bytes despite invalid regex: {:?}",
                String::from_utf8_lossy(&buf[..n])
            ),
            Ok(Err(e)) => panic!("device read errored: {e}"),
        }
    }

    #[tokio::test]
    async fn serial_exec_empty_expect_rejects_before_writing_command() {
        let backend = DuplexBackend::new();
        let s = State {
            sessions: Arc::new(SessionManager::with_seed(backend.clone(), 0xCAFE)),
            profiles: Arc::new(vec![]),
        };

        let mut open_req = req("serial.open", Some(json!(52)));
        open_req.params = json!({ "port": "/dev/ttyUSB0", "baud": 115200, "timeout_ms": 1000 });
        let open_resp = dispatch(&s, open_req).await.expect("response");
        let session_id = open_resp
            .result
            .expect("open result")["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let mut device = backend.take_device();

        let mut exec_req = req("serial.exec", Some(json!(53)));
        exec_req.params = json!({
            "session_id": session_id,
            "command": "DESTRUCTIVE_OP\n",
            "expect": "",
        });
        let exec_resp = dispatch(&s, exec_req).await.expect("response");
        let err = exec_resp.error.expect("error");
        assert_eq!(err.code, -32008);
        assert_eq!(err.data.as_ref().unwrap()["name"], "expect");

        let mut buf = [0u8; 64];
        let read = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            device.read(&mut buf),
        )
        .await;
        assert!(
            matches!(read, Err(_) | Ok(Ok(0))),
            "device must not receive command on empty expect"
        );
    }

    #[tokio::test]
    async fn id_less_serial_exec_is_suppressed_before_side_effects() {
        // Same guarantee as serial.open — id-less serial.* must not run the
        // write half of the compound. Uses CountingBackend to detect any
        // backend touch (open here, which is the proxy for "did we route
        // into the manager at all").
        let opens = StdArc::new(AtomicUsize::new(0));
        let s = State {
            sessions: Arc::new(SessionManager::with_seed(
                CountingBackend {
                    opens: opens.clone(),
                },
                0xCAFE,
            )),
            profiles: Arc::new(vec![]),
        };
        let mut r = req("serial.exec", None);
        r.params = json!({
            "session_id": "deadbeefdeadbeef",
            "command": "help\n",
            "expect": "prompt> ",
        });
        assert!(dispatch(&s, r).await.is_none());
        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert_eq!(s.sessions.session_count(), 0);
    }

    #[tokio::test]
    async fn serial_close_unknown_session_returns_session_not_found_code() {
        let s = mock_state();
        let mut r = req("serial.close", Some(json!(16)));
        r.params = json!({ "session_id": "deadbeefdeadbeef" });
        let resp = dispatch(&s, r).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, -32003);
    }
}
