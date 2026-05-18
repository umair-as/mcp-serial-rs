# mcp-serial-rs — Task Plan

Feed these to Claude Code **one at a time, in order**.
Each task assumes the previous one is complete and passing `cargo clippy && cargo test && cargo build`.

---

## Task 1 — Project skeleton and protocol layer

**Prompt:**

> Read CLAUDE.md first. Delete the current `src/main.rs` content and set up the full project
> structure from Section 3 of CLAUDE.md. Implement `protocol.rs` with JSON-RPC 2.0
> request/response/error types (Section 7). Implement `errors.rs` with a `SerialError` enum
> covering at least: PortNotAllowed, PortNotFound, SessionNotFound, InvalidState, Timeout,
> IoError, MaxSessionsReached. Map each variant to a unique negative JSON-RPC error code.
> Implement `config.rs` with the constants from Section 6. Wire a minimal `main.rs` that
> reads one JSON line from stdin, deserializes it, and responds with `{"error":"not implemented"}`.
> All modules must exist as files with at least a doc comment. Run `cargo clippy -- -D warnings`
> and `cargo build`.

**Done when:** compiles cleanly, all modules exist, `echo '{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}' | cargo run` returns a valid JSON-RPC error response on stdout.

---

## Task 2 — MCP lifecycle (initialize, tools/list)

**Prompt:**

> Implement the `initialize` and `tools/list` MCP methods in the dispatcher (`tools.rs` / `main.rs`).
> `initialize` returns `{name: "mcp-serial-rs", version: "0.1.0", capabilities: {tools: {}}}`.
> `tools/list` returns a JSON array with one entry per MVP tool (Section 4), each with a `name`,
> `description`, and `inputSchema` (JSON Schema object for params). Handle `notifications/initialized`
> as a no-op (no response). Add a protocol-level integration test that sends the three-message
> handshake via stdin and asserts the responses.

**Done when:** `cargo test` passes including the new integration test. `cargo clippy -- -D warnings` clean.

---

## Task 3 — SessionManager and serial.open / serial.close

**Prompt:**

> Implement `serial/manager.rs` (SessionManager) and `serial/session.rs` (Session struct with
> the state machine from CLAUDE.md Section 5). `serial.open` validates the port against the
> allowlist (glob match), opens it via `tokio-serial` with the given baud rate, transitions
> Opening→Ready, and returns a hex session_id. `serial.close` transitions Ready→Closing→Closed,
> drops the port, removes the session. Enforce max 4 concurrent sessions. Add unit tests for:
> allowlist acceptance/rejection, state transitions, max-session limit. You cannot test real
> serial ports here — gate hardware-dependent code behind a trait so tests can use a mock.

**Done when:** `cargo test` passes, `cargo clippy` clean, the `SessionManager` is fully tested with mock serial backend.

---

## Task 4 — serial.write and serial.read

**Prompt:**

> Implement `serial.write` (validates session is Ready, writes UTF-8 data to the port, returns
> bytes_written) and `serial.read` (reads up to max_bytes with timeout, returns UTF-8 data).
> Timeouts must be clamped to MAX_TIMEOUT_MS from config. Use `tokio::time::timeout` wrapping
> the async serial read. Add unit tests using the mock serial backend from Task 3.

**Done when:** `cargo test` passes, `cargo clippy` clean.

---

## Task 5 — serial.read_until and parser

**Prompt:**

> Implement `serial/parser.rs` with a `PatternMatcher` that takes a regex pattern string and
> matches against an accumulating buffer. Implement `serial.read_until` which reads bytes into
> a buffer until the pattern matches or timeout expires. Return `{data, matched: bool}`.
> Add thorough unit tests for the parser: exact match, regex match, timeout with partial data,
> empty pattern (should error), buffer overflow (truncate at MAX_READ_BUFFER).

**Done when:** `cargo test` passes with ≥6 parser test cases, `cargo clippy` clean.

---

## Task 6 — serial.list_ports

**Prompt:**

> Implement `serial.list_ports` using `tokio_serial::available_ports()` (which delegates to
> `serialport::available_ports()`). Filter results through the port allowlist. Return an array
> of `{port, vid, pid, serial}` where vid/pid/serial come from UsbPortInfo if available, or
> null otherwise. This tool requires no session. Add a unit test that verifies the allowlist
> filtering logic using a mock port list.

**Done when:** `cargo test` passes, `cargo clippy` clean, `cargo build --release` succeeds.

---

## Task 7 — End-to-end loopback integration test

**Prompt:**

> Create `tests/loopback.rs`. Use `socat` or `openpty` to create a PTY pair. Open one end
> via `serial.open`, write a known string via `serial.write`, echo it back on the other end,
> read it via `serial.read_until` with a matching pattern. Assert the round-trip works.
> If `socat` is unavailable, skip the test with `#[ignore]` and a comment explaining how to
> run it locally. This is the final MVP validation.

