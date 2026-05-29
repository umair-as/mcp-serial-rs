// SPDX-License-Identifier: MIT OR Apache-2.0

//! Typed errors for the serial library, with one-to-one mapping to JSON-RPC
//! error codes. See CLAUDE.md §1 ("thiserror, not anyhow") and §7.
//!
//! Server-defined error codes occupy the JSON-RPC reserved range -32000..=-32099.
//! Each `SerialError` variant has a unique code so clients can branch on it
//! without parsing the message string.

use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SerialError {
    #[error("port '{port}' is not in the allowlist")]
    PortNotAllowed { port: String },

    #[error("port '{port}' not found")]
    PortNotFound { port: String },

    #[error("port '{port}' is busy (held by another process)")]
    PortBusy { port: String },

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
        }
    }

    /// Structured context attached as the JSON-RPC `data` field.
    pub fn data(&self) -> serde_json::Value {
        match self {
            SerialError::PortNotAllowed { port }
            | SerialError::PortNotFound { port }
            | SerialError::PortBusy { port } => {
                json!({ "port": port })
            }
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
        }
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
            SerialError::PortBusy { port: "p".into() }.code(),
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
        };
        assert_eq!(err.code(), -32010);
        assert_eq!(
            err.data().get("port").and_then(|v| v.as_str()),
            Some("/dev/ttyACM0"),
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
