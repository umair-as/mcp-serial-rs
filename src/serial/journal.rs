// SPDX-License-Identifier: MIT OR Apache-2.0

//! Append-only JSONL traffic journal for every MCP tool call.
//!
//! One line per [`JournalEntry`]; each tool invocation produces a `call`
//! entry before dispatch and a `result` entry after. Per CLAUDE.md §6 this
//! is always-on auditing — not opt-in — but I/O failures degrade gracefully.
//! Open failure leaves the server with no journal handle; per-row failures are
//! logged via `tracing::warn` and the writer is retained for later retry.
//!
//! Concurrency: the inner `BufWriter` sits behind a `tokio::sync::Mutex`
//! because dispatch tasks can in principle interleave on the runtime. The
//! mutex is held only across a single line-write + flush, which is short
//! and ordered (lines never interleave).
//!
//! Time format: ISO 8601 UTC with millisecond precision
//! (`YYYY-MM-DDTHH:MM:SS.sssZ`). Hand-rolled rather than pulling in a date
//! crate — CLAUDE.md §2 forbids new runtime dependencies without approval.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex as TokioMutex;
use tracing::warn;

const MAX_IDENTIFIER_CHARS: usize = 128;
const JOURNAL_IO_TIMEOUT_MS: u64 = 250;
const MAX_JOURNAL_ROW_BYTES: usize = 16 * 1024;

/// One row in the journal. `summary` is tool-specific and metadata-only by
/// default; producers must keep it small so the journal stays line-oriented
/// and tail-able.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub ts: String,
    pub session_id: String,
    pub tool: String,
    pub direction: String,
    pub summary: serde_json::Value,
}

impl JournalEntry {
    pub const DIR_CALL: &'static str = "call";
    pub const DIR_RESULT: &'static str = "result";
    /// Sentinel placed in `session_id` for tools that don't have one
    /// (lifecycle methods, `serial.list_ports`, `serial.open` `call` entries
    /// — the id only exists once `open` resolves).
    pub const NO_SESSION: &'static str = "none";

    pub fn new(
        session_id: impl Into<String>,
        tool: impl Into<String>,
        direction: &'static str,
        summary: serde_json::Value,
    ) -> Self {
        Self {
            ts: iso8601_now(),
            session_id: clip_identifier(&session_id.into()),
            tool: clip_identifier(&tool.into()),
            direction: direction.into(),
            summary,
        }
    }
}

fn clip_identifier(value: &str) -> String {
    value.chars().take(MAX_IDENTIFIER_CHARS).collect()
}

/// Wraps an append-mode file. `log` serialises an entry as a single JSONL
/// line and flushes the underlying writer so a `tail -f` consumer sees rows
/// promptly. Errors are logged via `tracing::warn` and swallowed — by spec
/// journaling must never break tool execution.
#[derive(Debug)]
pub struct JournalWriter {
    inner: TokioMutex<BufWriter<tokio::fs::File>>,
    path: PathBuf,
}

impl JournalWriter {
    /// Open `path` in create+append mode. Returns the writer on success or
    /// the underlying I/O error — callers in `main.rs` log the error and
    /// fall back to `journal = None` (degraded mode) rather than aborting.
    pub async fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            let parent_existed = tokio::fs::symlink_metadata(parent).await.is_ok();
            tokio::fs::create_dir_all(parent).await?;
            #[cfg(unix)]
            if !parent_existed {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
            }
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path).await?;
        let metadata = file.metadata().await?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "journal path is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        Ok(Self {
            inner: TokioMutex::new(BufWriter::new(file)),
            path: path.to_path_buf(),
        })
    }

    /// Open `path` and wrap in `Arc`; on failure, log a warning and return
    /// `None` so callers can stay in degraded mode without branching on
    /// `Result`. Centralises the degraded-mode log message in one place.
    pub async fn try_open_arc(path: &Path) -> Option<Arc<Self>> {
        match Self::open(path).await {
            Ok(w) => {
                tracing::info!(path = %path.display(), "journal opened");
                Some(Arc::new(w))
            }
            Err(e) => {
                warn!(
                    error = %e,
                    path = %path.display(),
                    "journal open failed — continuing in degraded mode (no auditing)"
                );
                None
            }
        }
    }

    /// Path the journal was opened at — surfaced for diagnostics / tests.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serialise `entry` to JSONL and flush. Any failure is logged at warn
    /// level and discarded; the caller continues. We deliberately do NOT
    /// poison `self` on error: subsequent writes will retry, which matters
    /// when failures are transient (e.g. tmpfs full, then drained).
    pub async fn log(&self, entry: &JournalEntry) {
        let mut line = match serde_json::to_vec(entry) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "journal serialise failed");
                return;
            }
        };
        if line.len() > MAX_JOURNAL_ROW_BYTES {
            warn!(
                path = %self.path.display(),
                len = line.len(),
                max = MAX_JOURNAL_ROW_BYTES,
                "journal row too large; dropping row"
            );
            return;
        }
        line.push(b'\n');
        let Ok(mut guard) = tokio::time::timeout(
            std::time::Duration::from_millis(JOURNAL_IO_TIMEOUT_MS),
            self.inner.lock(),
        )
        .await
        else {
            warn!(path = %self.path.display(), "journal lock timed out");
            return;
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(JOURNAL_IO_TIMEOUT_MS),
            async {
                guard.write_all(&line).await?;
                guard.flush().await
            },
        )
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, path = %self.path.display(), "journal write failed"),
            Err(_) => warn!(path = %self.path.display(), "journal write timed out"),
        }
    }
}

