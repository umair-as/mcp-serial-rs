// SPDX-License-Identifier: MIT OR Apache-2.0

//! rmcp adapter layer (migration step 3).
//!
//! Parallel MCP server built on the `rmcp` SDK. The hand-rolled JSON-RPC
//! stack in `crate::protocol` and `crate::tools` remains the authoritative
//! production path; this module exists so the migration can advance one
//! tool at a time without breaking the existing binary surface.
//!
//! Only `serial.list_ports` is wired in this slice (spec §Migration
//! Sequence step 3). Other tools stay on the legacy stack until their
//! respective porting steps land.
//!
//! Domain state — session manager, device profiles, audit journal — is
//! shared with the legacy stack by reference. This module owns no
//! serial-domain logic; it only adapts rmcp tool calls onto
//! `crate::serial`.

mod journal;

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::instrument;

use crate::config::{self, DeviceProfile};
use crate::serial::SerialBackend;
use crate::serial::journal::{JournalEntry, JournalWriter};
use crate::serial::manager::SessionManager;

/// rmcp-facing server. Holds shared references to the same domain state as
/// the legacy [`crate::tools::State`], so both stacks can coexist during
/// the migration without duplicating sessions, profiles, or journal handle.
pub struct McpServer<B: SerialBackend> {
    sessions: Arc<SessionManager<B>>,
    profiles: Arc<Vec<DeviceProfile>>,
    /// Tool-call audit journal. `None` means degraded mode (journal
    /// open failed at startup); every dispatch then proceeds without
    /// logging. Wired through `call_tool` (spec §Journal Requirements).
    journal: Option<Arc<JournalWriter>>,
    tool_router: ToolRouter<Self>,
}

// Manual `Clone` so we do not require `B: Clone`. Every field is already
// behind an `Arc` (or is the rmcp-provided `ToolRouter`, which is `Clone`).
// The `tool_handler` macro requires `Self: Clone` to dispatch tool futures.
impl<B: SerialBackend> Clone for McpServer<B> {
    fn clone(&self) -> Self {
        Self {
            sessions: Arc::clone(&self.sessions),
            profiles: Arc::clone(&self.profiles),
            journal: self.journal.clone(),
            tool_router: self.tool_router.clone(),
        }
    }
}

#[tool_router(router = tool_router)]
impl<B: SerialBackend> McpServer<B> {
    pub fn new(
        sessions: Arc<SessionManager<B>>,
        profiles: Arc<Vec<DeviceProfile>>,
        journal: Option<Arc<JournalWriter>>,
    ) -> Self {
        Self {
            sessions,
            profiles,
            journal,
            tool_router: Self::tool_router(),
        }
    }

