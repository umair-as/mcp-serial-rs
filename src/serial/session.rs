// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-port `Session`: state machine (`Opening → Ready → Closing → Closed`,
//! CLAUDE.md §5) and the owned port handle.
//!
//! The port is held behind `Arc<tokio::sync::Mutex<P>>` so I/O can take place
//! off the manager-wide lock: under the manager lock we clone the `Arc`, then
//! the async I/O serialises on the per-session port mutex. Concurrent I/O on
//! *different* sessions is fully parallel.

use std::sync::Arc;

use tokio::sync::Mutex as AsyncMutex;

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
}

impl<P> Session<P> {
    /// Construct a placeholder in `Opening`. The manager reserves the slot
    /// under lock before awaiting `SerialBackend::open` so the `MAX_SESSIONS`
    /// cap is enforced even across the await point.
    pub fn opening(id: String, port_path: String, baud: u32, default_timeout_ms: u64) -> Self {
        Self {
            id,
            state: SessionState::Opening,
            port_path,
            baud,
            port: None,
            default_timeout_ms,
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
