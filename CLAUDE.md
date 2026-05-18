# mcp-serial-rs — Claude Code Orientation

> MCP tool server exposing serial-port access over JSON-RPC / stdio.
> First consumer: ESP32-C6 Zephyr DFU application over `/dev/ttyUSB1`.

---

## 1  Decisions (do not re-litigate)

| Topic | Decision | Notes |
|---|---|---|
| Language | Rust, edition 2021, MSRV 1.75+ | |
| Async runtime | **tokio** (multi-thread) | Everything is async; no blocking I/O on the stdio loop |
| Serial crate | **tokio-serial 5.x** | NOT `serialport` (sync). Async read/write mandatory |
| Error strategy | **thiserror** for library errors, map to MCP JSON-RPC error codes | NOT `anyhow` — we need typed error variants |
| MCP transport | Hand-rolled JSON-RPC 2.0 over stdio | No external MCP SDK crate. ~200 LOC in `protocol.rs` |
| Logging | **tracing** + **tracing-subscriber** (env-filter, fmt) | `RUST_LOG=mcp_serial=debug` |
| Pattern matching | **regex** crate for `read_until` and prompt detection | |
| Serialisation | **serde** + **serde_json** | All tool params and results are typed structs |

## 2  Crate manifest (Cargo.toml `[dependencies]`)

```toml
tokio = { version = "1", features = ["full"] }
tokio-serial = "5"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
regex = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
thiserror = "2"
```

No other runtime dependencies without explicit approval.

`[dev-dependencies]`: `tokio-test`, `assert_matches`, `tempfile` (for PTY loopback tests).

## 3  Module map

```
mcp-serial-rs/
├── Cargo.toml
├── CLAUDE.md          ← you are here
├── TASKS.md           ← phased work plan
├── src/
│   ├── lib.rs         ← crate root; re-exports modules for integration tests
│   ├── main.rs        ← tokio bootstrap, stdio read loop, dispatch
│   ├── protocol.rs    ← JSON-RPC 2.0 request/response/error types
│   ├── tools.rs       ← tool name → handler dispatch table
│   ├── serial/
│   │   ├── mod.rs
│   │   ├── manager.rs ← SessionManager: HashMap<SessionId, Session>
│   │   ├── session.rs ← per-port async state, reader task, ring buffer
│   │   ├── parser.rs  ← line/prompt matching, read_until logic
│   │   └── journal.rs ← always-on JSONL traffic journal (call + result)
│   ├── config.rs      ← port allowlist, limits, defaults
│   └── errors.rs      ← SerialError enum, impl Into<JsonRpcError>
└── tests/
    ├── protocol_tests.rs
    ├── parser_tests.rs
    └── session_tests.rs
```

Create exactly this structure. Do not flatten modules or invent new ones.

## 4  MCP tool surface (MVP — Phase 1)

| Tool | Params | Returns |
|---|---|---|
| `serial.list_ports` | — | `[{port, vid, pid, serial, device, description}]` |
| `serial.open` | `{port, baud, timeout_ms}` OR `{device, baud?, timeout_ms}` | `{session_id}` |
| `serial.write` | `{session_id, data}` | `{bytes_written}` |
| `serial.read` | `{session_id, max_bytes, timeout_ms}` | `{data}` |
| `serial.read_until` | `{session_id, pattern, timeout_ms}` | `{data, matched}` |
| `serial.exec` | `{session_id, command, expect, timeout_ms}` | `{output, ok}` |
| `serial.close` | `{session_id}` | `{ok}` |

`serial.exec` is a pure composition of `serial.write` then `serial.read_until`. The
caller's `command` bytes are written verbatim (no implicit newline); `timeout_ms`
applies to the read-until phase only and is clamped to `MAX_TIMEOUT_MS`. `output`
is the same accumulated buffer `serial.read_until` would have returned — i.e.
all bytes read up to and including the chunk that first satisfied the match (so
trailing bytes past the match point can appear), or whatever was read so far on
timeout / EOF. `ok` mirrors `read_until`'s `matched`.

### Phase 2 (deferred — do not implement until told)

- `serial.reset_esp32` — DTR/RTS toggle strategies
- `serial.capture_start` / `serial.capture_stop` — log to file

## 5  Session state machine

```
Opening ──▸ Ready ──▸ Closing ──▸ Closed
   │                     ▲
   └─── (error) ─────────┘
```

**Rules:**
- `session_id` is a random `u64` formatted as hex string.
- Every tool call validates `session_id` exists **and** state is `Ready` (except `close`, which accepts `Ready` or `Opening`).
- `Opening → Ready` transition happens only after the serial port is successfully opened and configured.
- `close` sets state to `Closing`, drops the reader task, flushes, closes port, then sets `Closed`.
- Closed sessions are removed from the map after `close` returns.
- Max concurrent sessions: 4 (configurable in `config.rs`).