    /// Enumerate allowlisted serial ports and annotate with matching device
    /// profile metadata. Read-only and sessionless; lowest-risk first slice.
    ///
    /// Returns a structured tool result whose `structuredContent` is a
    /// JSON object `{"ports": [...]}` — wrapped because MCP defines
    /// `structuredContent` as an *object* (see the 2025-11-25 schema),
    /// not a bare array. Inner element shape matches the legacy
    /// `serial.list_ports` array contract.
    ///
    /// Output schema is intentionally omitted in this step per spec
    /// §Structured Result Requirements ("phased in after behavioral
    /// parity").
    #[tool(
        name = "serial.list_ports",
        description = "List allowlisted serial ports, enriched with matching device-profile metadata."
    )]
    #[instrument(skip(self))]
    pub async fn list_ports(&self) -> Result<CallToolResult, McpError> {
        // `SerialError` -> `rmcp::ErrorData` preserves the project's pinned
        // codes (e.g. `Io = -32006`) and the structured `data` payload;
        // see `errors.rs`. Do NOT collapse this to `internal_error`.
        let ports = crate::serial::list_ports(&self.profiles)?;
        let value = serde_json::json!({ "ports": ports });
        Ok(CallToolResult::structured(value))
    }

    /// Open a serial session. Either `port` (literal device path) or
    /// `device` (named profile from `devices.toml`) must be supplied —
    /// not both, not neither. `baud` defaults to the profile's baud when
    /// resolving by `device`, otherwise to [`config::DEFAULT_BAUD`].
    /// `timeout_ms` is clamped to [`config::MAX_TIMEOUT_MS`].
    ///
    /// Returns structured `{"session_id": "<16-char-hex>"}`. The XOR /
    /// presence rule is enforced at runtime; we deliberately do NOT
    /// encode it in the generated input schema (see module docs and
    /// spec §Migration Sequence step 7 instructions).
    #[tool(
        name = "serial.open",
        description = "Open a serial session by literal port path or named device profile."
    )]
    #[instrument(skip(self, params), fields(port = ?params.0.port, device = ?params.0.device, baud = ?params.0.baud))]
    pub async fn open(
        &self,
        params: Parameters<OpenParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;

        // Tool-contract validation: surface as SerialError::InvalidParam so
        // the pinned -32008 code reaches the client (spec §Error Semantics
        // "Error-code compatibility"). Do NOT use `McpError::invalid_params`
        // here — that maps to -32602 and is reserved for JSON-RPC envelope
        // / params-deserialization failures.
        let (port_path, baud) = match (p.port, p.device) {
            (Some(_), Some(_)) => {
                return Err(crate::errors::SerialError::InvalidParam {
                    name: "port/device".into(),
                    reason: "specify 'device' or 'port', not both".into(),
                }
                .into());
            }
            (None, None) => {
                return Err(crate::errors::SerialError::InvalidParam {
                    name: "port/device".into(),
                    reason: "missing required parameter: 'port' or 'device'".into(),
                }
                .into());
            }
            (Some(port), None) => (port, p.baud.unwrap_or(config::DEFAULT_BAUD)),
            (None, Some(device_name)) => {
                let (path, profile_baud) =
                    crate::serial::resolve_device(&device_name, &self.profiles)?;
                (path, p.baud.unwrap_or(profile_baud))
            }
        };

        let timeout_ms = p
            .timeout_ms
            .unwrap_or(config::DEFAULT_TIMEOUT_MS)
            .min(config::MAX_TIMEOUT_MS);

        let session_id = self.sessions.open(&port_path, baud, timeout_ms).await?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "session_id": session_id }),
        ))
    }

    /// Write UTF-8 data to the session's port. Caller controls the bytes
    /// — no implicit newline is appended (spec §Non-Goals).
    ///
    /// Sizes above [`config::MAX_WRITE_CHUNK`] are rejected with the
    /// project's pinned `InvalidParam` code (-32008); a runtime
    /// validation, not a domain failure, so the device sees zero bytes.
    /// Returns structured `{"bytes_written": n}`.
    #[tool(
        name = "serial.write",
        description = "Write UTF-8 bytes to an open serial session. No implicit newline."
    )]
    #[instrument(skip(self, params), fields(session_id = %params.0.session_id, len = params.0.data.len()))]
    pub async fn write(
        &self,
        params: Parameters<WriteParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;

        if p.data.len() > config::MAX_WRITE_CHUNK {
            return Err(crate::errors::SerialError::InvalidParam {
                name: "data".into(),
                reason: format!(
                    "'data' is {} bytes; max per write is {}",
                    p.data.len(),
                    config::MAX_WRITE_CHUNK
                ),
            }
            .into());
        }

        let bytes = self
            .sessions
            .write(&p.session_id, p.data.as_bytes())
            .await?;
        Ok(CallToolResult::structured(
            serde_json::json!({ "bytes_written": bytes }),
        ))
    }

    /// Read up to `max_bytes` from the session's port, returning whatever
    /// arrived before `timeout_ms`. Both fields are clamped to the
    /// configured ceilings; out-of-range values are silently capped, not
    /// rejected.
    ///
    /// Returns structured `{"data": String}`. Bytes are decoded with
    /// UTF-8 lossy conversion — this is a console tool, not a binary
    /// protocol bridge (spec §Non-Goals).
    #[tool(
        name = "serial.read",
        description = "Read up to `max_bytes` bytes from a serial session, with timeout."
    )]
    #[instrument(skip(self, params), fields(session_id = %params.0.session_id))]
    pub async fn read(
        &self,
        params: Parameters<ReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;

        let timeout_ms = p
            .timeout_ms
            .unwrap_or(config::DEFAULT_TIMEOUT_MS)
            .min(config::MAX_TIMEOUT_MS);
        let max_bytes = p
            .max_bytes
            .unwrap_or(DEFAULT_READ_MAX_BYTES)
            .min(config::MAX_READ_BUFFER);

        let data = self
            .sessions
            .read(&p.session_id, max_bytes, timeout_ms)
            .await?;
        Ok(CallToolResult::structured(serde_json::json!({
            "data": String::from_utf8_lossy(&data),
        })))
    }

    /// Read until `pattern` matches (regex) or `timeout_ms` elapses.
    /// Partial output is **not** an error: timeout/EOF returns the
    /// buffered data with `matched=false` (spec §Error Semantics —
    /// "partial output is normal completion"). Invalid or empty regex
    /// is rejected with `InvalidParam` (-32008) *before* any port read,
    /// so the device stream is not consumed on bad input.
    ///
    /// Returns structured `{"data": String, "matched": bool}`.
    #[tool(
        name = "serial.read_until",
        description = "Read until a regex pattern matches or timeout elapses; returns partial output with matched=false on timeout."
    )]
    #[instrument(skip(self, params), fields(session_id = %params.0.session_id))]
    pub async fn read_until(
        &self,
        params: Parameters<ReadUntilParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;

        let timeout_ms = p
            .timeout_ms
            .unwrap_or(config::DEFAULT_TIMEOUT_MS)
            .min(config::MAX_TIMEOUT_MS);

        let (data, matched) = self
            .sessions
            .read_until(&p.session_id, &p.pattern, timeout_ms)
            .await?;
        Ok(CallToolResult::structured(serde_json::json!({
            "data": String::from_utf8_lossy(&data),
            "matched": matched,
        })))
    }

    /// Compound write + read_until. Writes `command` verbatim (caller
    /// controls bytes — no implicit newline, spec §Non-Goals) then
    /// reads until `expect` matches or `timeout_ms` elapses.
    ///
    /// Validation runs **before** the write, so a bad request never
    /// mutates device state (spec §Error Semantics / §Migration
    /// Sequence step 9):
    ///
    /// 1. `command` size ≤ [`config::MAX_WRITE_CHUNK`]
    /// 2. `expect` non-empty
    /// 3. `expect` is a valid regex
    ///
    /// Returns structured `{"output": String, "ok": bool}`. `ok=false`
    /// on timeout (with partial output) is normal completion — NOT a
    /// JSON-RPC error.
    #[tool(
        name = "serial.exec",
        description = "Write a command and read until an `expect` regex matches or timeout elapses. Validation happens before write."
    )]
    #[instrument(skip(self, params), fields(session_id = %params.0.session_id, cmd_len = params.0.command.len()))]
    pub async fn exec(
        &self,
        params: Parameters<ExecParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;

        // Step 1: size cap on command — surfaces the pinned -32008,
        // not the legacy -32602.
        if p.command.len() > config::MAX_WRITE_CHUNK {
            return Err(crate::errors::SerialError::InvalidParam {
                name: "command".into(),
                reason: format!(
                    "'command' is {} bytes; max per write is {}",
                    p.command.len(),
                    config::MAX_WRITE_CHUNK
                ),
            }
            .into());
        }

        // Step 2 + 3: validate `expect` BEFORE writing — read_until's
        // own regex compile happens *after* the write would have
        // occurred, so without this guard a bad pattern would mutate
        // the device (e.g. a remote shell) and only then surface
        // InvalidParam. The check duplicates parser logic deliberately;
        // we cannot afford the write side-effect.
        if p.expect.is_empty() {
            return Err(crate::errors::SerialError::InvalidParam {
                name: "expect".into(),
                reason: "must not be empty".into(),
            }
            .into());
        }
        if let Err(e) = regex::Regex::new(&p.expect) {
            return Err(crate::errors::SerialError::InvalidParam {
                name: "expect".into(),
                reason: format!("invalid regex: {e}"),
            }
            .into());
        }

        let timeout_ms = p
            .timeout_ms
            .unwrap_or(config::DEFAULT_TIMEOUT_MS)
            .min(config::MAX_TIMEOUT_MS);

        // Write first; on write failure, surface immediately rather
        // than swallowing into a timed-out read_until. The session-
        // state check is duplicated across the two manager calls but
        // is cheap, and keeps composition honest — a concurrent close
        // between write and read_until is reported, not hidden.
        self.sessions
            .write(&p.session_id, p.command.as_bytes())
            .await?;
        let (data, matched) = self
            .sessions
            .read_until(&p.session_id, &p.expect, timeout_ms)
            .await?;
        Ok(CallToolResult::structured(serde_json::json!({
            "output": String::from_utf8_lossy(&data),
            "ok": matched,
        })))
    }

    /// Close a session and release its port. Removes the session from
    /// the manager's map; subsequent reads/writes on the same id return
    /// [`crate::errors::SerialError::SessionNotFound`] (-32003).
    ///
    /// Returns structured `{"ok": true}` on success.
    #[tool(
        name = "serial.close",
        description = "Close a serial session and release its port."
    )]
    #[instrument(skip(self, params), fields(session_id = %params.0.session_id))]
    pub async fn close(
        &self,
        params: Parameters<CloseParams>,
    ) -> Result<CallToolResult, McpError> {
        let Parameters(p) = params;
        self.sessions.close(&p.session_id)?;
        Ok(CallToolResult::structured(serde_json::json!({ "ok": true })))
    }
}

