// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-port `Session`: state machine (`Opening → Ready → Closing → Closed`,
//! CLAUDE.md §5) and the owned port handle.
//!
//! The port is held behind `Arc<tokio::sync::Mutex<P>>` so I/O can take place
//! off the manager-wide lock: under the manager lock we clone the `Arc`, then
//! the async I/O serialises on the per-session port mutex. Concurrent I/O on
//! *different* sessions is fully parallel.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use crate::serial::console::ConsoleSettings;
use crate::serial::policy::CommandPolicy;

/// Per-session gate on whether `serial.write` / `serial.exec` may send bytes
/// to the device (CLAUDE.md safety model). Captured at `serial.open` and
/// immutable for the session's lifetime.
///
/// Variant order is significant: `Allow < Confirm < Deny`, so the derived
/// `Ord` lets the open path combine a profile default and a caller override
/// with `.max()` ("most-restrictive wins" — a caller may escalate but never
/// downgrade a privileged profile).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum WritePolicy {
    /// Writes proceed unconditionally. The default; preserves prior behavior.
    #[default]
    Allow,
    /// Writes require an explicit `confirm: true` on the call. A tripwire and
    /// audit point (self-satisfiable by an automated caller), and the seam a
    /// future MCP elicitation upgrade turns into a real human prompt.
    Confirm,
    /// Writes are refused server-side regardless of the call — a hard,
    /// model-proof read-only session. Reads/drains/clears are still allowed.
    Deny,
}

/// Lifecycle states for an open session.
pub enum SessionState {
    Opening,
    Ready,
    Closing,
    Closed,
}

impl SessionState {
    /// Stable human-readable name — surfaced in `InvalidState` errors.
    pub fn name(&self) -> &'static str {
        match self {
            SessionState::Opening => "Opening",
            SessionState::Ready => "Ready",
            SessionState::Closing => "Closing",
            SessionState::Closed => "Closed",
        }
    }
}

/// A single serial session. `id` is a random lowercase hex string; see
/// `SessionManager::next_session_id`.
pub struct Session<P> {
    pub id: String,
    pub state: SessionState,
    pub port_path: String,
    pub baud: u32,
    /// Some(port) iff `state == Ready`. `Arc<Mutex<P>>` so concurrent
    /// `serial.write` / `serial.read` calls on the same session serialise
    /// while leaving the manager-wide lock unheld during I/O.
    pub port: Option<Arc<AsyncMutex<P>>>,
    /// Default per-operation timeout supplied at `serial.open` time. Consumed
    /// by `serial.read_until` in Task 5.
    pub default_timeout_ms: u64,
    /// Whether writes are allowed / gated / forbidden for this session.
    /// Resolved at `serial.open` (most-restrictive of profile default and
    /// caller override) and enforced by the `write` / `exec` handlers.
    pub write_policy: WritePolicy,
    /// Immutable profile-owned console execution defaults. Literal-port
    /// sessions use [`ConsoleSettings::default`].
    pub console_settings: ConsoleSettings,
    /// Immutable, compiled server-owned policy for complete commands.
    pub command_policy: Arc<CommandPolicy>,
}

impl<P> Session<P> {
    /// Construct a placeholder in `Opening`. The manager reserves the slot
    /// under lock before awaiting `SerialBackend::open` so the `MAX_SESSIONS`
    /// cap is enforced even across the await point.
    pub fn opening(
        id: String,
        port_path: String,
        baud: u32,
        default_timeout_ms: u64,
        write_policy: WritePolicy,
        console_settings: ConsoleSettings,
        command_policy: Arc<CommandPolicy>,
    ) -> Self {
        Self {
            id,
            state: SessionState::Opening,
            port_path,
            baud,
            port: None,
            default_timeout_ms,
            write_policy,
            console_settings,
            command_policy,
        }
    }

    /// `Opening → Ready(port)`. Panics in debug on any other source state —
    /// internal invariant; the manager is the only caller.
    pub fn transition_to_ready(&mut self, port: P) {
        debug_assert!(matches!(self.state, SessionState::Opening));
        self.state = SessionState::Ready;
        self.port = Some(Arc::new(AsyncMutex::new(port)));
    }

    /// `Ready | Opening → Closing`, dropping our port handle. If outstanding
    /// I/O still holds an `Arc` clone, the underlying port is dropped when
    /// that clone is released (cooperative close).
    /// Returns whether the transition was valid.
    pub fn begin_close(&mut self) -> bool {
        match self.state {
            SessionState::Ready | SessionState::Opening => {
                self.state = SessionState::Closing;
                self.port = None;
                true
            }
            SessionState::Closing | SessionState::Closed => false,
        }
    }

    /// `Closing → Closed`.
    pub fn finish_close(&mut self) {
        debug_assert!(matches!(self.state, SessionState::Closing));
        self.state = SessionState::Closed;
    }

    /// Roll back `Closing → Ready` when close could not acquire the port lock.
    /// The manager is the only caller and passes back the same checked-out port
    /// handle that `begin_close` removed.
    pub fn restore_ready(&mut self, port: Arc<AsyncMutex<P>>) {
        debug_assert!(matches!(self.state, SessionState::Closing));
        self.state = SessionState::Ready;
        self.port = Some(port);
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.state, SessionState::Ready)
    }

    /// Clone the port handle for I/O. Returns `None` when not `Ready`.
    pub fn port_handle(&self) -> Option<Arc<AsyncMutex<P>>> {
        self.port.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_policy_default_is_allow() {
        assert_eq!(WritePolicy::default(), WritePolicy::Allow);
    }

    #[test]
    fn write_policy_ordering_is_most_restrictive() {
        // Allow < Confirm < Deny, so `.max()` picks the stricter of two.
        assert!(WritePolicy::Allow < WritePolicy::Confirm);
        assert!(WritePolicy::Confirm < WritePolicy::Deny);
        assert_eq!(
            WritePolicy::Allow.max(WritePolicy::Confirm),
            WritePolicy::Confirm
        );
        assert_eq!(WritePolicy::Allow.max(WritePolicy::Deny), WritePolicy::Deny);
        assert_eq!(
            WritePolicy::Confirm.max(WritePolicy::Deny),
            WritePolicy::Deny
        );
        // A caller may escalate but never downgrade a profile default.
        assert_eq!(
            WritePolicy::Confirm.max(WritePolicy::Allow),
            WritePolicy::Confirm
        );
    }

    #[test]
    fn write_policy_serializes_to_lowercase_tokens() {
        assert_eq!(
            serde_json::to_value(WritePolicy::Allow).unwrap(),
            serde_json::json!("allow")
        );
        assert_eq!(
            serde_json::to_value(WritePolicy::Confirm).unwrap(),
            serde_json::json!("confirm")
        );
        assert_eq!(
            serde_json::to_value(WritePolicy::Deny).unwrap(),
            serde_json::json!("deny")
        );
        let parsed: WritePolicy = serde_json::from_value(serde_json::json!("deny")).unwrap();
        assert_eq!(parsed, WritePolicy::Deny);
    }
}
