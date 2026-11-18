<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# ADR 0003: Use cryptographic session IDs and user-private audit journals

## Status

Accepted.

## Context

The previous session IDs were 16-character hex strings derived from a
non-cryptographic generator seeded with time and process ID. The default audit
journal path was under `/tmp`, a shared directory.

The server is local stdio today, but multiple agents or subagents can share a
server process, and journal rows expose session IDs and command/output
summaries. Future transports or wrappers may widen the threat model.

## Decision

Production session IDs are 128 bits from the OS random source, encoded as 32
lowercase hex characters. Deterministic generation remains only for tests.

The default audit journal resolves to a user-private state path:

- `$XDG_STATE_HOME/mcp-serial-rs/audit.jsonl`;
- otherwise `~/.local/state/mcp-serial-rs/audit.jsonl`;
- otherwise a relative fallback.

On Unix, newly created journal parent directories are set to `0700` and the
file is set to `0600` through the opened handle. Final-component symlinks,
FIFOs, and non-regular journal files are rejected promptly.

Default journal summaries are metadata-only. They record bounded argument-key
lists, byte counts, statuses, and error codes, not command text, serial output
heads, or error messages. Journal lock/write/flush operations use short
deadlines so an unhealthy journal sink cannot indefinitely block tool dispatch.

## Consequences

The session ID format changed from 16 to 32 hex characters. Clients must treat
session IDs as opaque strings.

The journal was best-effort when this was decided: if the path was unsafe or
could not be opened, the server logged a warning and continued without
journaling. The amendment below replaces that. Rotation is deferred.

## Amendment: the audit log fails closed

Running the server in a sandbox with a read-only home directory showed the
cost of best-effort auditing: the journal failed to open, one warning went to
stderr, and every serial tool call after that ran unaudited.

The audit log is now required:

- The server opens it before serving any request. If that fails, it prints
  one stderr line with the path, the OS error, and the fix, then exits with
  status 1.
- `--audit-log <path>` sets the path and takes precedence over
  `MCP_SERIAL_JOURNAL`. The default path is unchanged.
- The first failed write (I/O error, or the lock or write deadline passing)
  marks the log as failed until the process exits. From then on every tool
  except `serial.close` returns `AuditLogUnavailable` (`-32017`,
  `audit_log_unavailable`, not retryable, `command_written=false`,
  `session_usable=false`) without dispatching. A call whose own `call` row
  fails is refused the same way. A failed `result` row still returns the real
  result, because the I/O has already happened.
- `serial.close` stays available so ports can be released without a restart.
  It only releases a port, and it runs without an audit row; stderr records
  each one.
- The failed state is never cleared by a later successful write. After a
  failure nothing more is written, so a partly flushed row cannot be completed
  into a corrupt line.
- A row over the 16 KiB cap is still written, with its summary replaced by
  `{"summary_omitted": "too_large", "summary_bytes": N}`. Summary size depends
  on caller input (JSON escaping can multiply it), so it must neither drop a
  row nor mark the log as failed.
- A call is admitted only if its row is written and the log had not failed by
  then, so a failure recorded while another call's flush is pending refuses
  that call too.
- At open, a last line left without its newline by a write that failed partway
  in an earlier run is ended, so the next row starts on its own line.
- There is no option to run without the audit log. The row format is
  unchanged.

A stall over the 250 ms deadline now disables serial tools until restart.
That is deliberate: a missing row cannot be told apart from a lost one.
