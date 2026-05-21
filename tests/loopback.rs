// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end loopback integration test against the **rmcp wire shape**.
//!
//! Hardware-style I/O: `socat` creates a PTY pair whose symlinks live
//! under `/tmp`. A background OS thread on the device end echoes back
//! everything it reads — so a `serial.write` followed by a
//! `serial.read_until` for the same payload validates the full round
//! trip through real serial reads/writes via `tokio-serial`.
//!
//! Wire shape: the rmcp `McpServer` is wired in-process over a
//! `tokio::io::duplex` pair; the test side speaks the SDK envelopes
//! (`initialize`, `tools/call` with `name` + `arguments`, structured
//! results in `structuredContent`). The hand-rolled binary's
//! line-loop in `src/main.rs` is intentionally NOT in scope here —
//! it stays untouched until spec §Migration Sequence step 14 switches
//! it over. (Until then, `tests/protocol_tests.rs` keeps subprocess
//! coverage of the legacy wire.)
//!
//! Skips at runtime when `socat` is not on PATH. To run locally:
//!
//! ```sh
//! sudo apt install socat   # or: brew install socat
//! cargo test --test loopback -- --nocapture
//! ```

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use mcp_serial_rs::mcp::McpServer;
use mcp_serial_rs::serial::manager::{SessionManager, TokioSerialBackend};

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
/// Returns the JoinHandle so the test can wait for orderly shutdown.
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

/// Send one line-delimited JSON message into the rmcp server.
async fn send_line(
    writer: &mut (impl AsyncWriteExt + Unpin),
    value: &Value,
) {
    let mut bytes = serde_json::to_vec(value).expect("encode");
    bytes.push(b'\n');
    writer.write_all(&bytes).await.expect("write line");
    writer.flush().await.expect("flush");
}

/// Read one response from the rmcp server, with [`RESP_TIMEOUT`].
async fn read_response<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Value {
    let mut buf = String::new();
    let n = timeout(RESP_TIMEOUT, reader.read_line(&mut buf))
        .await
        .expect("response timeout")
        .expect("read line");
    assert!(n > 0, "EOF on rmcp server stdout-equivalent");
    serde_json::from_str(buf.trim_end()).expect("response JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_rmcp_open_write_echo_read_until_close() {
    if !socat_available() {
        eprintln!(
            "loopback: skipping — socat not installed. \
             Install with `sudo apt install socat` (or `brew install socat`) and re-run."
        );
        return;
    }

    let pty = start_pty_pair();
    let echo = spawn_echo_thread(&pty.b_path);

    // Allowlist override so `/tmp/mcp-loopback-{pid}-A` is accepted by
    // SessionManager::open. The env var bleeds into the whole test
    // process — this is the only test in `tests/loopback.rs`, and
    // each integration test file is its own cargo test binary, so
    // cross-test contention does not arise here.
    let pid = std::process::id();
    let allowlist = format!("/tmp/mcp-loopback-{pid}-*");
    // SAFETY: single-test binary; no concurrent env access here.
    unsafe { std::env::set_var("MCP_SERIAL_ALLOWLIST", &allowlist) };

    // Build the in-process rmcp server with the REAL tokio-serial
    // backend; the loopback intentionally exercises real OS-level
    // serial I/O via the PTY, not a stub.
    let sessions = Arc::new(SessionManager::new(TokioSerialBackend));
    let server = McpServer::new(sessions, Arc::new(Vec::new()), None);

    // Wire the server end of an in-memory duplex; client side stays
    // here in the test for driving the rmcp wire.
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let svc = server.serve(server_io).await.expect("serve start");
        let _ = svc.waiting().await;
    });
    let (cread, mut cwrite) = tokio::io::split(client_io);
    let mut reader = BufReader::new(cread);

    // 1. initialize — confirms the rmcp lifecycle handshake.
    send_line(
        &mut cwrite,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "loopback", "version": "0.0.1"},
            }
        }),
    )
    .await;
    let init = read_response(&mut reader).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "mcp-serial-rs");

    send_line(
        &mut cwrite,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;

    // 2. tools/call serial.open against the manager-side PTY.
    send_line(
        &mut cwrite,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
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
    )
    .await;
    let open = read_response(&mut reader).await;
    assert!(
        open.get("error").is_none(),
        "open failed (allowlist not honored?): {open}"
    );
    let session_id = open["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session_id string in structuredContent")
        .to_string();
    assert_eq!(session_id.len(), 16, "16-char hex session id");

    // 3. tools/call serial.write — bytes go onto the manager-side PTY.
    let payload = "echo-marker-7f3a\n";
    send_line(
        &mut cwrite,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "serial.write",
                "arguments": {"session_id": session_id, "data": payload},
            }
        }),
    )
    .await;
    let write_resp = read_response(&mut reader).await;
    assert_eq!(
        write_resp["result"]["structuredContent"]["bytes_written"],
        payload.len() as u64,
        "write response: {write_resp}",
    );

    // 4. tools/call serial.read_until — the OS echo thread bounces
    //    the bytes back; the matcher should see the marker.
    send_line(
        &mut cwrite,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
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
    )
    .await;
    let ru = read_response(&mut reader).await;
    assert!(ru.get("error").is_none(), "read_until errored: {ru}");
    let structured = &ru["result"]["structuredContent"];
    assert_eq!(structured["matched"], true, "round-trip failed; got: {ru}");
    let data = structured["data"].as_str().expect("data string");
    assert!(
        data.contains("echo-marker-7f3a"),
        "echoed data missing from buffer: {data:?}",
    );

    // 5. tools/call serial.close — releases the session.
    send_line(
        &mut cwrite,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "serial.close",
                "arguments": {"session_id": session_id},
            }
        }),
    )
    .await;
    let close = read_response(&mut reader).await;
    assert_eq!(close["result"]["structuredContent"]["ok"], json!(true));

    // Shutdown: drop the client side so the server task exits, then
    // bring down the PTY pair (kills socat, which causes the echo
    // thread's read to return Ok(0) and the JoinHandle to resolve).
    drop(cwrite);
    drop(reader);
    let _ = timeout(Duration::from_secs(2), server_task).await;
    drop(pty);
    let _ = echo.join();

    // SAFETY: same single-test rationale as the set above.
    unsafe { std::env::remove_var("MCP_SERIAL_ALLOWLIST") };
}