## 6  Config & safety

```rust
// config.rs defaults
pub const PORT_ALLOWLIST: &[&str] = &["/dev/ttyUSB*", "/dev/ttyACM*"];
pub const MAX_SESSIONS: usize = 4;
pub const MAX_READ_BUFFER: usize = 64 * 1024;   // 64 KiB ring buffer
pub const MAX_WRITE_CHUNK: usize = 4096;         // 4 KiB per write
pub const DEFAULT_BAUD: u32 = 115200;
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;
pub const MAX_TIMEOUT_MS: u64 = 30_000;
```

- Port allowlist is glob-matched. Reject anything outside it with a clear error.
- All timeouts are clamped to `MAX_TIMEOUT_MS`.
- `data` fields in `write` and `read` results are **UTF-8 strings**, not raw bytes. This is a console/shell tool, not a binary protocol bridge.

**Environment variables:**

| Var | Purpose | Default |
|---|---|---|
| `MCP_SERIAL_ALLOWLIST` | Comma-separated glob patterns. When set, replaces `PORT_ALLOWLIST` entirely. Used by integration tests under `/tmp/...` and on hosts with non-standard device paths. | unset → compiled-in list |
| `MCP_SERIAL_DEVICES` | Path to a TOML file mapping stable USB serial strings to device profiles (see `devices.toml`). Missing file is not an error — profiles are optional. | `./devices.toml` |
| `MCP_SERIAL_JOURNAL` | Append-only JSONL traffic journal. Every MCP tool call appends a `call` row and (when a result is produced) a `result` row. Always-on auditing — not opt-in — but unwritable paths degrade to a `tracing::warn` and continue without journaling rather than failing to start. | `/tmp/mcp-serial-journal.jsonl` |
| `RUST_LOG` | `tracing-subscriber` env filter. | `mcp_serial_rs=info` |

**Device profiles** (loaded once at startup):

- Each profile keyed by a short name (`esp32c6`, `imx93-evk`, …) with fields `match_serial`, `match_vid?`, `match_pid?`, `baud`, `description`, `probe?`, `tags`.
- `serial.list_ports` enriches matched ports with `device` (profile name) and `description` (from profile).
- `serial.open` accepts `{device}` as an alternative to `{port}` — the profile's `baud` is used as the default. `DeviceNotFound` error if the profile is unknown or no currently-plugged port matches it.

## 7  JSON-RPC 2.0 contract

### Request (stdin, one JSON object per line)

```json
{"jsonrpc":"2.0","id":1,"method":"serial.open","params":{"port":"/dev/ttyUSB1","baud":115200}}
```

### Success response (stdout)

```json
{"jsonrpc":"2.0","id":1,"result":{"session_id":"a3f7c012"}}
```

### Error response (stdout)

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"port not in allowlist","data":{"port":"/dev/ttyS0"}}}
```

### MCP lifecycle methods (must implement)

| Method | Purpose |
|---|---|
| `initialize` | Return server name, version, tool list |
| `tools/list` | Return tool schemas (JSON Schema for each tool's params) |
| `notifications/initialized` | Client ack — no response needed |

**Logging goes to stderr only.** stdout is exclusively the JSON-RPC channel.

## 8  Coding conventions

- `#![deny(clippy::all)]` in both crate roots (`main.rs` and `lib.rs`).
- All public types derive `Debug, Clone, Serialize, Deserialize` where sensible.
- No `unwrap()` or `expect()` in library code. `main.rs` may `expect()` on bootstrap only.
- Error variants carry context (port name, session_id, underlying OS error string).
- Use `tracing::instrument` on all tool handler functions.
- Line length soft limit: 100 columns.
- Tests go in `tests/` (integration) or inline `#[cfg(test)] mod tests` (unit).

## 9  Build verification

After each task, run:

```sh
cargo clippy -- -D warnings
cargo test
cargo build --release
```

All three must pass before reporting task done.

## 10  Do NOT

- Do not add `anyhow`. Use `thiserror`.
- Do not add `serialport` (sync). Use `tokio-serial`.
- Do not add any MCP SDK crate. We hand-roll JSON-RPC.
- Do not implement Phase 2 tools until explicitly asked.
- Do not write to stdout except JSON-RPC responses. All logs → stderr via `tracing`.
- Do not use `std::thread` for concurrency. Everything is tokio tasks.
- Do not flatten the `serial/` submodule — keep `manager.rs`, `session.rs`, `parser.rs` separate.
