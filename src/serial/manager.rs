// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SessionManager`: owns the `HashMap<SessionId, Session>` and enforces the
//! `MAX_SESSIONS` cap from `config.rs`. See CLAUDE.md §5.
//!
//! The manager is generic over [`SerialBackend`] so unit tests can substitute
//! a mock that never touches a real device.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::MutexGuard as AsyncMutexGuard;
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, warn};

use crate::config;
use crate::errors::SerialError;
use crate::serial::parser::{MatchDetails, PatternMatcher};
use crate::serial::session::{Session, WritePolicy};
use crate::serial::SerialBackend;

/// Real production backend over `tokio-serial`.
pub struct TokioSerialBackend;

impl SerialBackend for TokioSerialBackend {
    type Port = tokio_serial::SerialStream;

    async fn open(&self, port: &str, baud: u32) -> Result<Self::Port, SerialError> {
        use tokio_serial::SerialPortBuilderExt;
        tokio_serial::new(port, baud)
            .open_native_async()
            .map_err(|e| map_open_error(e, port))
    }
}

/// Map a `tokio_serial::Error` from the open path onto a typed `SerialError`.
///
/// The `serialport` posix backend collapses `EBUSY` (TIOCEXCL contention —
/// the port exists but another process holds it exclusively) into
/// `ErrorKind::NoDevice`, while a truly missing device surfaces as
/// `ErrorKind::Io(io::ErrorKind::NotFound)` via `ENOENT`. Folding both
/// into `PortNotFound` would mislead the caller into chasing an
/// enumeration problem, so we split them.
fn map_open_error(e: tokio_serial::Error, port: &str) -> SerialError {
    match e.kind {
        tokio_serial::ErrorKind::NoDevice => SerialError::PortBusy {
            port: port.to_string(),
        },
        tokio_serial::ErrorKind::Io(std::io::ErrorKind::NotFound) => SerialError::PortNotFound {
            port: port.to_string(),
        },
        _ => SerialError::Io {
            message: format!("open '{port}': {e}"),
        },
    }
}

pub struct SessionManager<B: SerialBackend> {
    backend: B,
    inner: Mutex<Inner<B::Port>>,
}

