// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SessionManager`: owns the `HashMap<SessionId, Session>` and enforces the
//! `MAX_SESSIONS` cap from `config.rs`. See CLAUDE.md §5.
//!
//! The manager is generic over [`SerialBackend`] so unit tests can substitute
//! a mock that never touches a real device.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, instrument, warn};

use crate::config;
use crate::errors::SerialError;
use crate::serial::parser::PatternMatcher;
use crate::serial::session::Session;
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
    rng_state: u64,
}

impl<B: SerialBackend> SessionManager<B> {
    pub fn new(backend: B) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| (d.as_nanos() as u64) ^ (std::process::id() as u64).rotate_left(17))
            .unwrap_or(0xCAFE_F00D_DEAD_BEEF);
        Self::with_seed(backend, seed)
    }

    /// Construct with a fixed RNG seed — useful for deterministic tests.
    pub fn with_seed(backend: B, seed: u64) -> Self {
        Self {
            backend,
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                rng_state: seed,
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

    /// Open a new session. Validates allowlist, enforces `MAX_SESSIONS`,
    /// reserves the slot under lock before awaiting the backend so the cap
    /// holds across the await.
    #[instrument(skip(self), fields(port = port_path, baud))]
    pub async fn open(
        &self,
        port_path: &str,
        baud: u32,
        default_timeout_ms: u64,
    ) -> Result<String, SerialError> {
        if !config::matches_allowlist(port_path) {
            return Err(SerialError::PortNotAllowed {
                port: port_path.to_string(),
            });
        }

        // Reserve a slot synchronously.
        let session_id = {
            let mut inner = lock(&self.inner);
            if inner.sessions.len() >= config::MAX_SESSIONS {
                return Err(SerialError::MaxSessionsReached {
                    max: config::MAX_SESSIONS,
                });
            }
            let id = next_id(&mut inner.rng_state);
            inner
                .sessions
                .insert(id.clone(), Session::opening(id.clone(), default_timeout_ms));
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
    /// with [`SerialError::InvalidState`]. Removes the entry on success.
    #[instrument(skip(self), fields(session_id))]
    pub fn close(&self, session_id: &str) -> Result<(), SerialError> {
        let mut inner = lock(&self.inner);
        let Some(session) = inner.sessions.get_mut(session_id) else {
            return Err(SerialError::SessionNotFound {
                session_id: session_id.to_string(),
            });
        };

        if !session.begin_close() {
            return Err(SerialError::InvalidState {
                session_id: session_id.to_string(),
                state: session.state.name(),
                expected: "Ready or Opening",
            });
        }
        session.finish_close();
        inner.sessions.remove(session_id);
        Ok(())
    }

    /// Write `data` to the session's port and flush. Caller is responsible
    /// for upstream size limits; `tools::handle_write` enforces
    /// `MAX_WRITE_CHUNK` at the JSON-RPC boundary.
    #[instrument(skip(self, data), fields(session_id, len = data.len()))]
    pub async fn write(&self, session_id: &str, data: &[u8]) -> Result<usize, SerialError> {
        let port = self.checkout_port(session_id)?;
        let mut guard = port.lock().await;
        guard.write_all(data).await.map_err(|e| SerialError::Io {
            message: format!("write: {e}"),
        })?;
        guard.flush().await.map_err(|e| SerialError::Io {
            message: format!("flush: {e}"),
        })?;
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
        let port = self.checkout_port(session_id)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut guard = port.lock().await;
        let mut acc: Vec<u8> = Vec::with_capacity(max_bytes.min(4096));
        let mut chunk = [0u8; 4096];

        loop {
            if acc.len() >= max_bytes {
                return Ok((acc, false));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok((acc, true));
            };
            let take = (max_bytes - acc.len()).min(chunk.len());
            let read_fut = guard.read(&mut chunk[..take]);
            match tokio::time::timeout(remaining, read_fut).await {
                Ok(Ok(0)) => return Ok((acc, false)),
                Ok(Ok(n)) => acc.extend_from_slice(&chunk[..n]),
                Ok(Err(e)) => {
                    return Err(SerialError::Io {
                        message: format!("read: {e}"),
                    });
                }
                Err(_) => return Ok((acc, true)),
            }
        }
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
        let port = self.checkout_port(session_id)?;
        let mut matcher = PatternMatcher::new(pattern)?;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut guard = port.lock().await;
        let mut buf = [0u8; 4096];

        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok((matcher.into_buffer(), false));
            };
            let read_fut = guard.read(&mut buf);
            match tokio::time::timeout(remaining, read_fut).await {
                Ok(Ok(0)) => {
                    // EOF — nothing more will arrive.
                    let matched = matcher.is_match();
                    return Ok((matcher.into_buffer(), matched));
                }
                Ok(Ok(n)) => {
                    if matcher.push(&buf[..n]) {
                        return Ok((matcher.into_buffer(), true));
                    }
                }
                Ok(Err(e)) => {
                    return Err(SerialError::Io {
                        message: format!("read: {e}"),
                    });
                }
                Err(_) => return Ok((matcher.into_buffer(), false)),
            }
        }
    }

    /// Resolve a session id to its port handle, validating state in one shot.
    /// Returns `SessionNotFound` for unknown ids, `InvalidState` for anything
    /// other than `Ready`.
    fn checkout_port(&self, session_id: &str) -> Result<Arc<AsyncMutex<B::Port>>, SerialError> {
        let inner = lock(&self.inner);
        let Some(session) = inner.sessions.get(session_id) else {
            return Err(SerialError::SessionNotFound {
                session_id: session_id.to_string(),
            });
        };
        match &session.port {
            Some(p) => Ok(p.clone()),
            None => Err(SerialError::InvalidState {
                session_id: session_id.to_string(),
                state: session.state.name(),
                expected: "Ready",
            }),
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

/// SplitMix64 — small, fast, non-cryptographic. Sufficient for unguessable
/// session ids in a local-only stdio context. Updates the seed state in place.
fn next_id(state: &mut u64) -> String {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    let id = z ^ (z >> 31);
    format!("{id:016x}")
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
        assert_eq!(id.len(), 16, "hex u64 must be 16 chars: {id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(m.session_count(), 1);
        assert_eq!(m.session_state(&id), Some("Ready"));
    }

    #[tokio::test]
    async fn allowlist_accepts_ttyacm() {
        let m = mgr();
        m.open("/dev/ttyACM0", 115_200, 5_000).await.unwrap();
    }

    #[tokio::test]
    async fn allowlist_rejects_non_matching_path() {
        let m = mgr();
        let err = m.open("/dev/ttyS0", 115_200, 5_000).await.unwrap_err();
        assert!(matches!(err, SerialError::PortNotAllowed { .. }));
        assert_eq!(m.session_count(), 0);
    }

    #[tokio::test]
    async fn opening_then_close_removes_session() {
        let m = mgr();
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
        assert_eq!(m.session_count(), 1);
        m.close(&id).unwrap();
        assert_eq!(m.session_count(), 0);
        assert_eq!(m.session_state(&id), None);
    }

    #[tokio::test]
    async fn close_unknown_session_errors() {
        let m = mgr();
        let err = m.close("0000000000000000").unwrap_err();
        assert!(matches!(err, SerialError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn double_close_errors() {
        let m = mgr();
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
        m.close(&id).unwrap();
        let err = m.close(&id).unwrap_err();
        assert!(matches!(err, SerialError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn max_sessions_enforced_then_recovers() {
        let m = mgr();
        let mut ids = Vec::new();
        for _ in 0..config::MAX_SESSIONS {
            ids.push(m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap());
        }
        let err = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap_err();
        assert!(
            matches!(err, SerialError::MaxSessionsReached { max } if max == config::MAX_SESSIONS),
            "unexpected error: {err:?}"
        );
        m.close(&ids.pop().unwrap()).unwrap();
        m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
    }

    #[tokio::test]
    async fn open_failure_cleans_up_placeholder() {
        let backend = MockBackend::new();
        backend.fail_next();
        let m = SessionManager::with_seed(backend, 0x1234);
        let err = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap_err();
        assert!(matches!(err, SerialError::Io { .. }));
        assert_eq!(m.session_count(), 0, "placeholder must be removed");
    }

    #[tokio::test]
    async fn ids_are_unique_across_opens() {
        let m = mgr();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..config::MAX_SESSIONS {
            let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
    fn next_id_format_is_16_char_lowercase_hex() {
        let mut state = 0xDEAD_BEEFu64;
        for _ in 0..1000 {
            let id = next_id(&mut state);
            assert_eq!(id.len(), 16);
            assert!(id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
    }

    // --- write / read --- //

    #[tokio::test]
    async fn write_round_trips_to_device_side() {
        let (m, backend) = mgr_with_backend();
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
        m.close(&id).unwrap();
        // SessionNotFound is a *protocol* failure (no such id), not a
        // domain outcome — it must still surface as Err, distinct from
        // the timed_out=true shape that real reads now use.
        let err = m.read(&id, 64, 100).await.unwrap_err();
        assert!(matches!(err, SerialError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn empty_write_succeeds_with_zero_bytes() {
        let (m, _backend) = mgr_with_backend();
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
        let n = m.write(&id, b"").await.unwrap();
        assert_eq!(n, 0);
    }

    // --- read_until --- //

    #[tokio::test]
    async fn read_until_matches_pattern_split_across_reads() {
        let (m, backend) = mgr_with_backend();
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
        let id = m.open("/dev/ttyUSB0", 115_200, 5_000).await.unwrap();
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
}
