<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# MCP Conformance Findings

Date: 2026-07-15

Scope: review of `mcp-serial-rs` against the MCP `2025-11-25` tools model,
the exact local `rmcp = 1.7` API, repository tests/docs, and a live probe of
the installed MCP server against `/dev/ttyACM0`.

## Current MCP Surface

The server intentionally implements a narrow tools-only MCP surface over stdio:

- lifecycle negotiation delegated to `rmcp`;
- `notifications/initialized`;
- `tools/list`;
- `tools/call`;
- generated input schemas;
- generated output schemas;
- structured tool results with text fallback content;
- static tool list with no list-change notification.

This is MCP-compliant as a tools-only server. Resources, prompts, progress,
completion, elicitation, tasks, and logging notifications are optional protocol
features and are tracked below as deferred enhancements, not compliance
failures.

## Live Probe Evidence

A live probe against an allowlisted `/dev/ttyACM*` path opened directly onto an
interactive, privileged device shell. Output included command echo, line
wrapping, CRLF, shell prompts, and OSC shell-integration escape sequences.

Durable conclusions from that probe:

- serial output must be treated as untrusted device data;
- raw terminal output must be preserved unless normalization is explicitly
  requested;
- `serial.exec` must be atomic per session to avoid response attribution errors
  on a privileged shell;
- deterministic buffer controls are necessary before command execution.

## Findings and Disposition

### 1. Tool-domain failures should be MCP tool errors

Disposition: implemented.

Serial-domain failures inside `tools/call` now return a successful JSON-RPC
response containing an MCP `CallToolResult` with `isError: true`. The stable
structured payload is under `structuredContent.error` and includes:

- `type`;
- numeric project code;
- human-readable message;
- structured `data`;
- `retryable`;
- `session_id`;
- `command_written`;
- `bytes_consumed`;
- `session_usable`.

JSON-RPC protocol errors remain reserved for MCP framing, dispatch, unknown
method/tool, and SDK deserialization failures.

Durable artifacts:

- `src/mcp/mod.rs`
- `src/errors.rs`
- `tests/mcp_tests.rs`
- `README.md`
- `CLAUDE.md`
- `docs/adr/0001-tool-errors-vs-json-rpc-errors.md`

### 2. Advertise output schemas

Disposition: implemented.

Every current tool advertises an MCP object-root `outputSchema` through `rmcp`
1.7-compatible schema generation. Schemas include the structured tool-error
shape so `isError=true` structured content also conforms to the advertised
contract. Success and error branches retain their own required fields and
referenced definitions. Wire tests assert the root object shape and error-ref
resolution.

Durable artifacts:

- `src/mcp/mod.rs`
- `tests/mcp_tests.rs`
- `README.md`

### 3. Tool annotations and side-effect descriptions

Disposition: implemented for current tools.

Tools now include titles and MCP annotations for read-only, destructive,
idempotent, and open-world hints. Descriptions explicitly state consumption,
mutation, privileged-shell risk, and untrusted output where relevant.

Durable artifacts:

- `src/mcp/mod.rs`
- `tests/mcp_tests.rs`
- `README.md`
- `SECURITY.md`

### 4. Cancellation propagation

Disposition: partially implemented.

`read`, `read_until`, and `exec` accept the `rmcp` request cancellation token and
select on it in serial read loops. `serial.write` also propagates request
cancellation through lock acquisition, write, and flush. Structured results
report cancellation state and whether a command was written. `serial.close` now
marks the session Closing and waits for the per-session port lock before
returning; if it cannot acquire the lock before the deadline, the session is
restored to Ready. Lock wait, write, flush, and exec read phases are now bounded
by the operation deadline or cancellation token where available. A richer
partial-write state model remains deferred.

Durable artifacts:

- `src/serial/manager.rs`
- `src/mcp/mod.rs`
- `tests/mcp_tests.rs`

### 5. Static tool list

Disposition: verified.

The server has a static tool list and does not advertise tool-list change
support. This is correct. Dynamic list-change notifications are rejected for the
current tool surface.

Durable artifacts:

- `src/mcp/mod.rs`
- `docs/adr/0004-defer-optional-mcp-features.md`

### 6. Resources

Disposition: deferred.

Resources map naturally to server policy, device profiles, session state, and
bounded capture artifacts. They are not required for compliance and are deferred
until the existing tool surface has stronger identity, capture, and policy
semantics.

Durable artifacts:

- `docs/adr/0004-defer-optional-mcp-features.md`

### 7. Prompts

Disposition: deferred.

Prompts would be useful for boot capture, shell diagnostics, and login/prompt
workflows. They remain optional and client support varies. Consumer workflow
guidance is currently documented in `AGENTS.md`.

Durable artifacts:

- `AGENTS.md`
- `docs/adr/0004-defer-optional-mcp-features.md`

### 8. Progress notifications and tasks

Disposition: deferred.

Progress and experimental tasks may fit future long-running capture or reboot
waits. Current tools remain bounded synchronous operations.

Durable artifacts:

- `docs/adr/0004-defer-optional-mcp-features.md`

### 9. Elicitation and completion

Disposition: deferred.

Elicitation could help ambiguous device selection or destructive-action
confirmation; completion could help device/profile/session IDs. Both depend on
client support and are less urgent than explicit structured errors and policy.

Durable artifacts:

- `docs/adr/0004-defer-optional-mcp-features.md`

### 10. Sampling and roots

Disposition: rejected for this server.

Sampling would blur the trust boundary by letting the serial server ask a model
to interpret device output. The MCP client already owns reasoning. Roots are
irrelevant because this server does not need client filesystem access.

Durable artifacts:

- `docs/adr/0004-defer-optional-mcp-features.md`

## Remaining Verification Gaps

- Add golden tests for exact tool descriptions and annotations.
- Add schema validation of successful `structuredContent` against every
  advertised output schema, beyond presence checks.
- Add wire tests for invalid MCP framing and unknown methods/tools if not fully
  covered by `rmcp`.
- Add cancellation wire tests that send MCP cancellation notifications from a
  client and verify serial-loop termination timing.
