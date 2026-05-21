// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end loopback integration test against the **rmcp wire shape**,
//! exercised through the real release binary as a subprocess.
//!
//! Hardware-style I/O: `socat` creates a PTY pair whose symlinks live
//! under `/tmp`. A background OS thread on the device end echoes back
//! everything it reads — so a `serial.write` followed by a
//! `serial.read_until` for the same payload validates the full round
//! trip through real serial reads/writes via `tokio-serial`.
//!
//! Wire shape (post step 14 — `src/main.rs` now speaks rmcp):
//!   * `initialize` envelope per MCP 2025-11-25
//!   * `notifications/initialized`
//!   * each serial.* tool invoked via a `tools/call` envelope
//!   * responses read from `result.structuredContent.<field>`
//!
//! Skips at runtime when `socat` is not on PATH. To run locally:
//!
//! ```sh
//! sudo apt install socat   # or: brew install socat
//! cargo test --test loopback -- --nocapture
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const RESP_TIMEOUT: Duration = Duration::from_secs(5);

fn socat_available() -> bool {
    Command::new("socat")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// PTY pair created by socat; symlinks live under /tmp with predictable
/// names, both matching `/tmp/mcp-loopback-{pid}-*`. Dropping the value
/// kills socat and removes the symlinks.
struct PtyPair {
    socat: Child,
    a_path: String,
    b_path: String,
}

impl Drop for PtyPair {
    fn drop(&mut self) {
        let _ = self.socat.kill();
        let _ = self.socat.wait();
        let _ = std::fs::remove_file(&self.a_path);
        let _ = std::fs::remove_file(&self.b_path);
    }
}

fn start_pty_pair() -> PtyPair {
    let pid = std::process::id();
    let a_path = format!("/tmp/mcp-loopback-{pid}-A");
    let b_path = format!("/tmp/mcp-loopback-{pid}-B");
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);

    let mut socat = Command::new("socat")
        .args([
            "-d",
            "-d",
            &format!("pty,raw,echo=0,link={a_path}"),
            &format!("pty,raw,echo=0,link={b_path}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn socat");

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if Path::new(&a_path).exists() && Path::new(&b_path).exists() {
            // Symlinks resolve; allow socat a beat to finish wiring
            // the bridge before either side is opened for I/O.
            thread::sleep(Duration::from_millis(80));
            return PtyPair { socat, a_path, b_path };
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = socat.kill();
    let _ = socat.wait();
    panic!("socat did not create both PTY links within 3s");
}

/// Background OS thread that reads from the device end of the PTY pair
/// and writes the same bytes back, creating a hardware-style echo loop.
fn spawn_echo_thread(device_path: &str) -> thread::JoinHandle<()> {
    let path = device_path.to_string();
    thread::spawn(move || {
        let mut dev = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("echo thread: failed to open device {path}: {e}");
                return;
            }
        };
        let mut buf = [0u8; 256];
        loop {
            match dev.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if dev.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    if dev.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

/// Pump child stdout into a channel; each whole line lands as a parsed
/// JSON value. Empty lines and non-JSON lines (should never happen, but
/// belt-and-braces) are dropped with a stderr note.
fn spawn_response_pump(stdout: std::process::ChildStdout) -> mpsc::Receiver<Value> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                eprintln!("non-JSON line from binary: {line}");
                continue;
            };
            if tx.send(v).is_err() {
                break;
            }
        }
    });
    rx
}

fn next_response(rx: &mpsc::Receiver<Value>) -> Value {
    rx.recv_timeout(RESP_TIMEOUT)
        .expect("binary did not respond within timeout")
}

fn send(stdin: &mut std::process::ChildStdin, req: Value) {
    let line = req.to_string();
    writeln!(stdin, "{line}").expect("write to binary stdin");
    stdin.flush().expect("flush binary stdin");
}

