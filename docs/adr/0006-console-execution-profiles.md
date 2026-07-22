<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# ADR 0006: Profile-aware console execution

## Status

Accepted.

## Context

`serial.exec` historically transmitted `command` byte-for-byte and never added
a line terminator. That raw default is necessary for bootloaders, monitors, and
other non-shell consoles, but an echoing shell-console validation exposed three
problems:

- a command without its own newline was echoed but never submitted;
- an `expect` pattern present in the command matched the local echo before the
  command ran; and
- narrow-terminal echo redraw inserted carriage returns into the transcript,
  making simple control-sequence stripping produce duplicated text.

The same target emits OSC 3008 semantic-prompt markers around command output.
Those markers can provide bounded command boundaries and success/failure
metadata, but they are device data and may be absent, malformed, duplicated, or
hostile. Raw output must remain authoritative.

An existing repository constraint unconditionally prohibited appending a
newline in `serial.exec`. That made the safe raw default impossible to combine
with an explicit profile-owned shell-console default. This ADR is the explicit
contract-level decision replacing that prohibition.

## Decision

Add immutable console-execution defaults to each session, resolved when it is
opened:

- `line_ending`: `none` (default), `lf`, `cr`, or `crlf`;
- `echo_mode`: `unknown` (default), `none`, or `line`;
- `semantic_prompt`: `none` (default) or `osc3008`.

Literal-port sessions use all-default settings. Device profiles may set these
fields, and `serial.exec` may override them per call.

`line_ending=none` preserves the existing contract exactly: `command` is
transmitted byte-for-byte with no implicit suffix. For any other effective
value, the server appends that terminator to the caller-provided command. This
is never an unrequested global behavior: it must come from the selected named
profile or the individual call.

The final transmitted byte sequence, including the terminator, is validated
against the write-size limit before device I/O. The existing session write
policy gates the whole operation before any command or terminator byte reaches
the port. Newline, carriage return, and control writes receive no confirmation
exemption.

For `echo_mode=line`, `serial.exec` preserves the complete echo in `raw_output`
but does not allow `expect` to match until the first echoed line terminator has
been observed. It therefore requires `line_ending` to be `lf`, `cr`, or
`crlf`; pairing it with `none` is rejected before device I/O. `unknown` and
`none` preserve the existing all-output matching behavior. Line-echo mode is
intended for single-line cooked consoles; generic binary and non-echoing
targets keep the default.

When `semantic_prompt=osc3008`, the result parser accepts exactly one bounded,
well-formed `type=command` start marker and its matching end marker. It exposes
only the command-output slice, an allowlisted success/failure token, and an
optional numeric status. It never exposes marker identifiers or treats marker
fields as instructions. Missing, malformed, mismatched, or ambiguous markers
fall back to normalized transcript output with no semantic status claim.

`raw_output` and its compatibility alias `output` remain unchanged and
authoritative. `normalized_output` remains opt-in. New additive result fields
surface best-effort `command_output`, its source, semantic status, and optional
exit code.

## Consequences

Existing literal-port calls and profiles without console settings retain the
current byte-for-byte write behavior. Shell-console profiles can make command
submission and echo handling discoverable without adding a second exec-shaped
tool.

Callers must not include a terminator in `command` while also selecting a
non-`none` line ending unless they intentionally want both byte sequences.
Profile authors are responsible for choosing settings appropriate to their
console.

Semantic parsing is an optional interpretation of untrusted transcript
structure, not a replacement for raw evidence. Clients must continue to use
timeouts, inspect write/consume state, and treat all device-derived text as
untrusted.