struct Inner<P> {
    sessions: HashMap<String, Session<P>>,
    deterministic_rng_state: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub port: String,
    pub baud: u32,
    pub state: &'static str,
    pub default_timeout_ms: u64,
    pub write_policy: WritePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadStatus {
    Complete,
    TimedOut,
    Eof,
    Cancelled,
}

impl ReadStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReadStatus::Complete => "complete",
            ReadStatus::TimedOut => "timed_out",
            ReadStatus::Eof => "eof",
            ReadStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOutcome {
    pub data: Vec<u8>,
    pub status: ReadStatus,
    pub bytes_read: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecStatus {
    Matched,
    TimedOut,
    Eof,
    Cancelled,
}

impl ExecStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecStatus::Matched => "matched",
            ExecStatus::TimedOut => "timed_out",
            ExecStatus::Eof => "eof",
            ExecStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    pub status: ExecStatus,
    pub output: Vec<u8>,
    pub bytes_written: usize,
    pub bytes_read: usize,
    pub elapsed_ms: u128,
    pub truncated: bool,
    pub match_details: Option<MatchDetails>,
    pub command_written: bool,
    pub session_usable: bool,
    pub cleared_before_write: Option<ReadOutcome>,
}

const DRAIN_IDLE_MS: u64 = 10;
const DRAIN_TOTAL_MS: u64 = 100;

impl<B: SerialBackend> SessionManager<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                deterministic_rng_state: None,
            }),
        }
    }

    /// Construct with a fixed deterministic ID seed. Test-only helper.
    #[cfg(test)]
    pub fn with_seed(backend: B, seed: u64) -> Self {
        Self {
            backend,
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                deterministic_rng_state: Some(seed),
            }),
        }
    }

    /// Number of live (non-`Closed`) sessions. Test introspection only.
    pub fn session_count(&self) -> usize {
        lock(&self.inner).sessions.len()
    }

    /// Inspect a session's state by id. Returns `None` if not present.
    pub fn session_state(&self, id: &str) -> Option<&'static str> {
        lock(&self.inner).sessions.get(id).map(|s| s.state.name())
    }

    /// The session's write policy, captured at open. Used by the `write` /
    /// `exec` handlers to gate device mutation before any port I/O. Returns
    /// [`SerialError::SessionNotFound`] for an unknown id. The policy is
    /// immutable for the session's lifetime, so reading it here and acting on
    /// it before the subsequent checkout is race-free with respect to policy.
    pub fn write_policy(&self, id: &str) -> Result<WritePolicy, SerialError> {
        lock(&self.inner)
            .sessions
            .get(id)
            .map(|s| s.write_policy)
            .ok_or_else(|| SerialError::SessionNotFound {
                session_id: id.to_string(),
            })
    }

    pub fn sessions(&self) -> Vec<SessionSnapshot> {
        let inner = lock(&self.inner);
        inner.sessions.values().map(snapshot).collect()
    }

    pub fn get_session(&self, id: &str) -> Result<SessionSnapshot, SerialError> {
        let inner = lock(&self.inner);
        inner
            .sessions
            .get(id)
            .map(snapshot)
            .ok_or_else(|| SerialError::SessionNotFound {
                session_id: id.to_string(),
            })
    }

    /// Open a new session. Validates allowlist, enforces `MAX_SESSIONS`,
    /// reserves the slot under lock before awaiting the backend so the cap
    /// holds across the await.
    #[instrument(skip(self), fields(port = port_path, baud))]
    pub async fn open(
        &self,
        port_path: &str,
        baud: u32,
        default_timeout_ms: u64,
        write_policy: WritePolicy,
    ) -> Result<String, SerialError> {
        if !config::matches_allowlist(port_path) {
            return Err(SerialError::PortNotAllowed {
                port: port_path.to_string(),
            });
        }

        // Reserve a slot synchronously.
        let session_id = {
            let mut inner = lock(&self.inner);
            if inner
                .sessions
                .values()
                .any(|session| session.port_path == port_path)
            {
                return Err(SerialError::PortBusy {
                    port: port_path.to_string(),
                });
            }
            if inner.sessions.len() >= config::MAX_SESSIONS {
                return Err(SerialError::MaxSessionsReached {
                    max: config::MAX_SESSIONS,
                });
            }
            let id = next_session_id(&mut inner)?;
            inner.sessions.insert(
                id.clone(),
                Session::opening(
                    id.clone(),
                    port_path.to_string(),
                    baud,
                    default_timeout_ms,
                    write_policy,
                ),
            );
            id
        };

        debug!(session_id, "opening port");
        let port_result = self.backend.open(port_path, baud).await;

        let mut inner = lock(&self.inner);
        match port_result {
            Ok(port) => match inner.sessions.get_mut(&session_id) {
                Some(session) => {
                    session.transition_to_ready(port);
                    Ok(session_id)
                }
                None => {
                    // Concurrent close removed the placeholder. Drop the
                    // freshly-opened port (happens on `port`'s Drop here).
                    warn!(session_id, "session removed during open");
                    Err(SerialError::SessionNotFound { session_id })
                }
            },
            Err(e) => {
                inner.sessions.remove(&session_id);
                Err(e)
            }
        }
    }

    /// Close a session. Accepts `Ready` or `Opening`; rejects everything else
    /// with [`SerialError::InvalidState`]. For ready sessions, waits for the
    /// per-session port mutex before returning so no checked-out I/O handle can
    /// perform serial I/O after close has completed.
    #[instrument(skip(self), fields(session_id))]
    pub async fn close(&self, session_id: &str) -> Result<(), SerialError> {
        self.close_with_timeout(session_id, config::MAX_TIMEOUT_MS)
            .await
    }

    async fn close_with_timeout(
        &self,
        session_id: &str,
        timeout_ms: u64,
    ) -> Result<(), SerialError> {
        let port = {
            let mut inner = lock(&self.inner);
            let Some(session) = inner.sessions.get_mut(session_id) else {
                return Err(SerialError::SessionNotFound {
                    session_id: session_id.to_string(),
                });
            };

            let port = session.port.clone();
            if !session.begin_close() {
                return Err(SerialError::InvalidState {
                    session_id: session_id.to_string(),
                    state: session.state.name(),
                    expected: "Ready or Opening",
                });
            }
            port
        };

        if let Some(port) = port {
            let lock_result = lock_port(
                &port,
                Instant::now() + Duration::from_millis(timeout_ms),
                timeout_ms,
                CancellationToken::new(),
            )
            .await;
            let _guard = match lock_result {
                Ok(guard) => guard,
                Err(err) => {
                    let mut inner = lock(&self.inner);
                    if let Some(session) = inner.sessions.get_mut(session_id) {
                        session.restore_ready(port.clone());
                    }
                    return Err(err);
                }
            };
        }

        let mut inner = lock(&self.inner);
        if let Some(session) = inner.sessions.get_mut(session_id) {
            session.finish_close();
        }
        inner.sessions.remove(session_id);
        Ok(())
    }

    /// Write `data` to the session's port and flush. Caller is responsible
    /// for upstream size limits; `tools::handle_write` enforces
    /// `MAX_WRITE_CHUNK` at the JSON-RPC boundary.
    #[instrument(skip(self, data), fields(session_id, len = data.len()))]
    pub async fn write(&self, session_id: &str, data: &[u8]) -> Result<usize, SerialError> {
        self.write_with_cancel(session_id, data, CancellationToken::new())
            .await
    }

    pub async fn write_with_cancel(
        &self,
        session_id: &str,
        data: &[u8],
        cancellation: CancellationToken,
    ) -> Result<usize, SerialError> {
        // `serial.write` exposes no per-call timeout, so the whole operation
        // (lock wait, write, flush) is bounded by the session default captured
        // at `serial.open`, not a fixed global.
        let (port, timeout_ms) = self.checkout_port_and_timeout(session_id)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut guard = lock_port(&port, deadline, timeout_ms, cancellation.clone()).await?;
        self.ensure_port_still_ready(session_id, &port)?;
        write_all_deadline(
            &mut *guard,
            data,
            deadline,
            timeout_ms,
            cancellation.clone(),
        )
        .await?;
        flush_deadline(&mut *guard, deadline, timeout_ms, cancellation).await?;
        Ok(data.len())
    }

    /// Drain bytes from the session's port until either `max_bytes` has
    /// been accumulated or the `timeout_ms` deadline elapses. Returns
    /// `(data, timed_out)` — on deadline-hit, `timed_out` is `true` and
    /// `data` is whatever was read so far (which may be empty). EOF on
    /// the port returns `timed_out=false` because EOF is a known
    /// completion, not a timeout. Genuine I/O errors stay errors
    /// (issue #4: domain-outcome parity with `read_until`).
    #[instrument(skip(self), fields(session_id, max_bytes, timeout_ms))]
    pub async fn read(
        &self,
        session_id: &str,
        max_bytes: usize,
        timeout_ms: u64,
    ) -> Result<(Vec<u8>, bool), SerialError> {
        let outcome = self
            .read_with_cancel(session_id, max_bytes, timeout_ms, CancellationToken::new())
            .await?;
        Ok((outcome.data, outcome.status == ReadStatus::TimedOut))
    }

    pub async fn read_with_cancel(
        &self,
        session_id: &str,
        max_bytes: usize,
        timeout_ms: u64,
        cancellation: CancellationToken,
    ) -> Result<ReadOutcome, SerialError> {
        let port = self.checkout_port(session_id)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut guard = lock_port(&port, deadline, timeout_ms, cancellation.clone()).await?;
        self.ensure_port_still_ready(session_id, &port)?;
        read_locked(&mut *guard, max_bytes, deadline, cancellation).await
    }

    /// Read until `pattern` matches anywhere in the accumulating buffer, or
    /// the overall `timeout_ms` deadline elapses. Returns `(data, matched)` —
    /// on timeout, `matched` is `false` and `data` is whatever was read so
    /// far (deliberately not a `Timeout` error: callers asked for "read
    /// until or quit," and partial output is informative). EOF on the port
    /// is treated the same as timeout.
    #[instrument(skip(self), fields(session_id, timeout_ms))]
    pub async fn read_until(
        &self,
        session_id: &str,
        pattern: &str,
        timeout_ms: u64,
    ) -> Result<(Vec<u8>, bool), SerialError> {
        let outcome = self
            .read_until_with_cancel(session_id, pattern, timeout_ms, CancellationToken::new())
            .await?;
        Ok((outcome.output, outcome.status == ExecStatus::Matched))
    }

    pub async fn read_until_with_cancel(
        &self,
        session_id: &str,
        pattern: &str,
        timeout_ms: u64,
        cancellation: CancellationToken,
    ) -> Result<ExecOutcome, SerialError> {
        let port = self.checkout_port(session_id)?;
        let mut matcher = PatternMatcher::new(pattern)?;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut guard = lock_port(&port, deadline, timeout_ms, cancellation.clone()).await?;
        self.ensure_port_still_ready(session_id, &port)?;
        let started = Instant::now();
        let (status, bytes_read, match_details) =
            read_until_locked(&mut *guard, &mut matcher, deadline, cancellation).await?;
        let output = matcher.into_buffer();
        Ok(ExecOutcome {
            status,
            bytes_written: 0,
            bytes_read,
            elapsed_ms: started.elapsed().as_millis(),
            truncated: output.len() >= config::MAX_READ_BUFFER,
            match_details,
            command_written: false,
            session_usable: true,
            cleared_before_write: None,
            output,
        })
    }

    pub async fn drain(
        &self,
        session_id: &str,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> Result<ReadOutcome, SerialError> {
        let port = self.checkout_port(session_id)?;
        let mut guard = lock_port(
            &port,
            Instant::now() + Duration::from_millis(DRAIN_TOTAL_MS),
            DRAIN_TOTAL_MS,
            cancellation.clone(),
        )
        .await?;
        self.ensure_port_still_ready(session_id, &port)?;
        read_idle_locked(
            &mut *guard,
            max_bytes,
            Duration::from_millis(DRAIN_IDLE_MS),
            Instant::now() + Duration::from_millis(DRAIN_TOTAL_MS),
            cancellation,
        )
        .await
    }

    pub async fn clear_input(
        &self,
        session_id: &str,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> Result<ReadOutcome, SerialError> {
        self.drain(session_id, max_bytes, cancellation).await
    }

    pub async fn exec(
        &self,
        session_id: &str,
        command: &[u8],
        expect: &str,
        timeout_ms: u64,
        clear_before_write: bool,
        cancellation: CancellationToken,
    ) -> Result<ExecOutcome, SerialError> {
        let port = self.checkout_port(session_id)?;
        let mut matcher = PatternMatcher::new(expect)?;
        if cancellation.is_cancelled() {
            return Ok(ExecOutcome {
                status: ExecStatus::Cancelled,
                output: Vec::new(),
                bytes_written: 0,
                bytes_read: 0,
                elapsed_ms: 0,
                truncated: false,
                match_details: None,
                command_written: false,
                session_usable: true,
                cleared_before_write: None,
            });
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut guard = lock_port(&port, deadline, timeout_ms, cancellation.clone()).await?;
        self.ensure_port_still_ready(session_id, &port)?;
        let started = Instant::now();
        let mut command_written = false;

        let cleared_before_write = if clear_before_write {
            let clear_deadline =
                (Instant::now() + Duration::from_millis(DRAIN_TOTAL_MS)).min(deadline);
            Some(
                read_idle_locked(
                    &mut *guard,
                    config::MAX_READ_BUFFER,
                    Duration::from_millis(DRAIN_IDLE_MS),
                    clear_deadline,
                    cancellation.clone(),
                )
                .await?,
            )
        } else {
            None
        };

        if cancellation.is_cancelled() {
            return Ok(ExecOutcome {
                status: ExecStatus::Cancelled,
                output: Vec::new(),
                bytes_written: 0,
                bytes_read: 0,
                elapsed_ms: started.elapsed().as_millis(),
                truncated: false,
                match_details: None,
                command_written,
                session_usable: true,
                cleared_before_write,
            });
        }

        write_all_deadline(
            &mut *guard,
            command,
            deadline,
            timeout_ms,
            cancellation.clone(),
        )
        .await?;
        flush_deadline(&mut *guard, deadline, timeout_ms, cancellation.clone()).await?;
        command_written = true;

        let (status, bytes_read, match_details) =
            read_until_locked(&mut *guard, &mut matcher, deadline, cancellation).await?;
        let output = matcher.into_buffer();

        Ok(ExecOutcome {
            status,
            bytes_written: command.len(),
            bytes_read,
            elapsed_ms: started.elapsed().as_millis(),
            truncated: output.len() >= config::MAX_READ_BUFFER,
            match_details,
            command_written,
            session_usable: true,
            cleared_before_write,
            output,
        })
    }

    /// Resolve a session id to its port handle, validating state in one shot.
    /// Returns `SessionNotFound` for unknown ids, `InvalidState` for anything
    /// other than `Ready`.
    fn checkout_port(&self, session_id: &str) -> Result<Arc<AsyncMutex<B::Port>>, SerialError> {
        self.checkout_port_and_timeout(session_id)
            .map(|(port, _)| port)
    }

    /// Resolve a session id to its port handle **and** its configured default
    /// timeout in one locked snapshot, so callers without a per-call timeout
    /// (e.g. `serial.write`) bound their operation by the session default
    /// without a second lookup that could observe a mutated session.
    #[allow(clippy::type_complexity)]
    fn checkout_port_and_timeout(
        &self,
        session_id: &str,
    ) -> Result<(Arc<AsyncMutex<B::Port>>, u64), SerialError> {
        let inner = lock(&self.inner);
        let Some(session) = inner.sessions.get(session_id) else {
            return Err(SerialError::SessionNotFound {
                session_id: session_id.to_string(),
            });
        };
        match &session.port {
            Some(p) => Ok((p.clone(), session.default_timeout_ms)),
            None => Err(SerialError::InvalidState {
                session_id: session_id.to_string(),
                state: session.state.name(),
                expected: "Ready",
            }),
        }
    }

    fn ensure_port_still_ready(
        &self,
        session_id: &str,
        checked_out: &Arc<AsyncMutex<B::Port>>,
    ) -> Result<(), SerialError> {
        let inner = lock(&self.inner);
        let Some(session) = inner.sessions.get(session_id) else {
            return Err(SerialError::SessionNotFound {
                session_id: session_id.to_string(),
            });
        };
        match &session.port {
            Some(current) if Arc::ptr_eq(current, checked_out) && session.is_ready() => Ok(()),
            _ => Err(SerialError::InvalidState {
                session_id: session_id.to_string(),
                state: session.state.name(),
                expected: "Ready",
            }),
        }
    }
}

fn snapshot<P>(session: &Session<P>) -> SessionSnapshot {
    SessionSnapshot {
        session_id: session.id.clone(),
        port: session.port_path.clone(),
        baud: session.baud,
        state: session.state.name(),
        default_timeout_ms: session.default_timeout_ms,
        write_policy: session.write_policy,
    }
}

async fn lock_port<'a, P>(
    port: &'a Arc<AsyncMutex<P>>,
    deadline: Instant,
    timeout_ms: u64,
    cancellation: CancellationToken,
) -> Result<AsyncMutexGuard<'a, P>, SerialError> {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(SerialError::Timeout { timeout_ms });
    };
    tokio::select! {
        _ = cancellation.cancelled() => Err(SerialError::Cancelled),
        result = tokio::time::timeout(remaining, port.lock()) => {
            result.map_err(|_| SerialError::Timeout { timeout_ms })
        }
    }
}

