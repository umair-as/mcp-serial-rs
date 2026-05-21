# rmcp Migration Specification

## Purpose

Migrate `mcp-serial-rs` from the hand-rolled JSON-RPC/MCP protocol layer to
the `rmcp` SDK while preserving the existing serial tool behavior.

This is a protocol-layer migration, not a product redesign. The serial domain,
device profile behavior, safety limits, journal behavior, and user-facing tool
contract must remain stable unless a compatibility-breaking change is reviewed
and approved separately.

## Goals

- Replace custom MCP protocol plumbing with `rmcp`.
- Preserve the current public `serial.*` tool contract.
- Use native MCP structured tool results where possible.
- Keep serial I/O safety boundaries explicit and tested.
- Make concurrency behavior safe under `rmcp` request dispatch.
- Keep logging and auditing behavior predictable for local hardware workflows.

## Non-Goals

- Do not rename tools from dotted names to underscore names.
- Do not add new payload formats such as `hex:` writes.
- Do not make `serial.exec` append newlines automatically.
- Do not change session ID format unless separately approved.
- Do not implement `serial.reset_esp32` as part of the migration.
- Do not implement capture start/stop as part of the migration.
- Do not silently drop audit journal behavior.
- Do not use `anyhow` unless project conventions are changed first.

## Current Tool Contract

The migrated server must expose these tools with these names:

| Tool | Input | Output |
|---|---|---|
| `serial.list_ports` | no required params | list of ports with `port`, `vid`, `pid`, `serial`, `device`, `description` |
| `serial.open` | either `port` or `device`, optional `baud`, optional `timeout_ms` | `session_id` |
| `serial.write` | `session_id`, `data` | `bytes_written` |
| `serial.read` | `session_id`, optional `max_bytes`, optional `timeout_ms` | `data` |
| `serial.read_until` | `session_id`, `pattern`, optional `timeout_ms` | `data`, `matched` |
| `serial.exec` | `session_id`, `command`, `expect`, optional `timeout_ms` | `output`, `ok` |
| `serial.close` | `session_id` | `ok` |

Compatibility requirements:

- `serial.open` must continue to support named devices loaded from
  `devices.toml`.
- `serial.open` must reject requests that include both `port` and `device`.
- `serial.list_ports` must continue to enrich ports with matching device
  profile metadata.
- `serial.write` must treat `data` as UTF-8 text and enforce
  `MAX_WRITE_CHUNK`.
- `serial.read` and `serial.read_until` must return UTF-8 lossy strings, not
  binary blobs.
- `serial.read` must clamp requested `max_bytes` to the configured read buffer
  ceiling.
- `serial.read_until` must return partial data with `matched=false` on timeout
  or EOF.
- `serial.exec` must write `command` verbatim.
- `serial.exec` must validate `expect` before writing to the device.
- `serial.exec` must return partial output with `ok=false` when the expected
  pattern is not seen before timeout.

## Environment Contract

The migration must preserve these environment variables:

| Variable | Requirement |
|---|---|
| `MCP_SERIAL_ALLOWLIST` | Overrides the compiled-in serial port allowlist. |
| `MCP_SERIAL_DEVICES` | Selects the device-profile TOML file. Missing file remains non-fatal. |
| `MCP_SERIAL_JOURNAL` | Selects the append-only JSONL journal path. Unwritable path remains non-fatal degraded mode. |
| `RUST_LOG` | Controls tracing verbosity. Logs must continue to go to stderr. |

The default journal path is `/tmp/mcp-serial-journal.jsonl`. The default path
and degraded-mode behavior must remain documented and tested.

## rmcp Facts To Design Around

These facts were verified against `rmcp 1.7.0`:

- Dotted tool names are valid.
- `rmcp` can generate input schemas from typed parameters.
- `rmcp` supports structured tool results through `structuredContent`.
- Tool results may also include text fallback content for compatibility.
- `rmcp` stdio transport uses stdin/stdout and does not configure logging.
- `rmcp` request handling is concurrent by default.
- `rmcp 1.7.0` uses edition 2024 internally. This crate may remain edition
  2021, but the compiler must support dependencies that use edition 2024.

## Dependency And MSRV Decision

Adopt the current `rmcp` line and raise the project MSRV:

