<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# ADR 0002: Make `serial.exec` atomic and preserve raw serial output

## Status

Accepted.

## Context

`serial.exec` used to compose `write` followed by `read_until` through two
separate manager calls. Each call independently locked the port, allowing
another same-session operation to interleave between the command write and
response read.

A live probe opened an allowlisted path directly onto an interactive, privileged
device shell and returned command echo, line wrapping, prompts, CRLF, and OSC
shell-integration escape sequences. On such a privileged endpoint, incorrectly
attributed writes or responses are high impact.

## Decision

`serial.exec` is a manager-level operation that holds one per-session port lock
across:

1. optional input clear;
2. command write;
3. flush;
4. read-until matching or terminal status.

The result preserves raw lossy-UTF-8 output. Optional normalization may add a
second field, but it must not replace `raw_output` or silently remove terminal
controls, command echo, or prompts.

## Consequences

Same-session operations queue behind an in-flight `exec`. This is intentional:
the server prioritizes correct response attribution over same-session
parallelism. Different sessions can still progress independently.

`serial.exec` results are larger but include recovery-critical state:
`status`, byte counts, match details, `command_written`, and
`session_usable`.