#[test]
fn loopback_rmcp_open_write_echo_read_until_close() {
    if !socat_available() {
        eprintln!(
            "loopback: skipping — socat not installed. \
             Install with `sudo apt install socat` (or `brew install socat`) and re-run."
        );
        return;
    }

    let pty = start_pty_pair();
    let echo = spawn_echo_thread(&pty.b_path);

    // Spawn the real binary with an allowlist override so /tmp/...
    // resolves. The binary now speaks the rmcp wire on stdio
    // (post-step 14 main.rs switch).
    let bin = env!("CARGO_BIN_EXE_mcp-serial-rs");
    let pid = std::process::id();
    let allowlist = format!("/tmp/mcp-loopback-{pid}-*");
    let mut child = Command::new(bin)
        .env("MCP_SERIAL_ALLOWLIST", &allowlist)
        // Disable the journal for this test — `try_open_arc` would
        // otherwise lock-step against /tmp/mcp-serial-journal.jsonl
        // and we don't need audit rows for the loopback assertion.
        .env("MCP_SERIAL_JOURNAL", format!("/tmp/mcp-loopback-{pid}.jsonl"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp-serial-rs binary");

    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let rx = spawn_response_pump(stdout);

    // 1. initialize — rmcp lifecycle handshake.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "loopback", "version": "0.0.1"},
            }
        }),
    );
    let init = next_response(&rx);
    assert_eq!(init["id"], 1);
    assert_eq!(
        init["result"]["serverInfo"]["name"], "mcp-serial-rs",
        "rmcp wire: name lives under serverInfo, got {init}",
    );
    assert!(init["result"]["capabilities"]["tools"].is_object());

    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // 2. tools/list — should advertise all seven dotted tool names.
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let list = next_response(&rx);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    for expected in [
        "serial.list_ports",
        "serial.open",
        "serial.write",
        "serial.read",
        "serial.read_until",
        "serial.exec",
        "serial.close",
    ] {
        assert!(
            tools.iter().any(|t| t["name"] == expected),
            "tools/list missing `{expected}`; got {tools:?}",
        );
    }

    // 3. tools/call serial.open against the manager-side PTY.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "serial.open",
                "arguments": {
                    "port": pty.a_path,
                    "baud": 115_200,
                    "timeout_ms": 1000,
                },
            }
        }),
    );
    let open = next_response(&rx);
    assert!(
        open.get("error").is_none(),
        "open failed (allowlist not honored?): {open}",
    );
    let session_id = open["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session_id string in structuredContent")
        .to_string();
    assert_eq!(session_id.len(), 16, "16-char hex session id");

    // 4. tools/call serial.write — bytes onto the manager-side PTY.
    let payload = "echo-marker-7f3a\n";
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "serial.write",
                "arguments": {"session_id": session_id, "data": payload},
            }
        }),
    );
    let write_resp = next_response(&rx);
    assert_eq!(
        write_resp["result"]["structuredContent"]["bytes_written"],
        payload.len() as u64,
        "write response: {write_resp}",
    );

    // 5. tools/call serial.read_until — echo thread bounces the
    //    marker back; matcher should see it.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "serial.read_until",
                "arguments": {
                    "session_id": session_id,
                    "pattern": "echo-marker-7f3a",
                    "timeout_ms": 2000,
                },
            }
        }),
    );
    let ru = next_response(&rx);
    assert!(ru.get("error").is_none(), "read_until errored: {ru}");
    let structured = &ru["result"]["structuredContent"];
    assert_eq!(structured["matched"], true, "round-trip failed; got: {ru}");
    let data = structured["data"].as_str().expect("data string");
    assert!(
        data.contains("echo-marker-7f3a"),
        "echoed data missing from buffer: {data:?}",
    );

    // 6. tools/call serial.close — releases the session.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "serial.close",
                "arguments": {"session_id": session_id},
            }
        }),
    );
    let close = next_response(&rx);
    assert_eq!(close["result"]["structuredContent"]["ok"], json!(true));

    // Shutdown: EOF stdin, reap binary, then drop the PTY (kills socat,
    // which causes the echo thread's read to return Ok(0) and exit).
    drop(stdin);
    let _ = child.wait();
    drop(pty);
    let _ = echo.join();

    // Clean up the per-test journal file too.
    let _ = std::fs::remove_file(format!("/tmp/mcp-loopback-{pid}.jsonl"));
}