async fn write_all_deadline<P>(
    port: &mut P,
    data: &[u8],
    deadline: Instant,
    timeout_ms: u64,
    cancellation: CancellationToken,
) -> Result<(), SerialError>
where
    P: tokio::io::AsyncWrite + Unpin,
{
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(SerialError::Timeout { timeout_ms });
    };
    tokio::select! {
        _ = cancellation.cancelled() => Err(SerialError::Cancelled),
        result = tokio::time::timeout(remaining, port.write_all(data)) => {
            match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(SerialError::Io { message: format!("write: {e}") }),
                Err(_) => Err(SerialError::Timeout { timeout_ms }),
            }
        }
    }
}

async fn flush_deadline<P>(
    port: &mut P,
    deadline: Instant,
    timeout_ms: u64,
    cancellation: CancellationToken,
) -> Result<(), SerialError>
where
    P: tokio::io::AsyncWrite + Unpin,
{
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(SerialError::Timeout { timeout_ms });
    };
    tokio::select! {
        _ = cancellation.cancelled() => Err(SerialError::Cancelled),
        result = tokio::time::timeout(remaining, port.flush()) => {
            match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(SerialError::Io { message: format!("flush: {e}") }),
                Err(_) => Err(SerialError::Timeout { timeout_ms }),
            }
        }
    }
}

