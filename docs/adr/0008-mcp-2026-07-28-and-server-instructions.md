<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# ADR 0008: Adopt MCP 2026-07-28 via rmcp 3.x; deliver guidance as server instructions

## Status

Accepted.

## Context

The server was built against MCP `2025-11-25` on `rmcp` 1.x. The `2026-07-28`
revision is the largest protocol change since launch:

- the `initialize` / `notifications/initialized` handshake is removed; each
  request carries its protocol version and client capabilities in `_meta`
  (SEP-2575), and an optional `server/discover` probe replaces capability
  negotiation;
- server-initiated `elicitation/create`, `sampling/createMessage`, and
  `roots/list` are replaced by Multi Round-Trip Requests (MRTR): a tool may
  answer `resultType: "input_required"` and the client retries with
  `inputResponses`;
- `tools/list` results carry `ttlMs` / `cacheScope` and must be deterministically
  ordered;
- Tasks moved into the `io.modelcontextprotocol/tasks` extension; Roots,
  Sampling, and Logging are deprecated with a twelve-month offramp.

`rmcp` 3.x implements the revision and keeps the legacy `initialize` lifecycle
for older clients. Staying on `rmcp` 1.x would leave the server speaking only
a superseded revision and unable to use MRTR for the one feature this server
has always wanted a human in the loop for: `write_policy = "confirm"`.

Separately, `AGENTS.md` carries the safe-workflow and trust-boundary rules an
agent should follow, but a host that only installs the binary never sees that
file. MCP has carried an `instructions` field on the server's advertised
configuration since `2025-03-26`; the server did not use it.

## Decision

1. **Upgrade to `rmcp` 3.x.** MSRV rises from 1.85 to 1.88 (rmcp's floor).
   The server advertises every revision `rmcp` knows; a `2025-11-25` client
   still receives a classic `initialize` reply negotiated to its own version,
   and a `2026-07-28` client may send `tools/list` or `tools/call` as its first
   message with inline `_meta`. Both paths are covered by wire tests.

2. **Serve AGENTS.md guidance as `instructions`.** `McpServer::get_info`
   sets `instructions` to `mcp::SERVER_INSTRUCTIONS`, a condensed copy of
   `AGENTS.md` (safe workflow, trust boundary, output handling, scope limits).
   `rmcp` places the same string in the `initialize` result and the
   `server/discover` result, so every client revision receives it. The text is
   scoped to this server's tools; it must not try to steer the host beyond the
   serial surface. `AGENTS.md` and `SERVER_INSTRUCTIONS` are kept in sync by
   hand; the wire test asserts the constant is what reaches the client.

3. **Mark `tools/list` cacheable.** The tool list is static for the process
   lifetime and `listChanged` is not advertised, so a `2026-07-28` client gets
   `ttlMs = 3_600_000` and `cacheScope = "public"`. The hints are version-gated
   and never appear for legacy clients. `rmcp`'s router sorts tools by name,
   which satisfies the deterministic-order requirement.

4. **MRTR and Tasks are not produced yet.** `call_tool` returns
   `CallToolResponse::Complete` for every tool. The journal records
   `result_type` for any other variant so a future MRTR path cannot bypass the
   audit row. Turning `write_policy = "confirm"` into an elicitation-backed
   MRTR gate (so a person, not the model, supplies the confirmation) is the
   intended next step and will get its own ADR; it must keep the current
   `confirm: true` behaviour as the fallback for clients without elicitation.

5. **Roots, Sampling, Logging stay out.** ADR 0004 already rejected sampling
   and roots and deferred logging; the revision's deprecation of all three
   turns "deferred" into "correct".

## Consequences

- Clients on either revision work; the quick-start examples remain valid.
- MSRV 1.88. CI runs `stable`, so no workflow change is needed.
- `docs/mcp-conformance-findings.md` is re-baselined to `2026-07-28`.
- `ServerInfo` (deprecated alias) is replaced by `ServerConfig`; the
  `call_tool` override returns `CallToolResponse`.
- `SERVER_INSTRUCTIONS` is a second copy of `AGENTS.md` content. Edits to one
  must be mirrored in the other; the constant's doc comment says so.
