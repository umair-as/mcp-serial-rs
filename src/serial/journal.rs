// SPDX-License-Identifier: MIT OR Apache-2.0

//! Append-only JSONL traffic journal for every MCP tool call.
//!
//! One line per [`JournalEntry`]; each tool invocation produces a `call`
//! entry before dispatch and a `result` entry after. Auditing is always on
//! and fails closed (ADR 0003): `main` refuses to start when the journal
//! cannot be opened, and the first failed write marks the writer as failed
//! for the rest of the process, so the server stops touching serial ports.
//!
//! Concurrency: the inner `BufWriter` sits behind a `tokio::sync::Mutex`
//! because dispatch tasks can in principle interleave on the runtime. The
//! mutex is held only across a single line-write + flush, which is short
//! and ordered (lines never interleave).
//!
//! Time format: ISO 8601 UTC with millisecond precision
//! (`YYYY-MM-DDTHH:MM:SS.sssZ`). Hand-rolled rather than pulling in a date
//! crate, to avoid a new runtime dependency.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex as TokioMutex;
use tracing::{error, warn};

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

/// Anyone who can write the journal's directory can rename or replace the
/// journal file. A world-writable directory without the sticky bit is refused
/// (degraded mode); a group-writable one only warns, because a umask of 002
/// makes that common for a user's own directories.
#[cfg(unix)]
async fn check_existing_parent(parent: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = tokio::fs::metadata(parent).await?.permissions().mode();
    let sticky = mode & 0o1000 != 0;
    if mode & 0o002 != 0 && !sticky {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "journal directory is world-writable without the sticky bit",
        ));
    }
    if mode & 0o020 != 0 && !sticky {
        warn!(
            path = %parent.display(),
            mode = format!("{:o}", mode & 0o7777),
            "journal directory is group-writable; group members could replace the journal"
        );
    }
    Ok(())
}

fn clip_identifier(value: &str) -> String {
    value.chars().take(MAX_IDENTIFIER_CHARS).collect()
}

/// Wraps an append-mode file. `log` serialises an entry as a single JSONL
/// line and flushes the underlying writer so a `tail -f` consumer sees rows
/// promptly. Any write, flush, lock or deadline failure marks the writer as
/// failed for good: later `log` calls return an error without writing, so a
/// half-flushed buffer can never land as a corrupt line, and `call_tool`
/// refuses serial tools from then on.
#[derive(Debug)]
pub struct JournalWriter {
    inner: TokioMutex<BufWriter<tokio::fs::File>>,
    path: PathBuf,
    failed: AtomicBool,
}

impl JournalWriter {
    /// Open `path` in create+append mode. Returns the writer on success or
    /// the underlying I/O error; `main` treats the error as fatal.
    pub async fn open(path: &Path) -> std::io::Result<Self> {
        // A bare file name has an empty parent: the working directory. It
        // already exists and is not ours to chmod, but it gets the same
        // safety check as any other existing parent.
        let parent = path.parent().map(|p| {
            if p.as_os_str().is_empty() {
                Path::new(".")
            } else {
                p
            }
        });
        if let Some(parent) = parent {
            let parent_existed = tokio::fs::symlink_metadata(parent).await.is_ok();
            tokio::fs::create_dir_all(parent).await?;
            #[cfg(unix)]
            if parent_existed {
                check_existing_parent(parent).await?;
            } else {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
            }
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true).read(true);
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
        let mut file = file;
        terminate_last_line(&mut file, metadata.len()).await?;
        Ok(Self::from_file(file, path))
    }

    /// Wrap an already-open file without the path checks in [`Self::open`].
    /// Tests use it to journal into a device such as `/dev/full`.
    #[doc(hidden)]
    pub fn from_file(file: tokio::fs::File, path: &Path) -> Self {
        Self {
            inner: TokioMutex::new(BufWriter::new(file)),
            path: path.to_path_buf(),
            failed: AtomicBool::new(false),
        }
    }

