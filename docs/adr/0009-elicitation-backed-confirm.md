<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# ADR 0009: `write_policy = "confirm"` asks the person through MRTR elicitation

## Status

Accepted.

## Context

ADR 0005 introduced `write_policy = "confirm"` and was explicit that it is a
tripwire, not a gate: a `confirm: true` argument is written by the same model
whose write it is meant to check, so prompt-injected device text could talk the
model into supplying it. ADR 0005 named "a real human prompt via elicitation"
as the eventual upgrade and `confirm` as the seam for it.

MCP `2026-07-28` makes that upgrade possible without server-held state. A tool
may answer `tools/call` with `resultType: "input_required"` carrying an
`elicitation/create` request; the host shows it to the person and retries the
original call with their answer in `inputResponses`. The answer is assembled by
the host, not by the model, which is what makes it worth asking for.

## Decision

On a `confirm` session, `serial.write` and `serial.exec` now have two shapes,
chosen per request:

1. **Client speaks `2026-07-28` and supports form elicitation.** That is an
   `elicitation` capability with `form`, or an empty `elicitation: {}` (implied
   form support, as in rmcp's `supported_elicitation_modes`). A URL-only client
   could not complete the form and takes path 2. The server returns one form
   elicitation whose message names the tool, session, port, baud, and the
   complete bytes about to be sent (`{:?}`-escaped, never truncated: accepting
   authorizes every byte, and `MAX_WRITE_CHUNK` already bounds the size), with
   a required boolean `confirm`. The model's own
   `confirm: true` argument is ignored on this path: the gate is only worth
   having if the model cannot satisfy it. Only `action: "accept"` with
   `content.confirm == true` proceeds. Decline, cancel, an accept without the
   flag, or a malformed answer returns a new tool error
   `ConfirmationDeclined` (`-32016`, `retryable: false`, `data.elicitation`
   naming the action) so the model learns a person said no and stops.

2. **Any other client** (`2025-11-25` or older, no elicitation capability, or
   URL-only elicitation).
   ADR 0005 behaviour is unchanged: `confirm: true` proceeds, its absence is
   `ConfirmationRequired` (`-32013`). A legacy client that declares elicitation
   still gets this path, because MRTR does not exist on its revision and the
   pre-2026 server-initiated `elicitation/create` is deprecated; rmcp would
   also refuse to send `input_required` to such a peer.

Binding, not state. The elicitation key is `confirm_write:<digest>` where the
digest covers `(session_id, tool, exact payload bytes)`, keyed with per-process
random SipHash keys so two colliding payloads cannot be precomputed offline. On the retry the server
recomputes the key from the arguments it actually received and looks the
answer up under it. A retry whose payload differs from what the person saw
therefore finds no answer and is elicited again; a "yes" cannot be replayed
for different bytes. The digest is a binding rather than a secret: forging it
would also require forging the host-supplied `inputResponses`, which is outside
the model's reach. No `requestState` is used, so there is nothing to seal or
verify.

Ordering. Every cheaper refusal runs before the person is asked, so no one
confirms a write that would have been rejected anyway: size caps, `expect`
validation, the raw-write refusal on guarded sessions, and the server-owned
command policy (ADR 0007) all precede the write gate. For `serial.exec` the
gate runs on the transmitted bytes (command plus any line ending) so the
person sees the final line. `deny` sessions are never elicited. The gate still
runs before port checkout; no lock is held across the round trip, and `exec`
remains atomic once it proceeds (ADR 0002).

Audit. The journal stays metadata-only. The call row gains `mrtr_retry: bool`
(whether `inputResponses` was present); the `input_required` result row is
recorded with `result_type: "input_required"`; a refusal is recorded through
the usual `error_code` / `error_type`. The elicitation message and the answer
are never journaled.

## Consequences

- With an elicitation-capable `2026-07-28` client, `confirm` becomes a gate
  the model cannot pass alone. `deny` remains the only model-proof guarantee
  for hosts that do not implement elicitation.
- Existing callers that pass `confirm: true` keep working on legacy clients
  and are prompted on capable ones. That is the intended behaviour change.
- `-32016` is reserved for `confirmation_declined`; ADR 0007 already reserved
  `-32015` for `raw_write_forbidden`.
- Error enum, tool descriptions, `AGENTS.md`, and `SERVER_INSTRUCTIONS` all
  describe the two shapes; the wire tests cover accept, every non-accept
  answer, payload rebinding, the `write` path, both fallbacks, and `deny`.
- Not done here: completion for session IDs, a URL-mode elicitation, or
  eliciting on legacy clients through the deprecated server-initiated path.