/// Default `max_bytes` for `serial.read` when the caller omits the field.
/// Matches the legacy stack's default. Kept local to the rmcp adapter so
/// the constant does not leak into the public domain config surface.
const DEFAULT_READ_MAX_BYTES: usize = 4096;

/// Input for `serial.open`. Mirrors the legacy `OpenParams` shape so the
/// public contract is preserved. The XOR between `port` and `device` is
/// validated at runtime, not via schema, per spec §Migration Sequence
/// step 7 ("Runtime validation is the contract").
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenParams {
    /// Literal device path (e.g. `/dev/ttyUSB0`). Mutually exclusive
    /// with `device`.
    #[serde(default)]
    pub port: Option<String>,
    /// Named device profile from `devices.toml`. Mutually exclusive
    /// with `port`.
    #[serde(default)]
    pub device: Option<String>,
    /// Override the baud rate. Defaults to the profile baud (device
    /// path) or `config::DEFAULT_BAUD` (port path).
    #[serde(default)]
    pub baud: Option<u32>,
    /// Per-session default operation timeout in milliseconds. Clamped
    /// to `config::MAX_TIMEOUT_MS`. Falls back to
    /// `config::DEFAULT_TIMEOUT_MS` when absent.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Input for `serial.write`. `data` is UTF-8 text; binary protocols are
/// out of scope (spec §Non-Goals).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteParams {
    pub session_id: String,
    pub data: String,
}

