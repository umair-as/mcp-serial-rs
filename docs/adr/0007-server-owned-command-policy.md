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

## Amendment (2026-10-14): guarded commands are single lines

A regex judges one `exec` call's `command`, but a shell judges *lines*. Refusing
raw `serial.write` alone did not stop a caller from assembling a line across
`exec` calls: `{command:"reb", line_ending:"none"}` then `{command:"oot",
line_ending:"lf"}` delivers `reboot\n` while neither call matches `\breboot\b`.
An embedded newline likewise let an anchored allow rule such as `^uname` admit
`uname -a\nreboot`.

On a session with any command policy, `serial.exec` now also refuses, with
`command_blocked` and a generated `shape.*` rule id, before port checkout:

- `shape.line_ending_override`: a per-call `line_ending` that differs from the
  session's. The line ending is fixed per guarded session; `serial.open` gains
  an optional `line_ending` so a session without a profile default can choose
  one. Restating the session's own ending is allowed.
- `shape.control_char`: any Unicode control character in `command` (C0
  including CR/LF/TAB/ESC, DEL, C1). These can end, edit, or recall a line.
- `shape.line_continuation`: a trailing backslash, which a POSIX shell joins
  with the next line.

The ADR 0006 default (`line_ending = none`, transmit unchanged) is preserved. A
guarded session whose line ending is `none` stays safe but cannot submit a
line; give the profile, or the `serial.open` call, a `line_ending`. Sessions
without a policy are unchanged, including multi-line commands.

**Limits.** These checks keep each guarded call to exactly one line. They do not
make *deny* rules a complete gate: shell quoting and expansion (`r''eboot`,
`$(printf reboot)`, `/sbin/re*t`) can still express a denied command a regex
does not match. Deny rules are defence in depth. An allow list anchored at
both ends over a narrow character set (`\A(uname|uptime)( -[a-z]+)?\z`) is
the strong form of command policy, and `write_policy = "deny"` remains the only
model-proof control.

## Consequences

The policy is server-owned and model-proof for guarded sessions, while the
existing `write_policy=deny` remains the broader read-only control. This ADR
does not add human elicitation; that is a separate future decision.
