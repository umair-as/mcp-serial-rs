<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# ADR 0004: Keep the current server tools-only and defer optional MCP features

## Status

Accepted.

## Context

MCP resources, prompts, progress, completion, elicitation, tasks, logging
notifications, roots, and sampling can all be useful in some servers. The
current project is intentionally a narrow local serial-console capability.

The review found that missing optional MCP features is not a conformance
problem. Incorrect behavior inside advertised tools was the higher priority.

## Decision

Keep this tranche focused on tools:

- correct tool-error semantics;
- output schemas;
- tool annotations;
- cancellation;
- session inspection;
- deterministic buffer controls.

Defer resources, prompts, progress, completion, elicitation, and tasks until the
server has stronger identity, capture, and policy models.

Reject sampling for this server. Interpretation belongs to the MCP client or
host agent, not to the serial server. Reject roots unless a future feature
requires client filesystem awareness.

Do not introduce board-specific primary APIs such as `serial.reset_esp32`.
Future reset support should be generic control-line primitives and
profile-defined sequences.

## Consequences

The server remains simple and deterministic. Users who need HIL orchestration,
power control, firmware flashing, or artifact management should compose this
server with a separate bench-control system rather than expanding
`mcp-serial-rs` into a full orchestrator.
