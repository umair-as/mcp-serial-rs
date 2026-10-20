// SPDX-License-Identifier: MIT OR Apache-2.0

//! Typed errors for the serial library, with one-to-one mapping to JSON-RPC
//! error codes. See CLAUDE.md §1 ("thiserror, not anyhow") and §7.
//!
//! Server-defined error codes occupy the JSON-RPC reserved range -32000..=-32099.
//! Each `SerialError` variant has a unique code so clients can branch on it
//! without parsing the message string.

use serde_json::json;
use thiserror::Error;

fn port_busy_message(port: &str, session_id: Option<&str>) -> String {
    match session_id {
        Some(id) => format!("port '{port}' is already open in session '{id}' of this server"),
        None => format!("port '{port}' is busy (held by another process)"),
    }
}

#[derive(Debug, Error)]
pub enum SerialError {
    #[error("port '{port}' is not in the allowlist")]
    PortNotAllowed { port: String },

    #[error("port '{port}' not found")]
    PortNotFound { port: String },

    #[error("{}", port_busy_message(port, session_id.as_deref()))]
    PortBusy {
        port: String,
        /// The session of this server that already holds the port, when the
        /// conflict is our own reservation rather than another process.
        session_id: Option<String>,
    },

    #[error("unknown session_id '{session_id}'")]
    SessionNotFound { session_id: String },

    #[error("session '{session_id}' is in state {state}, expected {expected}")]
    InvalidState {
        session_id: String,
        state: &'static str,
        expected: &'static str,
    },

    #[error("operation timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },

    #[error("i/o error: {message}")]
    Io { message: String },

    #[error("maximum of {max} concurrent sessions reached")]
    MaxSessionsReached { max: usize },

    #[error("invalid parameter '{name}': {reason}")]
    InvalidParam { name: String, reason: String },

    #[error("device '{device}' not found in profiles or not currently connected")]
    DeviceNotFound { device: String },

    #[error("operation cancelled")]
    Cancelled,

    #[error("session '{session_id}' is read-only (write_policy=deny)")]
    WriteForbidden { session_id: String },

    #[error("session '{session_id}' requires confirm=true to write (write_policy=confirm)")]
    ConfirmationRequired { session_id: String },

    #[error("command blocked by server-owned policy rule '{rule}'")]
    CommandBlocked { rule: String },

    #[error(
        "session '{session_id}' has a command policy; use serial.exec instead of serial.write"
    )]
    RawWriteForbidden { session_id: String },

    /// The person at the MCP client answered the server's confirmation
    /// elicitation with anything other than an accept (ADR 0009). Not
    /// retryable: re-issuing the same call re-asks the same person.
    #[error("session '{session_id}' write was not confirmed by the user (elicitation {action})")]
    ConfirmationDeclined { session_id: String, action: String },
}

impl SerialError {
    /// Unique JSON-RPC error code for this variant. All codes live inside the
    /// reserved server-defined range (-32000..=-32099).
    pub fn code(&self) -> i32 {
        match self {
            SerialError::PortNotAllowed { .. } => -32001,
            SerialError::PortNotFound { .. } => -32002,
            SerialError::SessionNotFound { .. } => -32003,
            SerialError::InvalidState { .. } => -32004,
            SerialError::Timeout { .. } => -32005,
            SerialError::Io { .. } => -32006,
            SerialError::MaxSessionsReached { .. } => -32007,
            SerialError::InvalidParam { .. } => -32008,
            SerialError::DeviceNotFound { .. } => -32009,
            SerialError::PortBusy { .. } => -32010,
            SerialError::Cancelled => -32011,
            SerialError::WriteForbidden { .. } => -32012,
            SerialError::ConfirmationRequired { .. } => -32013,
            SerialError::CommandBlocked { .. } => -32014,
            SerialError::RawWriteForbidden { .. } => -32015,
            SerialError::ConfirmationDeclined { .. } => -32016,
        }
    }