async fn read_locked<P>(
    port: &mut P,
    max_bytes: usize,
    deadline: Instant,
    cancellation: CancellationToken,
) -> Result<ReadOutcome, SerialError>
where
    P: tokio::io::AsyncRead + Unpin,
{
    let mut acc: Vec<u8> = Vec::with_capacity(max_bytes.min(4096));
    let mut chunk = [0u8; 4096];

    loop {
        if acc.len() >= max_bytes {
            let bytes_read = acc.len();
            return Ok(ReadOutcome {
                data: acc,
                status: ReadStatus::Complete,
                bytes_read,
                truncated: bytes_read >= max_bytes,
            });
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            let bytes_read = acc.len();
            return Ok(ReadOutcome {
                data: acc,
                status: ReadStatus::TimedOut,
                bytes_read,
                truncated: false,
            });
        };
        let take = (max_bytes - acc.len()).min(chunk.len());
        tokio::select! {
            _ = cancellation.cancelled() => {
                let bytes_read = acc.len();
                return Ok(ReadOutcome {
                    data: acc,
                    status: ReadStatus::Cancelled,
                    bytes_read,
                    truncated: false,
                });
            }
            result = tokio::time::timeout(remaining, port.read(&mut chunk[..take])) => {
                match result {
                    Ok(Ok(0)) => {
                        let bytes_read = acc.len();
                        return Ok(ReadOutcome {
                            data: acc,
                            status: ReadStatus::Eof,
                            bytes_read,
                            truncated: false,
                        });
                    }
                    Ok(Ok(n)) => acc.extend_from_slice(&chunk[..n]),
                    Ok(Err(e)) => return Err(SerialError::Io { message: format!("read: {e}") }),
                    Err(_) => {
                        let bytes_read = acc.len();
                        return Ok(ReadOutcome {
                            data: acc,
                            status: ReadStatus::TimedOut,
                            bytes_read,
                            truncated: false,
                        });
                    }
                }
            }
        }
    }
}

