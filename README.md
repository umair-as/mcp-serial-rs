# mcp-serial-rs

[![CI](https://github.com/umair-as/mcp-serial-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/umair-as/mcp-serial-rs/actions/workflows/ci.yml)
![Rust MSRV](https://img.shields.io/badge/rust-1.85%2B-orange)
![MCP](https://img.shields.io/badge/MCP-2025--11--25-blue)
![Transport](https://img.shields.io/badge/transport-stdio-lightgrey)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)

An MCP server that exposes a curated set of serial-port operations over stdio.
Built on the [`rmcp`](https://crates.io/crates/rmcp) SDK: it speaks the Model
Context Protocol — `initialize`, `tools/list`, `tools/call` — with one JSON-RPC
object per line on stdin, responses on stdout, logs on stderr. Designed for
agent-driven embedded workflows — opening boards by stable USB serial string,
writing console commands, scraping output until a regex matches, all from a
tokio-async backend with per-session isolation and an allowlist that keeps
stray paths out.

The server is hardware-agnostic: any USB-serial device matching the allowlist
can be exposed as a bounded MCP serial-console session.

## Build

```sh
cargo build --release      # → target/release/mcp-serial-rs
cargo clippy --all-targets -- -D warnings
cargo test
```

MSRV 1.85. Edition 2021. The 1.85 floor is required by `rmcp` 1.7
(which is built with edition 2024). See `CLAUDE.md §1–2` for the
dependency rationale. The protocol layer is the `rmcp` SDK over its
stdio transport; this crate owns the serial domain (sessions, the
device-profile registry, the allowlist, the audit journal) and
registers the `serial.*` tools with the SDK.

## Migration notes

`0.2.0` contains wire-visible changes from the original prototype:
session IDs are now opaque 32-character random hex strings, serial-domain
failures inside `tools/call` are MCP tool errors (`isError=true`) instead of
JSON-RPC protocol errors, output schemas are object-root schemas that include
the structured tool-error shape, duplicate opens for the same configured path
return `PortBusy`, and the default journal is metadata-only. Regenerate strict client types from
`tools/list` and branch on `result.isError` for tool execution failures.

## Quick start

```sh
# Optional: load device profiles so list_ports is enriched.
cp devices.toml.example devices.toml && edit devices.toml

# Drive the server with an MCP handshake, then call a tool.
(
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"demo","version":"0.1.0"}}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"serial.list_ports","arguments":{}}}'
) | ./target/release/mcp-serial-rs
```

Every `serial.*` operation goes through a `tools/call` envelope. Successful
calls return an MCP tool result whose parsed payload lives under
`result.structuredContent` (a text rendering is also present in
`result.content` for clients that do not read structured output).

With four boards attached and `devices.toml` from this repo, the
`serial.list_ports` call above answers with:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "structuredContent": {
      "ports": [
        {"port":"/dev/ttyUSBx","vid":4292,"pid":60000,
         "serial":"AABBCCDD00001111",
         "device":"lab-node","description":"Lab device console (CP2102N)"},
        {"port":"/dev/ttyACM0","vid":6790,"pid":21971,"serial":"EXAMPLE001",
         "device":"rpi5","description":"Raspberry Pi 5 debug UART (WCH CH340)"},
        {"port":"/dev/ttyUSB0","vid":4292,"pid":60000,"serial":"0001",
         "device":"imx93-evk","description":"i.MX93 EVK debug UART (CP2102 clone)"},
        {"port":"/dev/ttyACM1","vid":11914,"pid":12,"serial":"EXAMPLE002",
         "device":"pico2w","description":"RPi Pico 2W via Debug Probe (CMSIS-DAP)"}
      ]
    }
  }
}
```

## Tool reference

All tools are invoked via `tools/call` (`{"name": "<tool>", "arguments":
{...}}`) and advertised by `tools/list` under their dotted names. The `rmcp`
SDK generates input and output schemas from typed Rust structs.

| Tool | Arguments | `structuredContent` |
|---|---|---|
| `serial.list_ports` | — | `{ports: [{port, vid, pid, serial, device, description}]}` |
| `serial.open` | `{port, baud?, timeout_ms?, write_policy?}` **or** `{device, baud?, timeout_ms?, write_policy?}` | `{session_id}` (32-char random hex) |
| `serial.sessions` | — | `{sessions: [{session_id, port, baud, state, default_timeout_ms, write_policy, console_settings}]}` |
| `serial.get_session` | `{session_id}` | `{session}` (includes policy and console settings) |
| `serial.write` | `{session_id, data, confirm?}` (≤ 4 KiB) | `{bytes_written}` |
| `serial.read` | `{session_id, max_bytes?, timeout_ms?}` | `{data, status, timed_out, bytes_read, truncated, session_usable, output_is_untrusted}` |
| `serial.drain` | `{session_id, max_bytes?}` | same shape as `serial.read`, with a short idle deadline |
| `serial.clear_input` | `{session_id, max_bytes?}` | `{status, bytes_read, discarded_bytes, truncated, session_usable}` |
| `serial.read_until` | `{session_id, pattern, timeout_ms?}` | `{data, matched, status, bytes_read, truncated, match_details?, session_usable, output_is_untrusted}` |
| `serial.exec` | `{session_id, command, expect, line_ending?, echo_mode?, semantic_prompt?, timeout_ms?, clear_before_write?, normalize_output?, confirm?}` | raw output plus optional normalized/command output, semantic status, byte counts, match details, and write state |
| `serial.close` | `{session_id}` | `{ok}` |

`serial.open` accepts either a literal `port` path or a `device` profile name
— never both. The profile's `baud` is the default; pass an explicit `baud`
to override. Unknown device names return `DeviceNotFound` (`-32009`) before
any system call. A path already reserved by an Opening, Ready, or Closing
session returns `PortBusy` (`-32010`); the production backend also requests
exclusive OS access as defense in depth.

Each session carries a **write policy** set at `open` (`write_policy`), enforced
before any bytes reach the device:

- `allow` (default) — writes proceed as before.
- `deny` — a read-only session. `serial.write` / `serial.exec` are refused
  server-side with `WriteForbidden` (`-32012`) regardless of the request; reads,
  `drain`, and `clear_input` still work. This is model-proof: no bug and no
  injected device text can cause a write.
- `confirm` — writes require an explicit `confirm: true` on the call, else
  `ConfirmationRequired` (`-32013`). This is a tripwire and audit seam, not a
  hard gate (an automated caller can set `confirm` itself); use `deny` when you
  need a guarantee.

The effective policy is the most restrictive of the caller's `write_policy` and
any `privileged` device-profile default (`allow < confirm < deny`), so a caller
may escalate but never downgrade a privileged profile. Both gate errors carry
`command_written=false` and `session_usable=true`. See `docs/adr/0005`.

Domain validation and serial failures detected inside a tool call — unknown
session, disallowed port, unknown device, malformed regex, oversized write,
supplying both `port` and `device` — return a normal JSON-RPC result whose
MCP `CallToolResult` has `isError: true`. The structured error is under
`result.structuredContent.error` with a stable `type`, numeric `code`,
`data`, `retryable`, `command_written`, `bytes_consumed`, and
`session_usable`. JSON-RPC protocol errors are reserved for invalid MCP
framing, unknown methods/tools, and SDK-level parameter deserialization
failures such as wrong JSON types or unknown fields.

Runtime outcomes are **not** errors: a `serial.read_until` that times out
returns a successful tool result with `matched=false` and the partial buffer;
a `serial.exec` timeout returns `status="timed_out"`, `ok=false`, and partial
raw output.

Deadline exhaustion before a result can be formed is a tool execution error:
waiting for the per-session lock, writing, or flushing can return `isError:
true` with the configured `timeout_ms` and conservative side-effect metadata.

Serial output is preserved as lossy UTF-8 console text, including command echo,
prompts, wrapping, CRLF, ANSI escapes, and OSC shell-integration sequences.
Invalid UTF-8 bytes are represented by replacement characters; this is not a
binary transport. An allowlisted path can open directly onto an interactive
(and possibly privileged) device shell, so treat all device output as untrusted
data, never as instructions. `normalize_output=true` on `serial.exec` adds a
best-effort normalized copy; it never replaces `raw_output`.

`serial.exec` transmits `command` byte-for-byte by default
(`line_ending="none"`). A named profile or an individual call may explicitly
select `lf`, `cr`, or `crlf`; that suffix is part of the same size check and
write-policy-gated write, with no newline exemption. `echo_mode="line"` keeps
the echoed line in `raw_output` but prevents `expect` from matching it; it
requires `line_ending` to be `lf`, `cr`, or `crlf`.
`semantic_prompt="osc3008"` can expose bounded `command_output`,
`semantic_status`, and `exit_code`; missing or ambiguous markers fall back
cleanly without claiming a status. See [ADR 0006](docs/adr/0006-console-execution-profiles.md).

## Device profiles (`devices.toml`)

Profile entries map a stable USB serial string (and optional VID/PID) to a
short device name and human-readable description. They may also declare
console execution defaults (`line_ending`, `echo_mode`, and
`semantic_prompt`). The primary effects are:

1. **Enriched `serial.list_ports`** — matched ports gain `device` and
   `description` fields (see Quick start output above).
2. **Named-device `serial.open`** — `{"device": "esp32c6"}` resolves to the
   first connected port matching that profile, using `profile.baud` by default:

   ```json
   {"jsonrpc":"2.0","id":3,"method":"tools/call",
    "params":{"name":"serial.open","arguments":{"device":"esp32c6"}}}
   ```

A profile may also set `privileged = true` (optional, default false) for ports
that open onto an interactive or privileged shell. Sessions opened via such a
profile default to `write_policy = "confirm"`; a caller can still escalate to
`deny` at `serial.open`, but cannot downgrade below `confirm`.

The file shipped at `devices.toml.example` is a working template — copy to
`devices.toml` (the default location) and edit. Or set `MCP_SERIAL_DEVICES`
to point anywhere else. A missing file is not an error; profiles are
optional and named-device opens simply return `DeviceNotFound`.

**CP2102 clone caveat.** Counterfeit CP2102 chips often report `iSerial=0001`.
If you have more than one, tighten the profile with `match_vid` / `match_pid`
or disambiguate via `/dev/serial/by-path` and hard-code the port path.

## Configuration

| Env var | Purpose | Default |
|---|---|---|
| `MCP_SERIAL_DEVICES` | Path to the device-profile TOML file | `./devices.toml` |
| `MCP_SERIAL_ALLOWLIST` | Comma-separated glob patterns of openable device paths. When set, replaces the compiled-in list entirely. | unset → `/dev/ttyUSB*,/dev/ttyACM*` |
| `MCP_SERIAL_JOURNAL` | Append-only JSONL audit journal path (see below) | `$XDG_STATE_HOME/mcp-serial-rs/audit.jsonl`, else `~/.local/state/mcp-serial-rs/audit.jsonl` |
| `RUST_LOG` | `tracing-subscriber` filter, e.g. `mcp_serial_rs=debug` | `mcp_serial_rs=info` |

The compile-time allowlist is `/dev/ttyUSB*` and `/dev/ttyACM*` — Linux USB
serial bridges. Override with `MCP_SERIAL_ALLOWLIST="/dev/cuaU*,/dev/cu.usb*"`
on BSD/macOS, or to widen the path for the PTY-loopback test suite.

Other compile-time limits live in `src/config.rs`:

| Constant | Value | Effect |
|---|---|---|
| `MAX_SESSIONS` | 4 | Concurrent open ports |
| `MAX_READ_BUFFER` | 64 KiB | `serial.read_until` accumulator cap |
| `MAX_WRITE_CHUNK` | 4 KiB | Writes above this size are rejected with `InvalidParam` (`-32008`) |
| `MAX_TIMEOUT_MS` | 30 000 | All caller-supplied timeouts are clamped |

### Audit journal

The server keeps an always-on, append-only JSONL audit journal at
`MCP_SERIAL_JOURNAL`. It is a **tool-call journal**: every `tools/call`
appends a `call` row and, once a result is produced, a `result` row. Lifecycle
traffic (`initialize`,
`tools/list`, `notifications/initialized`) is **not** journaled — it stays
visible through `tracing` logs on stderr. If the journal path cannot be
opened the server logs a warning and continues in degraded mode (no
journaling). Journal lock, write, and flush operations have short deadlines so
auditing cannot indefinitely block tool dispatch. On Unix, the default
parent directory is created `0700` and the journal file `0600`. Final-component
symlinks and non-regular journal files are rejected on Unix.

The journal stores **metadata-only summaries, not payload bodies**. Call rows
record bounded argument-key lists plus byte counts for large fields such as
`data`, `command`, and `expect`; result rows record status, byte counts,
matching state, error code/type, and write/consume indicators. Command text,
serial output, error messages, and unknown-tool argument bodies are not written by default.
This keeps the audit trail bounded on resource-constrained gateways and avoids
recording credentials typed into a privileged serial shell. To capture full
transcripts, drive the server from a client that records them.

Tail the journal in a readable form with:

```sh
tail -F "${MCP_SERIAL_JOURNAL:-${XDG_STATE_HOME:-$HOME/.local/state}/mcp-serial-rs/audit.jsonl}" \
  | jq -r '"[\(.ts) \(.direction) \(.tool) sid=\(.session_id[0:8])] " +
           (.summary | tostring)'
```

Drop the `jq` filter for raw JSONL access when feeding the journal to other
tools.

## Hardware smoke test

End-to-end against a real serial-attached target. Using the named-device path
keeps the test stable when `/dev/ttyUSB*` numbering shifts across reboots. This
example uses a shell-like target and `serial.exec`, which preserves raw output
while reporting whether the command was written and whether the expected prompt
was observed:

```sh
./target/release/mcp-serial-rs <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke","version":"0.1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"serial.open","arguments":{"device":"rpi5","timeout_ms":2000}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"serial.exec","arguments":{"session_id":"<from-#2>","command":"uname -a","expect":"[#>$] ","timeout_ms":8000,"clear_before_write":true,"normalize_output":true,"confirm":true}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"serial.close","arguments":{"session_id":"<from-#2>"}}}
EOF
```

The `session_id` comes from request #2's `result.structuredContent.session_id`.
The example `rpi5` profile supplies `line_ending="lf"`, `echo_mode="line"`,
and `semantic_prompt="osc3008"`; literal-port callers can pass those fields on
the exec call instead. Expect request 3 to return `status`, `ok`, `raw_output`,
optional `normalized_output` / `command_output` / semantic status,
`bytes_written`, `bytes_read`, `command_written`, `match_details`, and
`session_usable`. Drive
this from any MCP client — `rmcp`
dispatches `tools/call` requests concurrently, so calls on independent
sessions run in parallel; calls on the same session serialise through the
per-port mutex.

## Tests

```sh
cargo test                          # 140 total: 85 lib unit + 3 CLI + 51 MCP wire + 1 loopback
cargo test --test loopback          # PTY round-trip — needs `socat`
```

Integration coverage: `tests/mcp_tests.rs` drives the `rmcp` server in-process
over a `tokio::io::duplex` pipe — initialize, `tools/list`, every tool, the
narrowed journal, and concurrency (independent sessions, serialised same-session
writes, close-vs-read races). `tests/loopback.rs` is the end-to-end check: it
spawns the release binary, stands up a real `socat` PTY pair, and drives
`open → write → read_until → close` through the `tools/call` wire against a
thread that echoes whatever it receives. If `socat` is unavailable the loopback
test skips with an install hint.

## Releases

Releases are created manually after the version change has merged to `main`.
Run the `release` GitHub Actions workflow from the default branch and provide
the version from `Cargo.toml` without the `v` prefix. The workflow reruns the
quality gates, builds the locked release binary, creates the matching
`vX.Y.Z` tag, and publishes a GitHub release containing a Linux x86-64 archive
and SHA-256 checksum. It does not publish the crate to crates.io.

Architecture diagrams live in `docs/mcp-serial-architecture.md`. The `rmcp`
migration that produced the current protocol layer is recorded in
`docs/archive/rmcp-migration-spec.md`.

Agent-consumer guidance lives in `AGENTS.md`. MCP conformance dispositions are
tracked in `docs/mcp-conformance-findings.md`, and significant design decisions
are recorded under `docs/adr/`.

## Status

The `serial.*` MVP and the `rmcp` SDK adoption are complete:

- MCP lifecycle (`initialize`, `tools/list`, `notifications/initialized`) and
  `tools/call` dispatch handled by the `rmcp` SDK over stdio.
- The `serial.*` tool set, with `rmcp`-generated input/output schemas and
  structured tool results.
- Device profiles and named-device open.
- Allowlist + timeout clamping + session cap.
- Tool-call audit journal with degraded-mode fallback.
- In-process rmcp wire tests + PTY-loopback integration test.

Deferred (not implemented):

- `serial.reset_esp32` — DTR/RTS toggle for hard reset / bootloader entry.
- `serial.capture_start` / `serial.capture_stop` — tee session bytes to a log file.
- MCP resources/prompts/progress beyond the current tools-only surface.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE)
at your option.
