# mcp-serial-rs

An MCP server that exposes a curated set of serial-port operations over stdio.
Built on the [`rmcp`](https://crates.io/crates/rmcp) SDK: it speaks the Model
Context Protocol — `initialize`, `tools/list`, `tools/call` — with one JSON-RPC
object per line on stdin, responses on stdout, logs on stderr. Designed for
agent-driven embedded workflows — opening boards by stable USB serial string,
writing console commands, scraping output until a regex matches, all from a
tokio-async backend with per-session isolation and an allowlist that keeps
stray paths out.

An example downstream target is an ESP32-C6 Zephyr DFU board, but the
server itself is hardware-agnostic — any USB-serial device matching the
allowlist works.

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
         "device":"esp32c6","description":"ESP32-C6 Zephyr DFU target (CP2102N)"},
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

All seven tools are invoked via `tools/call` (`{"name": "<tool>", "arguments":
{...}}`) and advertised by `tools/list` under their dotted names. The `rmcp`
SDK generates each tool's input schema (`additionalProperties: false` — unknown
arguments are rejected) from the typed parameter struct.

| Tool | Arguments | `structuredContent` |
|---|---|---|
| `serial.list_ports` | — | `{ports: [{port, vid, pid, serial, device, description}]}` (latter two `null` when no profile matches) |
| `serial.open` | `{port, baud?, timeout_ms?}` **or** `{device, baud?, timeout_ms?}` | `{session_id}` (16-char hex) |
| `serial.write` | `{session_id, data}` (≤ 4 KiB) | `{bytes_written}` |
| `serial.read` | `{session_id, max_bytes?, timeout_ms?}` | `{data}` (UTF-8 lossy) |
| `serial.read_until` | `{session_id, pattern, timeout_ms?}` | `{data, matched}` (`matched=false` on timeout or EOF) |
| `serial.exec` | `{session_id, command, expect, timeout_ms?}` | `{output, ok}` (compound write + read_until) |
| `serial.close` | `{session_id}` | `{ok}` |

`serial.open` accepts either a literal `port` path or a `device` profile name
— never both. The profile's `baud` is the default; pass an explicit `baud`
to override. Unknown device names return `DeviceNotFound` (`-32009`) before
any system call.

Domain validation failures detected by the tool handlers — unknown session,
disallowed port, unknown device, malformed regex, oversized write, supplying
both `port` and `device` — surface as JSON-RPC errors with project-defined
codes in the server range (`-32001` … `-32009`) plus a structured `error.data`
payload, so clients can branch without parsing messages. Argument failures
caught earlier, by the SDK's input-schema / parameter deserialization (wrong
JSON types, unknown fields), surface as standard `rmcp`/JSON-RPC errors —
typically `-32602` — and do not necessarily carry a project `error.data`.

Runtime outcomes are **not** errors: a `serial.read_until` that times out
returns a successful tool result with `matched=false` and the partial buffer;
a `serial.exec` timeout returns `ok=false` with partial `output`.

## Device profiles (`devices.toml`)

Profile entries map a stable USB serial string (and optional VID/PID) to a
short device name and human-readable description. Two effects:

1. **Enriched `serial.list_ports`** — matched ports gain `device` and
   `description` fields (see Quick start output above).
2. **Named-device `serial.open`** — `{"device": "esp32c6"}` resolves to the
   first connected port matching that profile, using `profile.baud` by default:

   ```json
   {"jsonrpc":"2.0","id":3,"method":"tools/call",
    "params":{"name":"serial.open","arguments":{"device":"esp32c6"}}}
   ```

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
| `MCP_SERIAL_JOURNAL` | Append-only JSONL audit journal path (see below) | `/tmp/mcp-serial-journal.jsonl` |
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
appends a `call` row and, once a result is produced, a `result` row. Large
`data` fields are summarised and truncated. Lifecycle traffic (`initialize`,
`tools/list`, `notifications/initialized`) is **not** journaled — it stays
visible through `tracing` logs on stderr. If the journal path cannot be
opened the server logs a warning and continues in degraded mode (no
journaling); it never blocks startup or tool dispatch.

## Hardware smoke test (ESP32-C6 Zephyr)

End-to-end against a real ESP32-C6 board running a Zephyr crypto KAT
firmware. Using the named-device path so the test survives `/dev/ttyUSB*`
numbering shifts across reboots:

```sh
./target/release/mcp-serial-rs <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke","version":"0.1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"serial.open","arguments":{"device":"esp32c6","timeout_ms":2000}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"serial.write","arguments":{"session_id":"<from-#2>","data":"crypto kat\n"}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"serial.read_until","arguments":{"session_id":"<from-#2>","pattern":"ALL KATS PASSED","timeout_ms":8000}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"serial.close","arguments":{"session_id":"<from-#2>"}}}
EOF
```

The `session_id` comes from request #2's `result.structuredContent.session_id`.
Expect `result.structuredContent.matched: true` on request 4 with the full
console transcript in `data`. Drive this from any MCP client — `rmcp`
dispatches `tools/call` requests concurrently, so calls on independent
sessions run in parallel; calls on the same session serialise through the
per-port mutex.

## Tests

```sh
cargo test                          # 113 total: 73 unit + 39 integration + 1 loopback
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

Architecture diagrams live in `docs/mcp-serial-architecture.md`. The `rmcp`
migration that produced the current protocol layer is recorded in
`docs/archive/rmcp-migration-spec.md`.

## Status

The `serial.*` MVP and the `rmcp` SDK adoption are complete:

- MCP lifecycle (`initialize`, `tools/list`, `notifications/initialized`) and
  `tools/call` dispatch handled by the `rmcp` SDK over stdio.
- All seven `serial.*` tools, with `rmcp`-generated input schemas and
  structured tool results.
- Device profiles and named-device open.
- Allowlist + timeout clamping + session cap.
- Tool-call audit journal with degraded-mode fallback.
- In-process rmcp wire tests + PTY-loopback integration test.

Deferred (not implemented):

- `serial.reset_esp32` — DTR/RTS toggle for hard reset / bootloader entry.
- `serial.capture_start` / `serial.capture_stop` — tee session bytes to a log file.
- Per-tool output schemas (input schemas are generated; output schemas are not).

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE)
at your option.