async fn read_idle_locked<P>(
    port: &mut P,
    max_bytes: usize,
    idle_window: Duration,
    overall_deadline: Instant,
    cancellation: CancellationToken,
) -> Result<ReadOutcome, SerialError>
where
    P: tokio::io::AsyncRead + Unpin,
{
    let mut acc: Vec<u8> = Vec::with_capacity(max_bytes.min(4096));
    let mut chunk = [0u8; 4096];

    loop {
        if acc.len() >= max_bytes {
            let bytes_read = acc.len();
            return Ok(ReadOutcome {
                data: acc,
                status: ReadStatus::Complete,
                bytes_read,
                truncated: bytes_read >= max_bytes,
            });
        }
        let Some(overall_remaining) = overall_deadline.checked_duration_since(Instant::now())
        else {
            let bytes_read = acc.len();
            return Ok(ReadOutcome {
                data: acc,
                status: ReadStatus::TimedOut,
                bytes_read,
                truncated: false,
            });
        };
        let read_window = idle_window.min(overall_remaining);
        let take = (max_bytes - acc.len()).min(chunk.len());
        tokio::select! {
            _ = cancellation.cancelled() => {
                let bytes_read = acc.len();
                return Ok(ReadOutcome {
                    data: acc,
                    status: ReadStatus::Cancelled,
                    bytes_read,
                    truncated: false,
                });
            }
            result = tokio::time::timeout(read_window, port.read(&mut chunk[..take])) => {
                match result {
                    Ok(Ok(0)) => {
                        let bytes_read = acc.len();
                        return Ok(ReadOutcome {
                            data: acc,
                            status: ReadStatus::Eof,
                            bytes_read,
                            truncated: false,
                        });
                    }
                    Ok(Ok(n)) => acc.extend_from_slice(&chunk[..n]),
                    Ok(Err(e)) => return Err(SerialError::Io { message: format!("read: {e}") }),
                    Err(_) => {
                        let bytes_read = acc.len();
                        return Ok(ReadOutcome {
                            data: acc,
                            status: ReadStatus::TimedOut,
                            bytes_read,
                            truncated: false,
                        });
                    }
                }
            }
        }
    }
}

async fn read_until_locked<P>(
    port: &mut P,
    matcher: &mut PatternMatcher,
    deadline: Instant,
    cancellation: CancellationToken,
) -> Result<(ExecStatus, usize, Option<MatchDetails>), SerialError>
where
    P: tokio::io::AsyncRead + Unpin,
{
    let mut bytes_read = 0;
    let mut buf = [0u8; 4096];

    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok((ExecStatus::TimedOut, bytes_read, matcher.match_details()));
        };
        tokio::select! {
            _ = cancellation.cancelled() => {
                return Ok((ExecStatus::Cancelled, bytes_read, matcher.match_details()));
            }
            result = tokio::time::timeout(remaining, port.read(&mut buf)) => {
                match result {
                    Ok(Ok(0)) => {
                        let status = if matcher.is_match() {
                            ExecStatus::Matched
                        } else {
                            ExecStatus::Eof
                        };
                        return Ok((status, bytes_read, matcher.match_details()));
                    }
                    Ok(Ok(n)) => {
                        bytes_read += n;
                        if matcher.push(&buf[..n]) {
                            return Ok((ExecStatus::Matched, bytes_read, matcher.match_details()));
                        }
                    }
                    Ok(Err(e)) => return Err(SerialError::Io { message: format!("read: {e}") }),
                    Err(_) => return Ok((ExecStatus::TimedOut, bytes_read, matcher.match_details())),
                }
            }
        }
    }
}

/// Recover from a poisoned mutex by extracting the inner guard.
///
/// The data under the lock is a plain `HashMap` and a `u64` RNG state; neither
/// is mid-mutation across an unwind boundary, so reading them after a poison
/// is safe. This keeps library code free of `expect()` per CLAUDE.md §8.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn next_session_id<P>(inner: &mut Inner<P>) -> Result<String, SerialError> {
    loop {
        let id = match inner.deterministic_rng_state.as_mut() {
            Some(state) => {
                let hi = next_splitmix64(state);
                let lo = next_splitmix64(state);
                format!("{hi:016x}{lo:016x}")
            }
            None => random_session_id()?,
        };
        if !inner.sessions.contains_key(&id) {
            return Ok(id);
        }
    }
}

fn random_session_id() -> Result<String, SerialError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| SerialError::Io {
        message: format!("session id entropy source failed: {e}"),
    })?;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    Ok(out)
}