/// Input for `serial.read`. Both bounds are optional; the handler
/// applies defaults and clamps to the configured ceilings.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    pub session_id: String,
    /// Soft cap on bytes returned. Clamped to `config::MAX_READ_BUFFER`.
    /// Defaults to `DEFAULT_READ_MAX_BYTES` when absent.
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// Read deadline in milliseconds. Clamped to `config::MAX_TIMEOUT_MS`.
    /// Defaults to `config::DEFAULT_TIMEOUT_MS` when absent.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Input for `serial.read_until`. `pattern` is a regex string compiled
/// by the parser before any port read; invalid or empty patterns fail
/// validation up-front.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadUntilParams {
    pub session_id: String,
    pub pattern: String,
    /// Read deadline in milliseconds. Clamped to `config::MAX_TIMEOUT_MS`.
    /// Defaults to `config::DEFAULT_TIMEOUT_MS` when absent.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Input for `serial.close`. Just the session id — closing is
/// idempotent only with respect to "already-removed sessions" (those
/// produce SessionNotFound, the same code an unknown id would).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseParams {
    pub session_id: String,
}

/// Input for `serial.exec`. `command` is written verbatim (no implicit
/// newline). `expect` is a non-empty regex compiled BEFORE the write so
/// invalid input never mutates device state.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecParams {
    pub session_id: String,
    pub command: String,
    pub expect: String,
    /// Read deadline in milliseconds, applied to the `read_until`
    /// phase. Clamped to `config::MAX_TIMEOUT_MS`. Defaults to
    /// `config::DEFAULT_TIMEOUT_MS` when absent.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[tool_handler(router = self.tool_router)]
