//! End-to-end loopback integration test.
//!
//! Uses `socat` to create a PTY pair with predictable symlinks under `/tmp`.
//! One end is opened by the binary via `serial.open`; a background thread on
//! the test side echoes everything it reads back to the device — so a
//! `serial.write` followed by `serial.read_until` for the same payload
//! validates the full round trip through every code path the MVP touches:
//! JSON-RPC dispatch, session lifecycle, port I/O, regex matcher.
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
            // Symlinks resolve; allow socat a beat to finish wiring the
            // bridge before either side is opened for I/O.
            thread::sleep(Duration::from_millis(80));
            return PtyPair {
                socat,
                a_path,
                b_path,
            };
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Setup failed — reap socat before panicking so we don't leave a zombie.
    let _ = socat.kill();
    let _ = socat.wait();
    panic!("socat did not create both PTY links within 3s");
}

/// Spawn a background thread that reads from the device side of the PTY pair
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
/// JSON `Value`. Lets the test apply per-response timeouts cleanly.
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
fn loopback_open_write_echo_read_until_close() {
    if !socat_available() {
        eprintln!(
            "loopback: skipping — socat not installed. \
             Install with `sudo apt install socat` (or `brew install socat`) and re-run."
        );
        return;
    }

    let pty = start_pty_pair();
    let echo = spawn_echo_thread(&pty.b_path);

    // Spawn the real binary with an allowlist override so /tmp/... resolves.
    let bin = env!("CARGO_BIN_EXE_mcp-serial-rs");
    let pid = std::process::id();
    let allowlist = format!("/tmp/mcp-loopback-{pid}-*");
    let mut child = Command::new(bin)
        .env("MCP_SERIAL_ALLOWLIST", &allowlist)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp-serial-rs binary");

    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let rx = spawn_response_pump(stdout);

    // 1. Initialize handshake — confirms the binary is up and routing works.
    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    );
    let init = next_response(&rx);
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["name"], "mcp-serial-rs");

    // 2. Open the manager-side PTY via the allowlist override.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "serial.open",
            "params": {"port": pty.a_path, "baud": 115200, "timeout_ms": 1000}
        }),
    );
    let open = next_response(&rx);
    assert!(
        open["error"].is_null(),
        "open failed (allowlist not honored?): {open}"
    );
    let session_id = open["result"]["session_id"]
        .as_str()
        .expect("session_id string")
        .to_string();
    assert_eq!(session_id.len(), 16, "16-char hex session id");

    // 3. Write a marker that the echo thread will bounce back.
    let payload = "echo-marker-7f3a\n";
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "serial.write",
            "params": {"session_id": &session_id, "data": payload}
        }),
    );
    let write_resp = next_response(&rx);
    assert_eq!(
        write_resp["result"]["bytes_written"], payload.len() as u64,
        "write response: {write_resp}"
    );

    // 4. read_until the payload — echo thread should bounce it back to us.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "serial.read_until",
            "params": {
                "session_id": &session_id,
                "pattern": "echo-marker-7f3a",
                "timeout_ms": 2000,
            }
        }),
    );
    let ru = next_response(&rx);
    let result = &ru["result"];
    assert!(result["error"].is_null());
    assert_eq!(
        result["matched"], true,
        "round-trip failed; got: {ru}"
    );
    let data = result["data"].as_str().expect("data string in response");
    assert!(
        data.contains("echo-marker-7f3a"),
        "echoed data missing from buffer: {data:?}"
    );

    // 5. Close — releases the session, drops the manager-side PTY end.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "serial.close",
            "params": {"session_id": &session_id}
        }),
    );
    let close = next_response(&rx);
    assert_eq!(close["result"]["ok"], true);

    // Shutdown: EOF stdin, reap binary, then drop the PTY (kills socat,
    // which causes the echo thread's read to return Ok(0) and exit).
    drop(stdin);
    let _ = child.wait();
    drop(pty);
    let _ = echo.join();
}
