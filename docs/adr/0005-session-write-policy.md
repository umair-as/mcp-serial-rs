<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# ADR 0005: Per-session write policy (`allow` / `confirm` / `deny`)

## Status

Accepted.

## Context

An allowlisted port can open directly onto an interactive, already-privileged
shell — a Linux debug UART frequently lands on a live `root@host:~#` prompt.
Before this change the server had no notion of a *write* being dangerous: any
`serial.write` / `serial.exec` sent bytes to whatever was on the line, and the
only thing keeping a root console safe was the caller's discipline.

This matters more because device output is untrusted **and** is typically piped
into an LLM. That is the textbook prompt-injection vector: a hostile banner or
MOTD could try to talk an automated caller into issuing a write. Nothing
server-side prevented it.

Full MCP elicitation (ask the human through the client mid-call) is the eventual
answer for "confirm this destructive action", but it depends on rmcp's
experimental `elicit` surface and on client support, and a client that does not
implement it needs a defined fallback. We wanted a guarantee that does not
depend on either.

## Decision

Add a per-session **write policy**, captured at `serial.open` and immutable for
the session's lifetime, enforced in the `write` / `exec` handlers **before** any
bytes reach the device:

- **`allow`** — writes proceed unconditionally. The default; preserves prior
  behavior and all existing call sites.
- **`confirm`** — `write` / `exec` require an explicit `confirm: true` on the
  call, else they fail with `ConfirmationRequired` (-32013). A `confirm` param
  is self-satisfiable by an automated caller, so this mode is a **tripwire +
  audit point + the seam a future elicitation upgrade turns into a real human
  prompt** — not a hard gate.
- **`deny`** — `write` / `exec` are refused with `WriteForbidden` (-32012)
  regardless of the call. This is a hard, **model-proof** read-only session: no
  bug, and no injected device text, can cause a write. Reads / `drain` /
  `clear_input` stay allowed — read-only means "no bytes to the device", not
  "no inspection".

The effective policy is `max(profile_default, open_param)` under the ordering
`Allow < Confirm < Deny` — **most-restrictive wins**. A caller may *escalate* a
session (e.g. force `deny` on any port) but may **not downgrade** a `privileged`
device-profile default. Device profiles gain an optional boolean `privileged`;
`privileged = true` implies a `confirm` default.

Gating lives in the MCP handlers, not the `SessionManager`. The policy value is
immutable, so reading it before the manager checks out the port is race-free
with respect to policy; a session closed concurrently is still caught by the
existing state check.

Errors are ordinary MCP tool errors (per ADR 0001), not JSON-RPC protocol
errors: `isError: true` with `command_written=false`, `bytes_consumed=false`,
and `session_usable=true` (the session is fine — only the write was gated).

The stable success result shapes (`WriteResult`, `ExecResult`) are **not**
reshaped. A denial surfaces in the audit journal automatically through the
existing error-row branch (`error_code` / `error_type`); the only additive
journal change is a metadata-only `confirm` flag on `write` / `exec` call rows
and a `write_policy` field on `open` call rows. `SessionSnapshot` gains a
`write_policy` field so `serial.get_session` / `serial.sessions` can report a
session's policy for recovery — an additive field, nothing renamed or removed.

## Consequences

Operators can open a privileged console read-only (`write_policy: "deny"`) and
get a guarantee the server cannot mutate it, independent of the client or the
model. `confirm` gives a lighter tripwire today and a clean upgrade path to
elicitation later (deferred; see ADR 0004). The default remains `allow`, so
existing consumers are unaffected.

The `confirm` mode's honest limitation — self-satisfiable by an automated
caller — is documented, not hidden: its value is the deliberate second step,
the audit record, and the future human-in-the-loop seam, with `deny` as the
real hard gate.
