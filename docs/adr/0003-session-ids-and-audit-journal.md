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

The journal still remains best-effort: if the path is unsafe or cannot be
opened, the server logs a warning and continues without journaling. Rotation
and required/disabled policy modes are deferred.