    /// Path the journal was opened at — surfaced for diagnostics / tests.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True once a write has failed. It stays true until the process exits.
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    /// Serialise `entry` to JSONL, write it and flush.
    ///
    /// A row over the size cap is still written, with its summary replaced
    /// by a marker, so caller-supplied arguments can neither drop a row nor
    /// mark the writer as failed. Every other failure marks the writer as
    /// failed; from then on `log` returns an error without writing. `Ok`
    /// means this row is on disk and the writer had not failed by then.
    pub async fn log(&self, entry: &JournalEntry) -> std::io::Result<()> {
        let mut line = self.encode(entry)?;
        if line.len() > MAX_JOURNAL_ROW_BYTES {
            warn!(
                tool = %entry.tool,
                len = line.len(),
                max = MAX_JOURNAL_ROW_BYTES,
                "journal summary too large; writing the row without it"
            );
            let marked = JournalEntry {
                summary: serde_json::json!({
                    "summary_omitted": "too_large",
                    "summary_bytes": line.len(),
                }),
                ..entry.clone()
            };
            line = self.encode(&marked)?;
            // Identifiers are clipped at construction, so this cannot fire.
            if line.len() > MAX_JOURNAL_ROW_BYTES {
                return Err(self.fail(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "journal row too large even without its summary",
                )));
            }
        }
        line.push(b'\n');
        let deadline = std::time::Duration::from_millis(JOURNAL_IO_TIMEOUT_MS);
        let Ok(mut guard) = tokio::time::timeout(deadline, self.inner.lock()).await else {
            return Err(self.fail(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "journal lock timed out",
            )));
        };
        // Checked under the lock: a call queued behind a failing write must
        // not flush that write's leftover bytes.
        if self.has_failed() {
            return Err(failed_error());
        }
        let result = tokio::time::timeout(deadline, async {
            guard.write_all(&line).await?;
            guard.flush().await
        })
        .await;
        match result {
            // Another call may have failed while this flush was pending (a
            // lock timeout is recorded without the lock). Once failed, no
            // call may be admitted, so report this one as refused too.
            Ok(Ok(())) if self.has_failed() => Err(failed_error()),
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(self.fail(e)),
            Err(_) => Err(self.fail(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "journal write timed out",
            ))),
        }
    }

    fn encode(&self, entry: &JournalEntry) -> std::io::Result<Vec<u8>> {
        serde_json::to_vec(entry).map_err(|e| self.fail(std::io::Error::other(e)))
    }

    /// Mark the writer failed, logging the first failure only.
    fn fail(&self, err: std::io::Error) -> std::io::Error {
        if !self.failed.swap(true, Ordering::SeqCst) {
            error!(
                error = %err,
                path = %self.path.display(),
                "audit log write failed; serial tools are disabled until restart"
            );
        }
        err
    }
}

/// A write that failed partway in an earlier run can leave a last row with
/// no newline. End it so the first new row starts on its own line.
async fn terminate_last_line(file: &mut tokio::fs::File, len: u64) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    if len == 0 {
        return Ok(());
    }
    file.seek(std::io::SeekFrom::End(-1)).await?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last).await?;
    if last[0] != b'\n' {
        file.write_all(b"\n").await?;
        file.flush().await?;
    }
    Ok(())
}