    /// Structured context attached as the JSON-RPC `data` field.
    pub fn data(&self) -> serde_json::Value {
        match self {
            SerialError::PortNotAllowed { port } | SerialError::PortNotFound { port } => {
                json!({ "port": port })
            }
            SerialError::PortBusy {
                port,
                session_id: None,
            } => json!({ "port": port }),
            SerialError::PortBusy {
                port,
                session_id: Some(session_id),
            } => json!({ "port": port, "session_id": session_id }),
            SerialError::SessionNotFound { session_id } => json!({ "session_id": session_id }),
            SerialError::InvalidState {
                session_id,
                state,
                expected,
            } => json!({ "session_id": session_id, "state": state, "expected": expected }),
            SerialError::Timeout { timeout_ms } => json!({ "timeout_ms": timeout_ms }),
            SerialError::Io { message } => json!({ "message": message }),
            SerialError::MaxSessionsReached { max } => json!({ "max": max }),
            SerialError::InvalidParam { name, reason } => json!({ "name": name, "reason": reason }),
            SerialError::DeviceNotFound { device } => json!({ "device": device }),
            SerialError::Cancelled => json!({}),
            SerialError::WriteForbidden { session_id } => {
                json!({ "session_id": session_id, "write_policy": "deny" })
            }
            SerialError::ConfirmationRequired { session_id } => {
                json!({ "session_id": session_id, "write_policy": "confirm" })
            }
            SerialError::CommandBlocked { rule } => json!({ "rule": rule }),
            SerialError::RawWriteForbidden { session_id } => {
                json!({ "session_id": session_id, "mutation_via_exec_only": true })
            }
            SerialError::ConfirmationDeclined { session_id, action } => {
                json!({ "session_id": session_id, "write_policy": "confirm", "elicitation": action })
            }
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            SerialError::PortNotAllowed { .. } => "port_not_allowed",
            SerialError::PortNotFound { .. } => "port_not_found",
            SerialError::PortBusy { .. } => "port_busy",
            SerialError::SessionNotFound { .. } => "unknown_session",
            SerialError::InvalidState { .. } => "invalid_session_state",
            SerialError::Timeout { .. } => "timeout",
            SerialError::Io { .. } => "io_error",
            SerialError::MaxSessionsReached { .. } => "max_sessions_reached",
            SerialError::InvalidParam { .. } => "invalid_parameter",
            SerialError::DeviceNotFound { .. } => "device_not_found",
            SerialError::Cancelled => "cancelled",
            SerialError::WriteForbidden { .. } => "write_forbidden",
            SerialError::ConfirmationRequired { .. } => "confirmation_required",
            SerialError::CommandBlocked { .. } => "command_blocked",
            SerialError::RawWriteForbidden { .. } => "raw_write_forbidden",
            SerialError::ConfirmationDeclined { .. } => "confirmation_declined",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            SerialError::PortBusy { .. }
                | SerialError::PortNotFound { .. }
                | SerialError::DeviceNotFound { .. }
                | SerialError::Io { .. }
                | SerialError::Cancelled
        )
    }
}

impl From<SerialError> for rmcp::ErrorData {
    /// Preserve the project's pinned JSON-RPC error codes (`-32001` …
    /// `-32009`) and structured `data` payload when adapting to rmcp's
    /// error type. Using a typed `From` keeps the rmcp handlers from
    /// drifting to `internal_error` for domain failures.
    fn from(err: SerialError) -> Self {
        let code = rmcp::model::ErrorCode(err.code());
        let data = err.data();
        rmcp::ErrorData::new(code, err.to_string(), Some(data))
    }
}

impl From<std::io::Error> for SerialError {
    fn from(err: std::io::Error) -> Self {
        SerialError::Io {
            message: err.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_unique() {
        let variants = [
            SerialError::PortNotAllowed { port: "p".into() }.code(),
            SerialError::PortNotFound { port: "p".into() }.code(),
            SerialError::SessionNotFound {
                session_id: "s".into(),
            }
            .code(),
            SerialError::InvalidState {
                session_id: "s".into(),
                state: "Closed",
                expected: "Ready",
            }
            .code(),
            SerialError::Timeout { timeout_ms: 1 }.code(),
            SerialError::Io {
                message: "x".into(),
            }
            .code(),
            SerialError::MaxSessionsReached { max: 4 }.code(),
            SerialError::InvalidParam {
                name: "x".into(),
                reason: "y".into(),
            }
            .code(),
            SerialError::DeviceNotFound {
                device: "esp32c6".into(),
            }
            .code(),
            SerialError::PortBusy {
                port: "p".into(),
                session_id: None,
            }
            .code(),
            SerialError::Cancelled.code(),
            SerialError::WriteForbidden {
                session_id: "s".into(),
            }
            .code(),
            SerialError::ConfirmationRequired {
                session_id: "s".into(),
            }
            .code(),
            SerialError::CommandBlocked { rule: "r".into() }.code(),
            SerialError::RawWriteForbidden {
                session_id: "s".into(),
            }
            .code(),
            SerialError::ConfirmationDeclined {
                session_id: "s".into(),
                action: "decline".into(),
            }
            .code(),
        ];
        let mut sorted = variants.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), variants.len(), "error codes must be unique");
        for c in variants {
            assert!(
                (-32099..=-32000).contains(&c),
                "code {c} outside reserved range"
            );
        }
    }

    #[test]
    fn port_busy_pins_code_and_carries_port_in_data() {
        let err = SerialError::PortBusy {
            port: "/dev/ttyACM0".into(),
            session_id: None,
        };
        assert_eq!(err.code(), -32010);
        assert_eq!(
            err.data().get("port").and_then(|v| v.as_str()),
            Some("/dev/ttyACM0"),
        );
        assert!(err.data().get("session_id").is_none());
        assert!(err.to_string().contains("another process"));
    }

    #[test]
    fn port_busy_names_our_own_holding_session() {
        let err = SerialError::PortBusy {
            port: "/dev/ttyUSB1".into(),
            session_id: Some("abc".into()),
        };
        assert_eq!(err.code(), -32010);
        assert_eq!(err.data()["session_id"], "abc");
        assert_eq!(
            err.to_string(),
            "port '/dev/ttyUSB1' is already open in session 'abc' of this server"
        );
    }

    #[test]
    fn maps_to_rmcp_error_with_pinned_code_and_data() {
        // Regression guard: rmcp adapters must preserve the project's
        // pinned codes, NOT collapse to -32603 internal_error.
        let err = SerialError::Io {
            message: "open: no such file".into(),
        };
        let r: rmcp::ErrorData = err.into();
        assert_eq!(r.code, rmcp::model::ErrorCode(-32006));
        assert_eq!(
            r.data
                .as_ref()
                .and_then(|d| d.get("message"))
                .and_then(|v| v.as_str()),
            Some("open: no such file"),
        );
    }
}
