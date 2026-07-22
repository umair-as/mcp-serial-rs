<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# ADR 0007: Server-owned command policy

## Status

Accepted.

## Decision

Profiles may define `deny_patterns` and `allow_patterns` for complete
`serial.exec` commands. The server also accepts a JSON-array global deny policy
through `MCP_SERIAL_DENY_PATTERNS`. The effective policy unions global and
profile deny rules; a caller may add `deny_patterns` at `serial.open`, but can
never remove rules or provide allow rules.

Rules compile before a session opens and remain immutable for its lifetime. A
matching deny rule, or a non-matching command when allow rules exist, is
rejected before port checkout with `command_blocked` (`-32014`). Error and
journal metadata identify only a generated rule name, never command text or
the full regex policy.

Raw `serial.write` is refused on a session with any command policy
(`raw_write_forbidden`, `-32015`). It remains an unbuffered verbatim primitive;
buffering or best-effort per-write matching would allow callers to split a
dangerous command across writes. Guarded mutation therefore goes through the
complete-command `serial.exec` path.

`-32015` is therefore reserved for `raw_write_forbidden`; a future elicitation
feature must use a different code than the provisional number in its planning
specification.

## Consequences

The policy is server-owned and model-proof for guarded sessions, while the
existing `write_policy=deny` remains the broader read-only control. This ADR
does not add human elicitation; that is a separate future decision.
