# mcp-serial-rs

A minimal MCP-style JSON-RPC server that exposes a curated set of serial-port
operations over stdio. One JSON-RPC object per line on stdin; responses on
stdout; logs on stderr. Designed for agent-driven embedded workflows — opening
boards by stable USB serial string, writing console commands, scraping output
until a regex matches, all from a tokio-async backend with per-session
isolation and an allowlist that keeps stray paths out.

First consumer: the ESP32-C6 Zephyr DFU target in the surrounding repo. The
server itself is hardware-agnostic — anything matching the allowlist works.

## Build

```sh
cargo build --release      # → target/release/mcp-serial-rs
cargo clippy --all-targets -- -D warnings
cargo test
```

MSRV 1.75. Edition 2021. No new runtime deps beyond what `Cargo.toml`
declares; see `CLAUDE.md §2`.

## Quick start

```sh
# Optional: load device profiles so list_ports is enriched.
cp devices.toml.example devices.toml && edit devices.toml

# Drive the server with a JSON-RPC handshake.
(
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  echo '{"jsonrpc":"2.0","id":2,"method":"serial.list_ports","params":{}}'
) | ./target/release/mcp-serial-rs
```

With four boards attached and `devices.toml` from this repo, `serial.list_ports`
returns:

```json
[
  {"port":"/dev/ttyUSB1","vid":4292,"pid":60000,
   "serial":"485732f0d027ee119e0c14d8f49e3369",
   "device":"esp32c6","description":"ESP32-C6 Zephyr DFU target (CP2102N)"},
  {"port":"/dev/ttyACM0","vid":6790,"pid":21971,"serial":"575C016219",
   "device":"rpi5","description":"Raspberry Pi 5 debug UART (WCH CH340)"},
  {"port":"/dev/ttyUSB0","vid":4292,"pid":60000,"serial":"0001",
   "device":"imx93-evk","description":"i.MX93 EVK debug UART (CP2102 clone)"},
  {"port":"/dev/ttyACM1","vid":11914,"pid":12,"serial":"E6647C740341AA30",
   "device":"pico2w","description":"RPi Pico 2W via Debug Probe (CMSIS-DAP)"}
]
```

## Tool reference

| Method | Params | Returns |
|---|---|---|
| `initialize` | — | `{name, version, capabilities}` |
| `tools/list` | — | array of `{name, description, inputSchema}` |
| `serial.list_ports` | — | `[{port, vid, pid, serial, device, description}]` (latter two `null` when no profile matches) |
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

## Device profiles (`devices.toml`)

Profile entries map a stable USB serial string (and optional VID/PID) to a
short device name and human-readable description. Two effects:

1. **Enriched `serial.list_ports`** — matched ports gain `device` and
   `description` fields (see Quick start output above).
2. **Named-device `serial.open`** — `{"device": "esp32c6"}` resolves to the
   first connected port matching that profile, using `profile.baud` by default:

   ```json
   {"jsonrpc":"2.0","id":1,"method":"serial.open",
    "params":{"device":"esp32c6"}}
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
| `RUST_LOG` | `tracing-subscriber` filter, e.g. `mcp_serial_rs=debug` | `mcp_serial_rs=info` |

The compile-time allowlist is `/dev/ttyUSB*` and `/dev/ttyACM*` — Linux USB
serial bridges. Override with `MCP_SERIAL_ALLOWLIST="/dev/cuaU*,/dev/cu.usb*"`
on BSD/macOS, or to widen the path for the PTY-loopback test suite.

Other compile-time limits live in `src/config.rs`:

| Constant | Value | Effect |
|---|---|---|
| `MAX_SESSIONS` | 4 | Concurrent open ports |
| `MAX_READ_BUFFER` | 64 KiB | `serial.read_until` accumulator cap |
| `MAX_WRITE_CHUNK` | 4 KiB | Rejected with `INVALID_PARAMS` above this size |
| `MAX_TIMEOUT_MS` | 30 000 | All caller-supplied timeouts are clamped |

## Hardware smoke test (ESP32-C6 Zephyr)

End-to-end against a real ESP32-C6 board running the Zephyr crypto KAT
firmware from the surrounding repo. Using the named-device path so the test
survives `/dev/ttyUSB*` numbering shifts across reboots:

```sh
./target/release/mcp-serial-rs <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"serial.open","params":{"device":"esp32c6","timeout_ms":2000}}
{"jsonrpc":"2.0","id":3,"method":"serial.write","params":{"session_id":"<from-#2>","data":"crypto kat\n"}}
{"jsonrpc":"2.0","id":4,"method":"serial.read_until","params":{"session_id":"<from-#2>","pattern":"ALL KATS PASSED","timeout_ms":8000}}
{"jsonrpc":"2.0","id":5,"method":"serial.close","params":{"session_id":"<from-#2>"}}
EOF
```

Expect `matched: true` on request 4 with the full console transcript in
`data`. Drive this from any MCP client that speaks JSON-RPC over stdio —
the stdio loop is synchronous-per-line but every `serial.*` handler is
async and concurrent across sessions.

## Tests

```sh
cargo test                          # 86 unit + 8 integration
cargo test --test loopback          # PTY round-trip — needs `socat`
```

The loopback suite (`tests/loopback.rs`) spins up a real `socat` PTY pair,
spawns the release binary with `MCP_SERIAL_ALLOWLIST=/tmp/...`, and drives
`open → write → read_until → close` against a thread that echoes whatever
it receives. If `socat` is unavailable, the test skips with an install hint.

Architecture diagrams live in `docs/mcp-serial-architecture.md`.

## Status

Phase 1 (this MVP) is complete:

- `initialize`, `tools/list`, `notifications/initialized` lifecycle.
- All six `serial.*` MVP tools, plus `serial.exec` (P2-1).
- Device profiles and named-device open.
- Allowlist + timeout clamping + session cap.
- PTY-loopback integration test.

Phase 2 (remaining, see `TASKS.md`):

- `serial.reset_esp32` — DTR/RTS toggle for hard reset / bootloader entry.
- `serial.capture_start` / `serial.capture_stop` — tee session bytes to a log file.
- Client config (`~/.claude/mcp.json` entry).

## License

Internal tool — see surrounding repo.
