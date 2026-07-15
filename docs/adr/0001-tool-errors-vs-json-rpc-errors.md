<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# ADR 0001: Return serial-domain failures as MCP tool errors

## Status

Accepted.

## Context

The initial implementation mapped most `SerialError` values to JSON-RPC
`error` responses with project codes in the `-320xx` range. The MCP tools
model distinguishes protocol failures from failures that happen while executing
a dispatched tool. For tool execution failures, returning `CallToolResult` with
`isError: true` keeps the result visible to model-driven clients and gives the
agent structured recovery data.

A live probe showed an allowlisted path can open directly onto a privileged
device shell. In that context, clients must know whether a failed operation
wrote bytes, consumed output, and left the session usable.

## Decision

Serial-domain failures inside `tools/call` return normal JSON-RPC responses
whose result is an MCP tool error:

- `result.isError = true`;
- `structuredContent.error.type`;
- numeric project `code`;
- `data`;
- `retryable`;
- `session_id`;
- `command_written`;
- `bytes_consumed`;
- `session_usable`.

JSON-RPC errors are reserved for MCP framing, dispatch, unknown methods/tools,
and SDK parameter deserialization failures.

## Consequences

Clients that previously branched on top-level JSON-RPC `error.code` for serial
failures must now inspect `result.structuredContent.error.code`. The behavior is
more MCP-native and better suited to recovery-oriented agent workflows.

Project-specific numeric codes are retained inside the structured tool error
for compatibility.