**Done when:** `cargo test` passes (loopback may be `#[ignore]` if no PTY available), full `cargo clippy && cargo build --release` clean. The binary at `target/release/mcp-serial-rs` is the deliverable.

---

## Task 7.5 — Device profile matching and named-device open

**Context:** The MCP server runs on a workstation with multiple serial boards connected simultaneously (ESP32-C6, i.MX93 EVK, RPi5, RPi Pico 2W). The agent must be able to identify boards by name, not by guessing ttyACM/ttyUSB numbers that shift across reboots. A `devices.toml` file maps stable USB serial strings to human-readable device profiles.

**Prompt:**

> Read CLAUDE.md and `devices.toml` in the repo root. Implement device-profile loading:
>
> 1. Add a `DeviceProfile` struct to `config.rs` with fields: `name` (the TOML key, e.g.
>    "esp32c6"), `match_serial`, `match_vid` (Option<u16>), `match_pid` (Option<u16>), `baud`,
>    `description`, `probe` (Option<String>), `tags` (Vec<String>). Add a `load_devices(path)`
>    function that parses `devices.toml` and returns `Vec<DeviceProfile>`. The file path comes
>    from the `MCP_SERIAL_DEVICES` env var, falling back to `./devices.toml`. If the file does
>    not exist, return an empty vec (not an error — profiles are optional).
>
> 2. Enrich `serial.list_ports` — for each discovered port, match its USB serial string against
>    loaded profiles. Return a `device` field (profile name or null) and `description` field
>    (from profile or null) alongside the existing `port`, `vid`, `pid`, `serial` fields.
>    Matching rule: `match_serial` must equal the port's serial string exactly. If `match_vid`
>    or `match_pid` are present in the profile, they must also match. First match wins.
>
> 3. Add a named-device path to `serial.open` — accept `{"device": "esp32c6"}` as an alternative
>    to `{"port": "/dev/ttyUSB1", "baud": 115200}`. When `device` is provided, resolve it to a
>    port by running the same match logic against currently available ports. Use the profile's
>    `baud` as the default (overridable by an explicit `baud` param). If the device is not found,
>    return a `DeviceNotFound` error. If both `device` and `port` are provided, reject with
>    `InvalidParam` ("specify device or port, not both").
>
> 4. Add a `DeviceNotFound` variant to `SerialError` with its own JSON-RPC error code.
>
> 5. Load device profiles once in `main.rs` at startup, store in an `Arc`, and pass to the
>    `SessionManager` or to dispatch alongside it.
>
> 6. Tests:
>    - Unit: `load_devices` with a valid TOML string, with a missing file, with malformed TOML.
>    - Unit: profile matching — exact serial match, vid+pid filter, no-match returns null.
>    - Unit: `serial.open` with `device` param resolves correctly.
>    - Unit: `serial.open` with both `device` and `port` returns InvalidParam.
>
> Run `cargo clippy -- -D warnings && cargo test && cargo build --release`.

**Done when:** `serial.list_ports` returns `device` and `description` fields, `serial.open` accepts `{"device": "esp32c6"}`, all new tests pass, clippy clean.

---

**CLAUDE.md updates needed for this task (add to §4 and §6):**

§4 tool surface — amend `serial.list_ports` return:
```
[{port, vid, pid, serial, device, description}]
```

§4 tool surface — amend `serial.open` params:
```
{port, baud, timeout_ms}  OR  {device, baud?, timeout_ms}
```

§6 config — add:
```
MCP_SERIAL_DEVICES env var → path to devices.toml (default: ./devices.toml)
```

---

## Task 8 — README.md

**Prompt:**

> Write a concise README.md covering: what this is (one paragraph), build instructions,
> usage example (pipe a JSON-RPC handshake + serial.list_ports into the binary and show the
> output), tool reference table, configuration (environment variables, allowlist), and a
> "Hardware smoke test" section with the ESP32-C6 Zephyr DFU example from CLAUDE.md.
> Keep it under 150 lines.

**Done when:** README.md exists and is accurate relative to the implemented code.

---

## Phase 2 tasks (do not start until Phase 1 is reviewed)

- **Task P2-1** ✅ DONE — `serial.exec` (compound write + read_until with `ok` flag). Dispatch-layer composition in `src/tools.rs`; no manager changes. Params `{session_id, command, expect, timeout_ms}` → `{output, ok}`. Timeout clamps to `MAX_TIMEOUT_MS`, command clamps to `MAX_WRITE_CHUNK`.
- **Task P2-2**: `serial.reset_esp32` — DTR/RTS toggle strategies (hard reset, bootloader entry)
- **Task P2-3**: `serial.capture_start` / `serial.capture_stop` — tee session data to a log file
- **Task P2-4**: Claude Code MCP client config (`~/.claude/mcp.json` entry for this server)
