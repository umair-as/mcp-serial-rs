//! Typed errors for the serial library, with one-to-one mapping to JSON-RPC
//! error codes. See CLAUDE.md §1 ("thiserror, not anyhow") and §7.
//!
//! Server-defined error codes occupy the JSON-RPC reserved range -32000..=-32099.
//! Each `SerialError` variant has a unique code so clients can branch on it
//! without parsing the message string.

use serde_json::json;
use thiserror::Error;

use crate::protocol;

#[derive(Debug, Error)]
pub enum SerialError {
    #[error("port '{port}' is not in the allowlist")]
    PortNotAllowed { port: String },

    #[error("port '{port}' not found")]
    PortNotFound { port: String },

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
        }
    }

    /// Structured context attached as the JSON-RPC `data` field.
    pub fn data(&self) -> serde_json::Value {
        match self {
            SerialError::PortNotAllowed { port } | SerialError::PortNotFound { port } => {
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

impl From<SerialError> for protocol::Error {
    fn from(err: SerialError) -> Self {
        let code = err.code();
        let data = err.data();
        protocol::Error::with_data(code, err.to_string(), data)
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
        ];
        let mut sorted = variants.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), variants.len(), "error codes must be unique");
        for c in variants {
            assert!((-32099..=-32000).contains(&c), "code {c} outside reserved range");
        }
    }

    #[test]
    fn maps_to_protocol_error_with_data() {
        let err = SerialError::PortNotAllowed {
            port: "/dev/ttyS0".into(),
        };
        let p: protocol::Error = err.into();
        assert_eq!(p.code, -32001);
        assert!(p.data.is_some());
    }
}