impl<B: SerialBackend> ServerHandler for McpServer<B> {
    /// Override the macro-generated `call_tool` so the rmcp dispatch
    /// has a single chokepoint for the tool-call journal (spec
    /// §Migration Sequence step 11 / §Journal Requirements). Lifecycle
    /// methods (`initialize`, `tools/list`, `notifications/initialized`)
    /// never enter this method — they are handled elsewhere in the
    /// SDK — so narrowing to "tool calls only" falls out of the choice
    /// of hook point, not a name filter.
    ///
    /// One `call` row goes in before dispatch and one `result` row
    /// after, regardless of outcome. Journal I/O is wrapped in
    /// `JournalWriter::log` which warns-and-swallows on failure, so
    /// audit pressure never blocks tool execution. Concurrent calls
    /// stay non-interleaving via the writer's `tokio::sync::Mutex`.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Capture the inputs we need for journaling BEFORE moving
        // `request` into the SDK's ToolCallContext.
        let tool_name = request.name.to_string();
        let args_value: Value = request
            .arguments
            .as_ref()
            .map(|m| Value::Object(m.clone()))
            .unwrap_or(Value::Null);

        // call row
        if let Some(j) = &self.journal {
            let entry = JournalEntry::new(
                journal::call_session_id(&args_value),
                tool_name.clone(),
                JournalEntry::DIR_CALL,
                journal::call_summary(&tool_name, &args_value),
            );
            j.log(&entry).await;
        }

        let tcc =
            rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let result = self.tool_router.call(tcc).await;

        // result row
        if let Some(j) = &self.journal {
            let result_ref: Result<&CallToolResult, &McpError> = result.as_ref();
            let entry = JournalEntry::new(
                journal::result_session_id(&tool_name, &args_value, &result_ref),
                tool_name.clone(),
                JournalEntry::DIR_RESULT,
                journal::result_summary(&tool_name, &result_ref),
            );
            j.log(&entry).await;
        }

        result
    }

    fn get_info(&self) -> ServerInfo {
        // `ServerInfo`/`InitializeResult` is `#[non_exhaustive]`, so we
        // start from `default()` (already pinned to the latest protocol
        // version) and overwrite only what we care about. Constructing it
        // directly with a struct-update expression is rejected by rustc.
        //
        // We construct `Implementation` explicitly instead of calling
        // `Implementation::from_build_env()`. That helper expands its
        // `env!("CARGO_PKG_*")` reads at the rmcp crate's build site, so
        // it would report `name = "rmcp"` here. We want our crate's
        // identity to reach the client.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        );
        info
    }
}
