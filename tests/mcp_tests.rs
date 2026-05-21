//! In-process integration tests for the rmcp adapter layer.
//!
//! These tests drive `crate::mcp::McpServer` over an in-memory
//! [`tokio::io::duplex`] pipe, send raw line-delimited JSON-RPC, and
//! assert on the responses. They exist to anchor parity of the rmcp
//! migration (spec §Migration Sequence step 3) before the binary entry
//! point switches over.
//!
//! Tests cover:
//!
//! * `initialize` returns a usable server-info / capabilities object.
//! * `tools/list` advertises the dotted `serial.list_ports` name with an
//!   input schema.
//! * `tools/call serial.list_ports` returns a structured tool result whose
//!   `structuredContent` matches the public list-ports array shape.
//!
//! The legacy hand-rolled stack tests in `tests/protocol_tests.rs` remain
//! authoritative for the binary's MCP wire surface; this file is additive.

use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use mcp_serial_rs::mcp::McpServer;
use mcp_serial_rs::serial::manager::{SessionManager, TokioSerialBackend};

/// Build a fresh server with empty device profiles and no journal — the
/// list_ports test path does not exercise either.
fn fresh_server() -> McpServer<TokioSerialBackend> {
    let sessions = Arc::new(SessionManager::new(TokioSerialBackend));
    let profiles = Arc::new(Vec::new());
    McpServer::new(sessions, profiles, None)
}

/// Drive the rmcp server over a duplex pipe, send a sequence of MCP
/// request lines, read one JSON response per line. Returns responses in
/// order. The `initialized` notification, if needed, must be included in
/// `requests` by the caller — this helper makes no assumptions about
/// handshake order.
async fn roundtrip(requests: &[Value]) -> Vec<Value> {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);

    // Spawn the rmcp server on the server end of the duplex.
    let server = fresh_server();
    let server_task = tokio::spawn(async move {
        let svc = server.serve(server_io).await.expect("serve start");
        // Run until the client side closes the pipe.
        let _ = svc.waiting().await;
    });

    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut reader = BufReader::new(client_read);

    for req in requests {
        let mut line = serde_json::to_vec(req).expect("encode request");
        line.push(b'\n');
        client_write.write_all(&line).await.expect("write request");
    }
    client_write.flush().await.expect("flush");

    // Count how many responses we expect: every request that has an `id`
    // (i.e. not a notification) gets exactly one response back.
    let expected: usize = requests
        .iter()
        .filter(|r| r.get("id").is_some())
        .count();

    let mut responses = Vec::with_capacity(expected);
    for _ in 0..expected {
        let mut buf = String::new();
        // Cap each read so a regression cannot wedge the suite.
        let n = timeout(Duration::from_secs(5), reader.read_line(&mut buf))
            .await
            .expect("response timeout")
            .expect("read line");
        assert!(n > 0, "EOF before all responses arrived");
        let value: Value = serde_json::from_str(buf.trim_end()).expect("response JSON");
        responses.push(value);
    }

    // Drop client side; the spawned server task will exit on EOF.
    drop(client_write);
    drop(reader);
    let _ = timeout(Duration::from_secs(2), server_task).await;

    responses
}

fn init_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "mcp-tests", "version": "0.0.1"},
        }
    })
}

fn initialized_notification() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
}

#[tokio::test]
async fn initialize_advertises_tools_capability_and_server_info() {
    let responses = roundtrip(&[init_request()]).await;
    assert_eq!(responses.len(), 1);
    let result = responses[0].get("result").expect("initialize result");

    assert_eq!(
        result["serverInfo"]["name"], "mcp-serial-rs",
        "serverInfo.name should be the crate name",
    );
    assert!(
        result["capabilities"].get("tools").is_some(),
        "capabilities.tools must be present so clients know tools/list is supported; got {result:?}",
    );
}

#[tokio::test]
async fn tools_list_contains_serial_list_ports_with_input_schema() {
    let responses = roundtrip(&[
        init_request(),
        initialized_notification(),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ])
    .await;
    assert_eq!(responses.len(), 2);
    let result = responses[1].get("result").expect("tools/list result");
    let tools = result["tools"].as_array().expect("tools array");

    let lp = tools
        .iter()
        .find(|t| t["name"] == "serial.list_ports")
        .expect("serial.list_ports must be listed under its dotted name");

    assert!(
        lp.get("inputSchema").is_some(),
        "rmcp must generate an input schema for serial.list_ports (even for an empty-param tool)",
    );
    // Spec §Structured Result Requirements: output schemas are deferred to
    // a later migration pass. Confirm we haven't accidentally emitted one.
    assert!(
        lp.get("outputSchema").is_none(),
        "outputSchema should NOT be set yet (deferred until behavioral parity is in)",
    );
}

#[tokio::test]
async fn call_serial_list_ports_returns_structured_array() {
    let responses = roundtrip(&[
        init_request(),
        initialized_notification(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "serial.list_ports", "arguments": {}},
        }),
    ])
    .await;
    assert_eq!(responses.len(), 2);
    let result = responses[1].get("result").expect("tools/call result");

    let structured = result
        .get("structuredContent")
        .expect("rmcp adapter must emit structuredContent for object results");
    // MCP defines structuredContent as a JSON object, not a bare array.
    // serial.list_ports therefore wraps its result as `{"ports": [...]}`.
    assert!(
        structured.is_object(),
        "structuredContent must be an object per MCP 2025-11-25 schema, got: {structured}",
    );
    let arr = structured
        .get("ports")
        .and_then(Value::as_array)
        .expect("structuredContent.ports must be the array of descriptors");

    // The CI host may have zero matching ports; that is a valid result.
    // What we assert is: every element matches the public list_ports
    // descriptor shape so clients can parse without re-jsonifying.
    for entry in arr {
        assert!(entry.get("port").is_some(), "missing `port` in {entry}");
        // vid/pid/serial/device/description may be `null` but the keys
        // must be present so the structuredContent matches the README's
        // documented contract.
        for key in ["vid", "pid", "serial", "device", "description"] {
            assert!(entry.get(key).is_some(), "missing `{key}` in {entry}");
        }
    }

    // `is_error` must be `false` (or absent) for a successful call.
    assert_ne!(
        result.get("isError"),
        Some(&json!(true)),
        "successful tools/call must not set isError=true",
    );
}
