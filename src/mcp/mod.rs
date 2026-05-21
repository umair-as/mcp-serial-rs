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

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::config::{self, DeviceProfile};
use crate::serial::SerialBackend;
use crate::serial::journal::JournalWriter;
use crate::serial::manager::SessionManager;

/// rmcp-facing server. Holds shared references to the same domain state as
/// the legacy [`crate::tools::State`], so both stacks can coexist during
/// the migration without duplicating sessions, profiles, or journal handle.
pub struct McpServer<B: SerialBackend> {
    sessions: Arc<SessionManager<B>>,
    profiles: Arc<Vec<DeviceProfile>>,
    /// Reserved for the step-11 tool-call journal narrowing. Held now so
    /// the constructor signature does not churn when journaling is wired
    /// onto this stack.
    _journal: Option<Arc<JournalWriter>>,
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
            _journal: self._journal.clone(),
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
            _journal: journal,
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
        // Suppress dead-code warning for the field while step-11 journal
        // narrowing has not yet been wired onto this stack.
        let _ = &self.sessions;

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
}

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

#[tool_handler(router = self.tool_router)]
impl<B: SerialBackend> ServerHandler for McpServer<B> {
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