fn next_splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    /// Mock backend with controllable behaviour. Cloneable so tests can keep
    /// a handle to the same shared state after moving one clone into the
    /// `SessionManager`.
    #[derive(Clone)]
    struct MockBackend {
        shared: Arc<MockShared>,
    }

    struct MockShared {
        fail_next: AtomicBool,
        // The "device" side of each duplex pair, in open order. Tests pop
        // the half they want and drive the device end.
        test_sides: Mutex<Vec<DuplexStream>>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                shared: Arc::new(MockShared {
                    fail_next: AtomicBool::new(false),
                    test_sides: Mutex::new(Vec::new()),
                }),
            }
        }

        fn fail_next(&self) {
            self.shared.fail_next.store(true, Ordering::SeqCst);
        }

        /// Remove and return the device-side half of the most recently opened
        /// duplex. Tests drive this end to feed reads / inspect writes.
        fn take_device(&self) -> DuplexStream {
            self.shared
                .test_sides
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop()
                .expect("no device side available")
        }
    }

    impl SerialBackend for MockBackend {
        type Port = DuplexStream;

        async fn open(&self, _port: &str, _baud: u32) -> Result<DuplexStream, SerialError> {
            if self.shared.fail_next.swap(false, Ordering::SeqCst) {
                return Err(SerialError::Io {
                    message: "mock backend forced failure".into(),
                });
            }
            let (manager_side, device_side) = tokio::io::duplex(4096);
            self.shared
                .test_sides
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(device_side);
            Ok(manager_side)
        }
    }

    fn mgr() -> SessionManager<MockBackend> {
        SessionManager::with_seed(MockBackend::new(), 0xDEAD_BEEF)
    }

    /// Manager + a handle to the same backend, for tests that drive the
    /// device side of the duplex.
    fn mgr_with_backend() -> (SessionManager<MockBackend>, MockBackend) {
        let backend = MockBackend::new();
        let m = SessionManager::with_seed(backend.clone(), 0xDEAD_BEEF);
        (m, backend)
    }

    #[tokio::test]
    async fn allowlist_accepts_ttyusb() {
        let m = mgr();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        assert_eq!(id.len(), 32, "128-bit hex id must be 32 chars: {id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(m.session_count(), 1);
        assert_eq!(m.session_state(&id), Some("Ready"));
    }

    #[tokio::test]
    async fn allowlist_accepts_ttyacm() {
        let m = mgr();
        m.open("/dev/ttyACM0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn allowlist_rejects_non_matching_path() {
        let m = mgr();
        let err = m
            .open("/dev/ttyS0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap_err();
        assert!(matches!(err, SerialError::PortNotAllowed { .. }));
        assert_eq!(m.session_count(), 0);
    }

    #[tokio::test]
    async fn opening_then_close_removes_session() {
        let m = mgr();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        assert_eq!(m.session_count(), 1);
        m.close(&id).await.unwrap();
        assert_eq!(m.session_count(), 0);
        assert_eq!(m.session_state(&id), None);
    }

    #[tokio::test]
    async fn close_unknown_session_errors() {
        let m = mgr();
        let err = m.close("0000000000000000").await.unwrap_err();
        assert!(matches!(err, SerialError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn open_records_write_policy_and_exposes_it() {
        let m = mgr();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Deny)
            .await
            .unwrap();
        // Accessor and snapshot both surface the policy captured at open.
        assert_eq!(m.write_policy(&id).unwrap(), WritePolicy::Deny);
        assert_eq!(m.get_session(&id).unwrap().write_policy, WritePolicy::Deny);
        // Unknown id → SessionNotFound, same as the other lookups.
        assert!(matches!(
            m.write_policy("0000000000000000").unwrap_err(),
            SerialError::SessionNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn double_close_errors() {
        let m = mgr();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        m.close(&id).await.unwrap();
        let err = m.close(&id).await.unwrap_err();
        assert!(matches!(err, SerialError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn close_timeout_rolls_back_to_ready_and_can_retry() {
        let m = mgr();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let port = m.checkout_port(&id).unwrap();
        let guard = port.lock().await;

        let err = m.close_with_timeout(&id, 5).await.unwrap_err();
        assert!(matches!(err, SerialError::Timeout { timeout_ms: 5 }));
        assert_eq!(m.session_state(&id), Some("Ready"));

        drop(guard);
        m.close(&id).await.unwrap();
        assert_eq!(m.session_state(&id), None);
    }

    #[tokio::test]
    async fn write_is_bounded_by_session_default_timeout_not_global() {
        let m = mgr();
        // Open with a short session default timeout distinct from the 5000ms
        // global default. `serial.write` has no per-call timeout, so this must
        // be the deadline for lock wait / write / flush.
        let id = m
            .open("/dev/ttyUSB0", 115_200, 40, WritePolicy::Allow)
            .await
            .unwrap();
        let port = m.checkout_port(&id).unwrap();
        let guard = port.lock().await; // force write to block on lock acquisition

        let started = Instant::now();
        let err = m.write(&id, b"blocked\n").await.unwrap_err();
        assert!(
            matches!(err, SerialError::Timeout { timeout_ms: 40 }),
            "write must time out at the session default (40ms), got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(config::DEFAULT_TIMEOUT_MS),
            "write must not fall back to the global default timeout",
        );

        drop(guard);
        m.write(&id, b"ok\n").await.unwrap();
    }

    #[tokio::test]
    async fn max_sessions_enforced_then_recovers() {
        let m = mgr();
        let mut ids = Vec::new();
        for idx in 0..config::MAX_SESSIONS {
            ids.push(
                m.open(
                    &format!("/dev/ttyUSB{idx}"),
                    115_200,
                    5_000,
                    WritePolicy::Allow,
                )
                .await
                .unwrap(),
            );
        }
        let err = m
            .open("/dev/ttyUSB99", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SerialError::MaxSessionsReached { max } if max == config::MAX_SESSIONS),
            "unexpected error: {err:?}"
        );
        m.close(&ids.pop().unwrap()).await.unwrap();
        m.open("/dev/ttyUSB99", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn duplicate_port_path_is_rejected_until_close_completes() {
        let m = mgr();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let err = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap_err();
        assert!(matches!(err, SerialError::PortBusy { .. }));
        m.close(&id).await.unwrap();
        m.open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn open_failure_cleans_up_placeholder() {
        let backend = MockBackend::new();
        backend.fail_next();
        let m = SessionManager::with_seed(backend, 0x1234);
        let err = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap_err();
        assert!(matches!(err, SerialError::Io { .. }));
        assert_eq!(m.session_count(), 0, "placeholder must be removed");
    }

    #[tokio::test]
    async fn ids_are_unique_across_opens() {
        let m = mgr();
        let mut ids = std::collections::HashSet::new();
        for idx in 0..config::MAX_SESSIONS {
            let id = m
                .open(
                    &format!("/dev/ttyUSB{idx}"),
                    115_200,
                    5_000,
                    WritePolicy::Allow,
                )
                .await
                .unwrap();
            assert!(ids.insert(id.clone()), "duplicate session id: {id}");
        }
    }

    #[test]
    fn map_open_error_no_device_maps_to_port_busy() {
        // serialport-rs deliberately reports EBUSY (TIOCEXCL contention)
        // as ErrorKind::NoDevice. The mapper must surface that as
        // PortBusy, not PortNotFound — see issue #3.
        let e =
            tokio_serial::Error::new(tokio_serial::ErrorKind::NoDevice, "Device or resource busy");
        match super::map_open_error(e, "/dev/ttyACM0") {
            SerialError::PortBusy { port } => assert_eq!(port, "/dev/ttyACM0"),
            other => panic!("expected PortBusy, got {other:?}"),
        }
    }

    #[test]
    fn map_open_error_io_notfound_maps_to_port_not_found() {
        let e = tokio_serial::Error::new(
            tokio_serial::ErrorKind::Io(std::io::ErrorKind::NotFound),
            "No such file or directory",
        );
        match super::map_open_error(e, "/dev/ttyUSB7") {
            SerialError::PortNotFound { port } => assert_eq!(port, "/dev/ttyUSB7"),
            other => panic!("expected PortNotFound, got {other:?}"),
        }
    }

    #[test]
    fn map_open_error_other_io_maps_to_io() {
        let e = tokio_serial::Error::new(
            tokio_serial::ErrorKind::Io(std::io::ErrorKind::PermissionDenied),
            "Permission denied",
        );
        match super::map_open_error(e, "/dev/ttyUSB0") {
            SerialError::Io { message } => assert!(message.contains("/dev/ttyUSB0")),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn deterministic_next_session_id_format_is_32_char_lowercase_hex() {
        let mut inner: Inner<()> = Inner {
            sessions: HashMap::new(),
            deterministic_rng_state: Some(0xDEAD_BEEFu64),
        };
        for _ in 0..1000 {
            let id = next_session_id(&mut inner).unwrap();
            assert_eq!(id.len(), 32);
            assert!(id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
    }

    // --- write / read --- //

    #[tokio::test]
    async fn write_round_trips_to_device_side() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();

        let n = m.write(&id, b"hello esp32\n").await.unwrap();
        assert_eq!(n, 12);

        let mut buf = [0u8; 64];
        let read = device.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..read], b"hello esp32\n");
    }

    #[tokio::test]
    async fn read_returns_buffered_device_data() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();
        device.write_all(b"boot ok\n").await.unwrap();

        let (data, timed_out) = m.read(&id, 64, 1_000).await.unwrap();
        assert_eq!(&data, b"boot ok\n");
        // The first read returned bytes; the loop then re-enters and
        // its next read blocks until the deadline (no more data). That
        // is the new domain-outcome shape: not an error.
        assert!(
            timed_out,
            "loop should hit the deadline after consuming data"
        );
    }

    #[tokio::test]
    async fn read_times_out_when_no_data() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        // Keep the device side alive so the manager-side stream doesn't EOF;
        // an EOF would surface as Ok(0) and timed_out=false.
        let _device = backend.take_device();

        let (data, timed_out) = m.read(&id, 64, 50).await.unwrap();
        assert!(data.is_empty(), "no bytes should accumulate on idle port");
        assert!(
            timed_out,
            "deadline must surface as timed_out=true, not an Err"
        );
    }

    #[tokio::test]
    async fn read_stops_at_max_bytes_with_timed_out_false() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();
        device.write_all(b"abcdefghij").await.unwrap();
        let _device = device; // keep alive

        let (data, timed_out) = m.read(&id, 5, 1_000).await.unwrap();
        assert_eq!(&data, b"abcde");
        assert!(!timed_out, "hitting max_bytes is not a timeout");
    }

    #[tokio::test]
    async fn read_loop_accumulates_across_multiple_writes() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();
        let writer = tokio::spawn(async move {
            device.write_all(b"part1-").await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            device.write_all(b"part2").await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(device);
        });

        let (data, _timed_out) = m.read(&id, 64, 150).await.unwrap();
        // The deadline cuts the loop short — depending on scheduling the
        // first or both chunks land. The point is both chunks are valid
        // accumulation, not lost inside an error.
        assert!(
            data == b"part1-part2" || data == b"part1-",
            "unexpected accumulator contents: {:?}",
            String::from_utf8_lossy(&data),
        );
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn read_eof_returns_timed_out_false() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();
        device.write_all(b"bye").await.unwrap();
        drop(device); // EOF on the manager-side stream

        let (data, timed_out) = m.read(&id, 64, 1_000).await.unwrap();
        assert_eq!(&data, b"bye");
        assert!(!timed_out, "EOF is a known completion, not a timeout");
    }

    #[tokio::test]
    async fn write_to_unknown_session_returns_session_not_found() {
        let m = mgr();
        let err = m.write("nonexistentid000", b"x").await.unwrap_err();
        assert!(matches!(err, SerialError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn read_after_close_returns_session_not_found() {
        let m = mgr();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        m.close(&id).await.unwrap();
        // SessionNotFound is a *protocol* failure (no such id), not a
        // domain outcome — it must still surface as Err, distinct from
        // the timed_out=true shape that real reads now use.
        let err = m.read(&id, 64, 100).await.unwrap_err();
        assert!(matches!(err, SerialError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn empty_write_succeeds_with_zero_bytes() {
        let (m, _backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let n = m.write(&id, b"").await.unwrap();
        assert_eq!(n, 0);
    }

    // --- read_until --- //

    #[tokio::test]
    async fn read_until_matches_pattern_split_across_reads() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();

        // Drive the device in chunks; the pattern straddles a chunk boundary.
        let device_task = tokio::spawn(async move {
            device.write_all(b"booting...\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            device.write_all(b"ALL KATS ").await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            device.write_all(b"PASSED\nprompt> ").await.unwrap();
        });

        let (data, matched) = m.read_until(&id, "ALL KATS PASSED", 2_000).await.unwrap();
        assert!(matched, "should match across reads");
        assert!(data.windows(15).any(|w| w == b"ALL KATS PASSED"));
        device_task.await.unwrap();
    }

    #[tokio::test]
    async fn read_until_returns_partial_on_timeout() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();
        device.write_all(b"partial output").await.unwrap();
        // Keep device alive so we don't EOF.
        let _device = device;

        let (data, matched) = m.read_until(&id, "NEVER GONNA MATCH", 80).await.unwrap();
        assert!(!matched);
        assert_eq!(&data, b"partial output");
    }

    #[tokio::test]
    async fn read_until_with_empty_pattern_errors() {
        let m = mgr();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let err = m.read_until(&id, "", 100).await.unwrap_err();
        assert!(matches!(err, SerialError::InvalidParam { ref name, .. } if name == "pattern"));
    }

    #[tokio::test]
    async fn read_until_unknown_session_errors() {
        let m = mgr();
        let err = m
            .read_until("noSuchId00000000", "x", 100)
            .await
            .unwrap_err();
        assert!(matches!(err, SerialError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn read_until_eof_returns_unmatched_partial() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();
        device.write_all(b"goodbye").await.unwrap();
        drop(device); // signals EOF on the manager-side read

        let (data, matched) = m.read_until(&id, "match-me", 1_000).await.unwrap();
        assert!(!matched);
        assert_eq!(&data, b"goodbye");
    }

    #[tokio::test]
    async fn concurrent_writes_to_same_session_serialise() {
        // Two writes that exceed the duplex internal buffer would deadlock
        // if they weren't serialised. With per-session mutex, the second
        // write waits, the device drains, then both complete.
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();

        // Start two writes; one drains the device in parallel.
        let id_a = id.clone();
        let id_b = id.clone();
        let m = Arc::new(m);
        let m_a = m.clone();
        let m_b = m.clone();
        let w1 = tokio::spawn(async move { m_a.write(&id_a, b"alpha-").await });
        let w2 = tokio::spawn(async move { m_b.write(&id_b, b"beta").await });

        let mut acc = Vec::new();
        let mut buf = [0u8; 32];
        while acc.len() < b"alpha-beta".len() {
            let n = device.read(&mut buf).await.unwrap();
            acc.extend_from_slice(&buf[..n]);
        }

        w1.await.unwrap().unwrap();
        w2.await.unwrap().unwrap();
        // Order between two concurrent writes isn't strictly defined, but
        // each whole payload must appear contiguously thanks to the per-
        // session mutex.
        assert!(
            acc == b"alpha-beta" || acc == b"betaalpha-",
            "interleaved unexpectedly: {:?}",
            String::from_utf8_lossy(&acc)
        );
    }

    #[tokio::test]
    async fn exec_holds_session_lock_across_write_and_read_until() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();
        let m = Arc::new(m);

        let exec_mgr = m.clone();
        let exec_id = id.clone();
        let exec_task = tokio::spawn(async move {
            exec_mgr
                .exec(
                    &exec_id,
                    b"cmd-one\n",
                    "DONE1",
                    300,
                    false,
                    CancellationToken::new(),
                )
                .await
        });

        let mut command = vec![0u8; b"cmd-one\n".len()];
        device.read_exact(&mut command).await.unwrap();
        assert_eq!(command, b"cmd-one\n");

        let write_mgr = m.clone();
        let write_id = id.clone();
        let mut write_task =
            tokio::spawn(async move { write_mgr.write(&write_id, b"cmd-two\n").await });

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut write_task)
                .await
                .is_err(),
            "write must wait while exec is still reading under the same session lock",
        );

        device.write_all(b"DONE1\n").await.unwrap();
        let outcome = exec_task.await.unwrap().unwrap();
        assert_eq!(outcome.status, ExecStatus::Matched);

        let mut second = vec![0u8; b"cmd-two\n".len()];
        device.read_exact(&mut second).await.unwrap();
        assert_eq!(second, b"cmd-two\n");
        write_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn exec_cancelled_before_write_reports_no_side_effect() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let _device = backend.take_device();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let outcome = m
            .exec(&id, b"must-not-write\n", "DONE", 100, false, cancellation)
            .await
            .unwrap();

        assert_eq!(outcome.status, ExecStatus::Cancelled);
        assert!(!outcome.command_written);
        assert_eq!(outcome.bytes_written, 0);
        assert_eq!(outcome.bytes_read, 0);
        assert!(outcome.session_usable);
    }

    #[tokio::test]
    async fn exec_clear_before_write_respects_operation_deadline() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();

        let writer = tokio::spawn(async move {
            for _ in 0..10 {
                device.write_all(b"stale\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let mut buf = [0u8; 32];
            match tokio::time::timeout(Duration::from_millis(50), device.read(&mut buf)).await {
                Ok(Ok(n)) => n,
                _ => 0,
            }
        });

        let started = Instant::now();
        let err = m
            .exec(
                &id,
                b"must-not-write\n",
                "DONE",
                20,
                true,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, SerialError::Timeout { timeout_ms: 20 }));
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "clear phase must not allocate an independent long deadline",
        );
        assert_eq!(writer.await.unwrap(), 0, "command must not be written");
    }

    #[tokio::test]
    async fn close_waits_for_in_flight_exec_before_returning() {
        let (m, backend) = mgr_with_backend();
        let id = m
            .open("/dev/ttyUSB0", 115_200, 5_000, WritePolicy::Allow)
            .await
            .unwrap();
        let mut device = backend.take_device();
        let m = Arc::new(m);

        let exec_mgr = m.clone();
        let exec_id = id.clone();
        let exec_task = tokio::spawn(async move {
            exec_mgr
                .exec(
                    &exec_id,
                    b"long-running\n",
                    "DONE",
                    1_000,
                    false,
                    CancellationToken::new(),
                )
                .await
        });

        let mut command = vec![0u8; b"long-running\n".len()];
        device.read_exact(&mut command).await.unwrap();
        assert_eq!(command, b"long-running\n");

        let close_mgr = m.clone();
        let close_id = id.clone();
        let mut close_task = tokio::spawn(async move { close_mgr.close(&close_id).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut close_task)
                .await
                .is_err(),
            "close must wait while exec holds the session port lock",
        );

        device.write_all(b"DONE\n").await.unwrap();
        exec_task.await.unwrap().unwrap();
        close_task.await.unwrap().unwrap();

        let err = m.write(&id, b"after-close\n").await.unwrap_err();
        assert!(matches!(err, SerialError::SessionNotFound { .. }));
    }
}