- `rmcp = "1.7"` with the stdio transport feature enabled.
- `schemars = "1.0"` to match the schema generation stack used by `rmcp`.
- Keep this crate on Rust edition 2021 unless a separate edition migration is
  justified.
- Raise `rust-version` to at least `1.85`, the conservative floor for building
  dependencies that use edition 2024.

This must be reflected in `Cargo.toml`, `CLAUDE.md`, and the README. Local
development currently uses a newer stable compiler, but the spec should define
a maintainable minimum rather than copying the local toolchain version.

## Architecture Constraints

The serial domain must remain separated from MCP protocol plumbing:

- MCP transport and protocol concerns belong in the server layer.
- Port allowlist, device profiles, limits, and defaults belong in config/domain
  modules.
- Session lifecycle and serial I/O remain owned by the serial manager/session
  modules.
- Regex/pattern matching remains owned by the parser module.
- Journal writing remains owned by the journal module or a clearly named
  server/journal adapter.

Locking and concurrency constraints:

- Do not hold a manager-wide lock across serial I/O.
- Operations on different sessions must be able to progress independently.
- Operations on the same session must serialize access to the underlying port.
- Close racing with in-flight I/O must have deterministic, tested behavior.
- Journal writes from concurrent requests must remain valid JSONL and must not
  interleave partial lines.
- Journal writing must serialize each JSONL line with a single async critical
  section, such as the current `tokio::sync::Mutex` around the buffered writer.

Future capture-design constraint:

- If future capture support introduces a background read loop, direct read tools
  must not compete with that loop for the same port stream.
- Any future capture implementation must use a single reader per session and
  fan out data to subscribers such as direct read tools and capture sinks.
- Do not introduce competing direct reads from the same serial port.

## Structured Result Requirements

The rmcp migration should use structured MCP tool results for object outputs.

Requirements:

- Tool clients must be able to read parsed result objects without reparsing a
  text blob.
- Text fallback content may be present for compatibility.
- Output schemas should be phased in after behavioral parity is established.
- Result field names must match the current public contract.
- Output schemas are not a behavioral-parity gate. Add and test them only for
  tools where they are explicitly enabled in the migration pass.

The migration should not turn existing object results into JSON strings inside
text-only content unless a client compatibility issue forces that choice.

## Error Semantics

Preserve the distinction between protocol/setup failures and tool-domain
outcomes.

Protocol/setup failures include:

- invalid tool arguments,
- unknown session IDs,
- unknown device profile names,
- disallowed port paths,
- malformed regex patterns,
- unsupported protocol or framework-level failures.

Tool-domain outcomes include:

- read timeout with partial data,
- `serial.exec` timeout with partial output,
- future capture/reset states that the model can react to.

Guidance:

- Keep current behavior where compatibility requires protocol errors.
- Use protocol errors for validation and setup failures.
- Use structured tool results for outcomes that are useful for model recovery,
  especially partial-output cases.
- Include enough structured context for clients to branch without parsing human
  messages.
- Preserve the current partial-output result shape for the parity pass:
  `serial.read_until` returns `data` with `matched=false`, and `serial.exec`
  returns `output` with `ok=false`. The string field itself is the partial data.

Error-code compatibility:

- Preserve the current server-defined error-code mapping unless a separate
  compatibility decision changes it.
- `PortNotAllowed`: `-32001`.
- `PortNotFound`: `-32002`.
- `SessionNotFound`: `-32003`.
- `InvalidState`: `-32004`.
- `Timeout`: `-32005`.
- `Io`: `-32006`.
- `MaxSessionsReached`: `-32007`.
- `InvalidParam`: `-32008`.
- `DeviceNotFound`: `-32009`.
- Preserve structured error data fields so clients do not need to parse human
  messages.

Validation ordering:

- `serial.exec` must validate `expect` before writing to the device.
- Session lookup/state validation may happen before or after regex validation,
  but no write may occur unless both the session is valid/ready and `expect` is
  valid.
- Unknown sessions and invalid states remain protocol/setup failures.

Extra-field handling:

- Tool input schemas should reject unknown parameters with
  `additionalProperties=false`.
- Runtime handlers should not depend on unknown fields for behavior.
- `serial.list_ports` has no meaningful parameters; extra fields should be
  rejected by schema validation where rmcp applies schemas.

## Journal Requirements

Current journal behavior is dispatch-level:

- call row before routing,
- result row after routing when a result exists,
- lifecycle methods are journaled,
- notifications are journaled as call-only,
- sessionless calls use the no-session sentinel,
- large data fields are summarized and truncated,
- journal I/O failures warn and do not fail tool execution.

The rmcp migration will intentionally narrow the JSONL audit journal to tool
calls only:

- Journal `tools/call` invocations and their results.
- Keep summaries for large data fields.
- Keep degraded mode when the journal cannot open.
- Keep journal writes line-delimited and non-interleaving under concurrent tool
  calls.
- Do not journal lifecycle traffic such as `initialize`, `tools/list`, or
  `notifications/initialized`.

This is an intentional compatibility change. Documentation should describe the
result as a tool-call or serial-operation journal, not full MCP traffic
journaling. Framework and lifecycle events should remain visible through
tracing logs on stderr.

## Tool Annotations

Tool annotations should be used as hints where helpful, but they must not
replace enforcement.

Suggested intent:

- `serial.list_ports` is read-only.
- `serial.read` and `serial.read_until` are read-only from the host API
  perspective but can consume serial stream data.
- `serial.write` and `serial.exec` modify external device state.
- `serial.close` modifies server session state.

Annotations are advisory only. Safety must remain enforced by validation,
allowlists, size limits, and session state.

## Testing Requirements

The migration is complete only when these behaviors are covered.

Protocol and tool listing:

- MCP initialize succeeds through rmcp.
- Tool list contains all existing dotted tool names.
- Tool schemas include the expected required and optional inputs.
- Structured output is present for object results.
- Output schema presence is tested only for structured tools where output
  schemas are explicitly enabled.

Serial behavior:

- `serial.open` by `port` works.
- `serial.open` by `device` works.
- `serial.open` rejects both `port` and `device`.
- `serial.open` rejects requests with neither `port` nor `device`.
- `serial.write` rejects oversized data.
- `serial.read` clamps `max_bytes`.
- `serial.read_until` matches across chunks.
- `serial.read_until` returns partial data on timeout.
- `serial.exec` writes command verbatim.
- `serial.exec` rejects invalid or empty `expect` before writing.
- `serial.exec` returns `{output, ok:false}` on timeout.
- `serial.close` removes the session and releases the port.

Concurrency:

- A slow read on one session does not block a write/read on another session.
- Concurrent writes on one session serialize without interleaving.
- Close racing with an in-flight operation has defined behavior.
- Concurrent journal writes remain valid line-delimited JSON.

Integration:

- PTY loopback still passes through the rmcp `tools/call` wire shape.
- Logs still go to stderr only.
- stdout remains reserved for MCP messages only.
- Journal degraded mode still allows the server to start.

Documentation:

- README reflects rmcp wire usage.
- CLAUDE.md no longer says no MCP SDK crate if rmcp is adopted.
- MSRV and dependency rationale are documented.
- Any journal compatibility change is explicit.

## Migration Sequence

Recommended order:

1. Update the documented dependency/MSRV policy.
2. Add dependencies without deleting the current implementation.
3. Build a parallel rmcp server layer for one read-only tool.
4. Add rmcp wire tests for initialize, tools/list, and one tool call.
5. Port structured result types and schemas.
6. Port `serial.list_ports`.
7. Port `serial.open` with device-profile support.
8. Port `serial.write`, `serial.read`, and `serial.read_until`.
9. Port `serial.exec` with pre-write validation.
10. Port `serial.close`.
11. Implement tool-call journal behavior and update docs/tests for the narrowed
    scope.
12. Add concurrency tests.
13. Port loopback integration test to rmcp wire shape.
14. Remove the old protocol/dispatch layer only after parity tests pass.
15. Update documentation and task tracking.

## Open Decisions

- The exact CI matrix and downstream build environments used to enforce the
  documented Rust 1.85 MSRV.
- Which output schemas are worth adding in the first migration pass after
  behavioral parity is complete.
- The concrete subscriber/buffer policy for the future single-reader capture
  architecture.

## Acceptance Criteria

The rmcp migration is acceptable when:

- all current tool behavior is preserved,
- all compatibility guardrails are covered by tests,
- rmcp structured results are available to clients,
- concurrency behavior is tested and safe,
- journal behavior is narrowed to tool calls and documented as such,
- stdout/stderr separation remains intact,
- docs match the implemented behavior,
- old protocol code is removed only after replacement tests pass.
