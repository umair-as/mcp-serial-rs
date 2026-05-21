// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the JSON-RPC 2.0 wire layer.
//!
//! Spawns the compiled binary as a subprocess, feeds JSON-RPC lines on stdin,
//! and asserts the responses on stdout. `CARGO_BIN_EXE_mcp-serial-rs` is set
//! by Cargo for integration tests.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

/// Send the three handshake lines, close stdin, collect stdout responses.
/// Returns one `Value` per stdout line.
fn run_handshake(lines: &[&str]) -> Vec<Value> {
    let bin = env!("CARGO_BIN_EXE_mcp-serial-rs");
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn binary");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in lines {
            writeln!(stdin, "{line}").expect("write stdin");
        }
    }
    // Drop stdin to signal EOF.
    drop(child.stdin.take());

    let stdout = child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);
    let responses: Vec<Value> = reader
        .lines()
        .map(|l| l.expect("read line"))
        .map(|l| serde_json::from_str(&l).expect("valid json response"))
        .collect();

    // Bound the wait — the process should exit promptly on EOF.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("binary did not exit within timeout");
            }
            Err(e) => panic!("wait failed: {e}"),
        }
    }

    responses
}

#[test]
fn three_message_handshake() {
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let tools_list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;

    let responses = run_handshake(&[initialize, initialized, tools_list]);

    // Notification produces no response — exactly two responses expected.
    assert_eq!(responses.len(), 2, "got {responses:?}");

    let init = &responses[0];
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["name"], "mcp-serial-rs");
    assert!(init["result"]["version"].is_string());
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["error"].is_null());

    let list = &responses[1];
    assert_eq!(list["jsonrpc"], "2.0");
    assert_eq!(list["id"], 2);
    let tools = list["result"].as_array().expect("array");
    assert_eq!(tools.len(), 7);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "serial.list_ports",
        "serial.open",
        "serial.write",
        "serial.read",
        "serial.read_until",
        "serial.exec",
        "serial.close",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    for tool in tools {
        assert!(tool["description"].is_string(), "tool missing description: {tool}");
        assert_eq!(tool["inputSchema"]["type"], "object");
    }
}

#[test]
fn parse_error_yields_null_id_response() {
    let responses = run_handshake(&["{not valid json"]);
    assert_eq!(responses.len(), 1);
    let r = &responses[0];
    assert_eq!(r["jsonrpc"], "2.0");
    assert!(r["id"].is_null());
    assert_eq!(r["error"]["code"], -32700);
}

#[test]
fn missing_method_field_is_invalid_request_not_parse_error() {
    // Valid JSON but missing the required `method` field — must be -32600,
    // and the id should echo back even though shape validation failed.
    let responses = run_handshake(&[r#"{"jsonrpc":"2.0","id":7,"params":{}}"#]);
    assert_eq!(responses.len(), 1);
    let r = &responses[0];
    assert_eq!(r["jsonrpc"], "2.0");
    assert_eq!(r["id"], 7);
    assert_eq!(r["error"]["code"], -32600);
}

#[test]
fn missing_jsonrpc_field_is_invalid_request() {
    let responses = run_handshake(&[r#"{"id":8,"method":"initialize","params":{}}"#]);
    assert_eq!(responses.len(), 1);
    let r = &responses[0];
    assert_eq!(r["jsonrpc"], "2.0");
    assert_eq!(r["id"], 8);
    assert_eq!(r["error"]["code"], -32600);
}

#[test]
fn id_less_request_produces_no_response_over_wire() {
    // Notification (no id) for a normal method must produce zero stdout
    // lines. Pair it with a real request to confirm the loop is still alive.
    let notif = r#"{"jsonrpc":"2.0","method":"initialize","params":{}}"#;
    let real = r#"{"jsonrpc":"2.0","id":42,"method":"initialize","params":{}}"#;
    let responses = run_handshake(&[notif, real]);
    assert_eq!(responses.len(), 1, "got responses: {responses:?}");
    assert_eq!(responses[0]["id"], 42);
}

#[test]
fn serial_read_until_routes_to_handler_over_wire() {
    // Two wire-level shots at the new dispatch arm. We can't easily open a
    // real port here (Task 7 will do that via PTY), but the routing and
    // error-response shape are exactly what would break if the dispatcher
    // wiring regressed. The full success-path {data, matched} shape is
    // covered by the manager + dispatcher unit tests.
    let missing_pattern =
        r#"{"jsonrpc":"2.0","id":101,"method":"serial.read_until","params":{"session_id":"x"}}"#;
    let unknown_session =
        r#"{"jsonrpc":"2.0","id":102,"method":"serial.read_until","params":{"session_id":"deadbeefdeadbeef","pattern":"ready","timeout_ms":5}}"#;

    let responses = run_handshake(&[missing_pattern, unknown_session]);
    assert_eq!(responses.len(), 2, "got: {responses:?}");

    // Missing required `pattern` → -32602 INVALID_PARAMS.
    assert_eq!(responses[0]["id"], 101);
    assert_eq!(responses[0]["error"]["code"], -32602);
    assert!(responses[0]["result"].is_null());

    // Unknown session → SerialError::SessionNotFound → -32003.
    assert_eq!(responses[1]["id"], 102);
    assert_eq!(responses[1]["error"]["code"], -32003);
    assert_eq!(responses[1]["error"]["data"]["session_id"], "deadbeefdeadbeef");
}

#[test]
fn wrong_jsonrpc_version_is_invalid_request() {
    let responses =
        run_handshake(&[r#"{"jsonrpc":"1.0","id":9,"method":"initialize","params":{}}"#]);
    assert_eq!(responses.len(), 1);
    let r = &responses[0];
    assert_eq!(r["id"], 9);
    assert_eq!(r["error"]["code"], -32600);
}
