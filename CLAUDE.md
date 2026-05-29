# mcp-serial-rs — Claude Code Orientation

> MCP tool server exposing serial-port access over stdio.
> Example consumer: an ESP32-C6 Zephyr DFU application on a USB-UART
> adapter (e.g. `/dev/ttyUSBx`).

The MCP protocol layer is the `rmcp` SDK over its stdio transport. The
migration off the original hand-rolled JSON-RPC layer is complete; its
plan, constraints, and acceptance criteria are kept for reference in
[`docs/archive/rmcp-migration-spec.md`](docs/archive/rmcp-migration-spec.md).
That document is historical — this file is the current source of truth.

---

## 1  Decisions (do not re-litigate)

| Topic | Decision | Notes |
|---|---|---|
| Language | Rust | Edition 2021 retained (`rmcp` 1.7's edition 2024 is a dependency-side concern, not a consumer requirement). MSRV: Rust 1.85. |
| Async runtime | **tokio** (multi-thread) | Everything is async; no blocking I/O on the stdio loop |
| Serial crate | **tokio-serial 5.x** | NOT `serialport` (sync). Async read/write mandatory |
| Error strategy | **thiserror** for library errors, map to MCP error codes / structured tool results | NOT `anyhow` — we need typed error variants |
| MCP transport | **`rmcp` SDK over stdio** | The SDK owns initialize, `tools/list`, `tools/call`, and `notifications/initialized`. The serial domain stays separate (see §3). |
| Logging | **tracing** + **tracing-subscriber** (env-filter, fmt, stderr writer) | `rmcp::transport::stdio()` does NOT redirect logs — stderr writer setup stays mandatory. |
| Pattern matching | **regex** crate for `read_until` and prompt detection | |
| Serialisation | **serde** + **serde_json** + **schemars** | All tool params and results are typed structs; rmcp generates input schemas from them and supports structured tool results (`structuredContent`). |

- Error semantics: Protocol errors are for validation failures: bad params,
  unknown session, disallowed port, malformed regex. Structured tool results are
  for runtime outcomes: timeout with partial data, exec failure with partial
  output. Runtime outcomes carry `ok: false` and partial data in the result, not
  as JSON-RPC errors.
- Capture architecture (future): single reader per session. Any future capture
  implementation must fan data out from one reader task via
  `tokio::sync::broadcast`; direct reads and capture subscribers consume from
  the same stream. No competing readers on the port.

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
rmcp = { version = "1.7", features = ["transport-io"] }
schemars = "1.0"
```

`rmcp` 1.7 + `schemars` 1.0 are the MCP/schema stack; adopting them raised the
project MSRV to Rust 1.85 (edition 2021 retained — `rmcp`'s edition 2024 is a
dependency-side concern). `rmcp`'s default features already include `macros`
and `server`; `transport-io` is enabled for the stdio transport.

No other runtime dependencies without explicit approval.

`[dev-dependencies]`: `tokio-test`, `assert_matches`, `tempfile` (for PTY loopback tests).

## 3  Module map

```
mcp-serial-rs/
├── Cargo.toml
├── CLAUDE.md          ← you are here
├── docs/
│   ├── mcp-serial-architecture.md  ← architecture diagrams
│   └── archive/
│       └── rmcp-migration-spec.md  ← historical: the completed rmcp migration
├── src/
│   ├── lib.rs         ← crate root; re-exports modules for integration tests
│   ├── main.rs        ← tokio bootstrap; builds McpServer and hands stdio to rmcp
│   ├── mcp/
│   │   ├── mod.rs     ← rmcp adapter: McpServer, #[tool] handlers, journal hook
│   │   └── journal.rs ← rmcp-call-shaped journal row construction (summaries)
│   ├── serial/
│   │   ├── mod.rs
│   │   ├── manager.rs ← SessionManager: HashMap<SessionId, Session>
│   │   ├── session.rs ← per-port async state, reader task, ring buffer
│   │   ├── parser.rs  ← line/prompt matching, read_until logic
│   │   └── journal.rs ← JournalWriter: append-only JSONL sink, degraded mode
│   ├── config.rs      ← port allowlist, limits, defaults
│   └── errors.rs      ← SerialError enum, error mapping (incl. → rmcp::ErrorData)
└── tests/
    ├── mcp_tests.rs   ← in-process rmcp wire tests (all tools, journal, concurrency)
    ├── loopback.rs    ← end-to-end PTY loopback through the release binary
    ├── parser_tests.rs
    └── session_tests.rs
```

Architecture constraint (carried over from the migration spec §Architecture
Constraints): keep MCP protocol concerns separated from the serial domain.
The `mcp/` adapter owns rmcp wiring; the serial manager / session / parser /
journal modules own their respective concerns. The rmcp adapter must not
absorb serial-domain logic, and the `serial/` submodule must not be
flattened.

## 4  MCP tool surface

Tool names and field shapes are stable — do not rename or reshape them.

| Tool | Params | Returns |
|---|---|---|
| `serial.list_ports` | — | `[{port, vid, pid, serial, device, description}]` |
| `serial.open` | `{port, baud?, timeout_ms?}` OR `{device, baud?, timeout_ms?}` | `{session_id}` |
| `serial.write` | `{session_id, data}` | `{bytes_written}` |
| `serial.read` | `{session_id, max_bytes?, timeout_ms?}` | `{data, timed_out}` |
| `serial.read_until` | `{session_id, pattern, timeout_ms?}` | `{data, matched}` |
| `serial.exec` | `{session_id, command, expect, timeout_ms?}` | `{output, ok}` |
| `serial.close` | `{session_id}` | `{ok}` |

Behavioural notes:

- Dotted tool names stay (no rename to `serial_open` etc.).
- `data` fields are UTF-8 lossy strings; no `hex:` prefix; binary protocols
  are not in scope.
- `serial.exec` writes `command` verbatim — no implicit newline — and
  validates `expect` (regex + non-empty) BEFORE writing, so an invalid
  pattern cannot mutate device state and only then return InvalidParam.
- `serial.exec` returns partial output with `ok=false` on timeout (same
  buffer `serial.read_until` would have returned).
- `serial.read_until` returns partial data with `matched=false` on timeout
  or EOF — partial output is normal completion, not an error.
- `serial.read` drains until `max_bytes` is reached or the deadline
  elapses, returning `{data, timed_out}`. A deadline hit is `timed_out=true`
  with whatever bytes accumulated (possibly empty) — NOT a JSON-RPC error.
  EOF returns `timed_out=false`. Genuine I/O failures still surface as
  errors. (Issue #4 — domain-outcome parity with `read_until`.)
- `serial.open` distinguishes a missing port (`PortNotFound`, -32002)
  from a port held exclusively by another process (`PortBusy`, -32010,
  reported when `serialport-rs` surfaces `EBUSY` via `ErrorKind::NoDevice`).

Tool results use rmcp's structured tool results (`CallToolResult::structured`)
for object outputs so clients read parsed fields without re-parsing a text
blob. Input schemas are generated by rmcp; output schemas are not (deferred).

Deferred (not implemented):

- `serial.reset_esp32` — DTR/RTS toggle strategies
- `serial.capture_start` / `serial.capture_stop` — tee mid-session bytes to a log file

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
| `MCP_SERIAL_JOURNAL` | Append-only JSONL **tool-call** journal: every `tools/call` appends a `call` row and, when a result is produced, a `result` row. Lifecycle traffic (`initialize`, `tools/list`, `notifications/initialized`) is not journaled. Always-on auditing — not opt-in — but unwritable paths degrade to a `tracing::warn` and continue without journaling rather than failing to start. | `/tmp/mcp-serial-journal.jsonl` |
| `RUST_LOG` | `tracing-subscriber` env filter. | `mcp_serial_rs=info` |

**Device profiles** (loaded once at startup):

- Device profiles (`devices.toml`) are optional convenience support for
  pinned local environments, not the primary workflow. Do not promote them as
  a headline feature in README/docs. The primary workflow is:
  `serial.list_ports` → choose an allowlisted port → `serial.open` with
  `port`. Preserve existing profile support and tests, but do not expand
  profile coverage or let profile behavior drive the rmcp migration design.
- Each profile keyed by a short name (`esp32c6`, `imx93-evk`, …) with fields `match_serial`, `match_vid?`, `match_pid?`, `baud`, `description`, `probe?`, `tags`.
- `serial.list_ports` enriches matched ports with `device` (profile name) and `description` (from profile).
- `serial.open` accepts `{device}` as an alternative to `{port}` — the profile's `baud` is used as the default. `DeviceNotFound` error if the profile is unknown or no currently-plugged port matches it.

## 7  MCP wire & framing

The MCP framing on stdio is owned by `rmcp`. Initialize, `tools/list`,
`tools/call`, and `notifications/initialized` are handled by the SDK; this
project does not hand-code JSON-RPC envelopes.

Invariants:

- **stdout is reserved for MCP messages only.** No `println!`, no debug
  prints, no stray writes. `rmcp::transport::stdio()` returns
  `(tokio::io::Stdin, tokio::io::Stdout)` and does not redirect anything —
  the caller MUST configure `tracing-subscriber` with a stderr writer.
- **Logs go to stderr only**, via `tracing` + `tracing-subscriber`. Driven
  by `RUST_LOG`.
- **rmcp dispatches requests concurrently** (`tokio::task::JoinSet` inside
  the service loop). Two tool calls on the same connection can run in
  parallel. The per-session port mutex in `serial/manager.rs` is therefore
  load-bearing, not defensive — every concurrent-access invariant in spec
  §Architecture Constraints applies.

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

- Do not add `anyhow`. Use `thiserror` — typed error variants are required.
- Do not add `serialport` (sync). Use `tokio-serial`.
- Do not rename tools from dotted (`serial.open`) to underscored
  (`serial_open`).
- Do not implement `serial.reset_esp32` or `serial.capture_*` without a
  separate decision — they are deferred (§4).
- Do not add a `hex:` prefix or other binary payload encoding to
  `serial.write`. UTF-8 only.
- Do not make `serial.exec` append a newline to `command`. Caller
  controls the bytes.
- Do not change the session-id format (16-char hex, SplitMix64) without a
  separate decision.
- Do not silently drop audit-journal behaviour, and do not widen it back to
  full MCP traffic — it is intentionally a tool-call / serial-operation
  journal (§6).
- Do not write to stdout except MCP-framed responses (via `rmcp`). All
  logs → stderr via `tracing`.
- Do not use `std::thread` for concurrency. Everything is tokio tasks.
- Do not flatten the `serial/` submodule — keep `manager.rs`, `session.rs`,
  `parser.rs`, `journal.rs` separate. Likewise keep the `mcp/` adapter
  separate from serial-domain logic.