fn failed_error() -> std::io::Error {
    std::io::Error::other("audit log failed earlier; no further rows are written")
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
        writer.log(&a).await.unwrap();
        writer.log(&b).await.unwrap();

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

    /// Caller input decides the summary size, so an oversized summary must
    /// neither drop the row nor disable the log.
    #[tokio::test]
    async fn writer_replaces_oversized_summary_and_stays_healthy() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let writer = JournalWriter::open(tmp.path()).await.unwrap();

        let oversized = JournalEntry::new(
            "none",
            "serial.exec",
            JournalEntry::DIR_CALL,
            json!({"blob": "\u{1}".repeat(MAX_JOURNAL_ROW_BYTES / 4)}),
        );
        writer.log(&oversized).await.unwrap();
        assert!(!writer.has_failed());

        let normal = JournalEntry::new(
            "none",
            "serial.list_ports",
            JournalEntry::DIR_RESULT,
            json!({"ok": true, "port_count": 1}),
        );
        writer.log(&normal).await.unwrap();

        let contents = std::fs::read_to_string(tmp.path()).unwrap();
        let rows: Vec<JournalEntry> = contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2, "{contents}");
        assert_eq!(rows[0].tool, "serial.exec");
        assert_eq!(rows[0].summary["summary_omitted"], "too_large");
        assert!(rows[0].summary["summary_bytes"].as_u64().unwrap() > MAX_JOURNAL_ROW_BYTES as u64);
        assert_eq!(rows[1].tool, "serial.list_ports");
    }

    /// A failure recorded while another call's flush is pending must refuse
    /// that call too. The journal is a socket whose send buffer is full, so
    /// the flush blocks until the test drains the peer.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_refuses_pending_row_after_failure_latches() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        let (mut log_end, mut peer) = UnixStream::pair().unwrap();
        log_end.set_nonblocking(true).unwrap();
        while log_end.write(&[b'x'; 4096]).is_ok() {}
        log_end.set_nonblocking(false).unwrap();
        let file =
            tokio::fs::File::from_std(std::fs::File::from(std::os::fd::OwnedFd::from(log_end)));
        let writer = std::sync::Arc::new(JournalWriter::from_file(file, Path::new("socket")));

        let pending = {
            let writer = writer.clone();
            tokio::spawn(async move {
                let entry =
                    JournalEntry::new("none", "serial.write", JournalEntry::DIR_CALL, json!({}));
                writer.log(&entry).await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !pending.is_finished(),
            "flush must be blocked on the full socket"
        );

        writer.fail(std::io::Error::other("failure from another call"));
        let drain = std::thread::spawn(move || {
            peer.set_read_timeout(Some(std::time::Duration::from_millis(300)))
                .unwrap();
            let mut buf = [0u8; 65536];
            while matches!(peer.read(&mut buf), Ok(n) if n > 0) {}
        });

        let result = pending.await.unwrap();
        drain.join().unwrap();
        let err = result.expect_err("a row admitted after the failure latched");
        assert_eq!(err.kind(), std::io::ErrorKind::Other, "{err}");
        assert_eq!(err.to_string(), failed_error().to_string());
    }

    /// A bare file name lives in the working directory, which must pass the
    /// same parent check as any other directory.
    #[cfg(unix)]
    #[tokio::test]
    async fn writer_checks_working_directory_for_bare_file_name() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        // `open` resolves "." against the process working directory, so
        // drive the check directly rather than changing it for every test.
        let err = check_existing_parent(temp.path()).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// A row left without its newline by an earlier failed write is ended at
    /// open, so the next row is not glued onto it.
    #[tokio::test]
    async fn writer_ends_a_partial_last_line_at_open() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"{\"ts\":\"partial").unwrap();
        let writer = JournalWriter::open(tmp.path()).await.unwrap();
        let entry = JournalEntry::new(
            "none",
            "serial.list_ports",
            JournalEntry::DIR_CALL,
            json!({}),
        );
        writer.log(&entry).await.unwrap();

        let contents = std::fs::read_to_string(tmp.path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "{contents}");
        let parsed: JournalEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed.tool, "serial.list_ports");

        // A file that already ends in a newline is left alone.
        drop(writer);
        let _writer = JournalWriter::open(tmp.path()).await.unwrap();
        assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), contents);
    }

    #[tokio::test]
    async fn writer_open_on_unwritable_path_errors() {
        // Opening a journal inside a non-existent directory cannot succeed;
        // `main` turns this error into a refusal to start.
        let _err = JournalWriter::open(Path::new("/proc/1/no-such-journal"))
            .await
            .unwrap_err();
    }

    /// `/dev/full` fails every write with ENOSPC. The first failure must be
    /// reported and stick: later rows are refused without being written.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn writer_failure_is_sticky() {
        let path = Path::new("/dev/full");
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .await
            .unwrap();
        let writer = JournalWriter::from_file(file, path);
        let entry = JournalEntry::new(
            "none",
            "serial.list_ports",
            JournalEntry::DIR_CALL,
            json!({}),
        );

        assert!(!writer.has_failed());
        let first = writer.log(&entry).await.unwrap_err();
        assert_eq!(first.raw_os_error(), Some(libc::ENOSPC), "{first}");
        assert!(writer.has_failed());

        let second = writer.log(&entry).await.unwrap_err();
        assert_eq!(
            second.raw_os_error(),
            None,
            "no second write attempt: {second}"
        );
        assert!(writer.has_failed());
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
    async fn writer_checks_existing_parent_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let set_mode = |mode| {
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(mode)).unwrap()
        };
        let path = temp.path().join("audit.jsonl");

        set_mode(0o777);
        let err = JournalWriter::open(&path).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!path.exists(), "refused before creating the journal");

        // Sticky (like /tmp) and group-writable (umask 002) parents are usable.
        set_mode(0o1777);
        JournalWriter::open(&path).await.unwrap();
        set_mode(0o770);
        JournalWriter::open(&path).await.unwrap();
        set_mode(0o700);
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