/// Format `SystemTime::now()` as `YYYY-MM-DDTHH:MM:SS.sssZ`. Pre-epoch
/// clocks collapse to the epoch — we don't need a faithful representation,
/// just a monotone-ish timestamp that's safe to compare lexically.
fn iso8601_now() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso8601_from_secs(dur.as_secs() as i64, dur.subsec_millis())
}

fn iso8601_from_secs(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let h = (tod / 3600) as u32;
    let mi = ((tod % 3600) / 60) as u32;
    let s = (tod % 60) as u32;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// Howard Hinnant's date algorithm: days-since-Unix-epoch → (year, month,
/// day). See https://howardhinnant.github.io/date_algorithms.html#civil_from_days.
/// Returns month in 1..=12 and day in 1..=31.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift epoch from 1970-01-01 to 0000-03-01 (start of an internal "era").
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day-of-era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso8601_epoch_renders_zero() {
        assert_eq!(iso8601_from_secs(0, 0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn iso8601_known_dates() {
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(
            iso8601_from_secs(1_704_067_200, 0),
            "2024-01-01T00:00:00.000Z"
        );
        // 2026-05-18T12:34:56.789Z. Days = 20454 (1970→2026-01-01: 14 leap +
        // 42 non-leap) + 137 (Jan..Apr = 120, +17 May) = 20591. Secs = 20591
        // * 86400 + 12*3600 + 34*60 + 56 = 1_779_107_696.
        assert_eq!(
            iso8601_from_secs(1_779_107_696, 789),
            "2026-05-18T12:34:56.789Z"
        );
    }

    #[test]
    fn iso8601_pre_epoch_clamps() {
        // Pre-epoch input should still render a valid ISO string (year < 1970).
        let s = iso8601_from_secs(-86_400, 0);
        assert!(s.ends_with("T00:00:00.000Z"), "got: {s}");
        assert!(s.starts_with("1969-12-31"), "got: {s}");
    }

    #[tokio::test]
    async fn writer_appends_one_jsonl_line_per_log() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let writer = JournalWriter::open(tmp.path()).await.unwrap();

        let a = JournalEntry::new(
            "none",
            "initialize",
            JournalEntry::DIR_CALL,
            json!({"id": 1}),
        );
        let b = JournalEntry::new(
            "deadbeef",
            "serial.write",
            JournalEntry::DIR_RESULT,
            json!({"ok": true, "bytes_written": 5}),
        );
        writer.log(&a).await;
        writer.log(&b).await;

        let contents = std::fs::read_to_string(tmp.path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines, got:\n{contents}");

        let parsed_a: JournalEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed_a.tool, "initialize");
        assert_eq!(parsed_a.direction, "call");
        assert_eq!(parsed_a.session_id, "none");
        assert!(
            parsed_a.ts.ends_with('Z'),
            "ts must be UTC: {}",
            parsed_a.ts
        );

        let parsed_b: JournalEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed_b.tool, "serial.write");
        assert_eq!(parsed_b.direction, "result");
        assert_eq!(parsed_b.session_id, "deadbeef");
        assert_eq!(parsed_b.summary["bytes_written"], 5);
    }

    #[tokio::test]
    async fn writer_drops_oversized_row_and_recovers() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let writer = JournalWriter::open(tmp.path()).await.unwrap();

        let oversized = JournalEntry::new(
            "none",
            "serial.write",
            JournalEntry::DIR_CALL,
            json!({"blob": "x".repeat(MAX_JOURNAL_ROW_BYTES + 1)}),
        );
        writer.log(&oversized).await;

        let normal = JournalEntry::new(
            "none",
            "serial.list_ports",
            JournalEntry::DIR_RESULT,
            json!({"ok": true, "port_count": 1}),
        );
        writer.log(&normal).await;

        let contents = std::fs::read_to_string(tmp.path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "oversized row must be dropped");
        let parsed: JournalEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.tool, "serial.list_ports");
    }

    #[tokio::test]
    async fn writer_open_on_unwritable_path_errors() {
        // Opening a journal inside a non-existent directory cannot succeed,
        // and the error is what feeds the degraded-mode branch in main.rs.
        let _err = JournalWriter::open(Path::new("/proc/1/no-such-journal"))
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn try_open_arc_returns_none_in_degraded_mode() {
        // Unwritable path → tracing::warn + None, no panic.
        let result = JournalWriter::try_open_arc(Path::new("/proc/1/no-such-journal")).await;
        assert!(result.is_none(), "must degrade to None on open failure");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writer_creates_new_parent_private_and_file_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("state").join("mcp-serial-rs");
        let path = dir.join("audit.jsonl");
        let _writer = JournalWriter::open(&path).await.unwrap();

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writer_rejects_final_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.jsonl");
        let link = temp.path().join("audit.jsonl");
        std::fs::write(&target, b"keep\n").unwrap();
        symlink(&target, &link).unwrap();

        let err = JournalWriter::open(&link).await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writer_rejects_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let temp = tempfile::tempdir().unwrap();
        let fifo = temp.path().join("audit.fifo");
        let c_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed");

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            JournalWriter::open(&fifo),
        )
        .await
        .expect("journal open must not block on FIFO");
        assert!(result.is_err(), "FIFO must not be accepted as a journal");
    }
}
