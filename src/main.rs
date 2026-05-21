// SPDX-License-Identifier: MIT OR Apache-2.0

//! mcp-serial-rs entry point.
//!
//! Bootstraps the tokio runtime, configures `tracing` for stderr-only
//! output (stdout is reserved for MCP messages), loads the optional
//! device-profile TOML and the always-on JSONL audit journal, then
//! hands stdio to the rmcp [`McpServer`] which owns dispatch from
//! that point on.

#![deny(clippy::all)]

use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::transport;
use tracing::{info, warn};

use mcp_serial_rs::config;
use mcp_serial_rs::mcp::McpServer;
use mcp_serial_rs::serial::journal::JournalWriter;
use mcp_serial_rs::serial::manager::{SessionManager, TokioSerialBackend};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // `rmcp::transport::stdio()` returns the OS stdin/stdout pair and
    // does NOT configure any logging — stderr-only `tracing` setup is
    // mandatory here, otherwise crate logs would corrupt the MCP wire.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("mcp_serial_rs=info")),
        )
        .init();

    let devices_path = config::devices_path();
    let profiles = match config::load_devices(&devices_path) {
        Ok(p) => {
            info!(
                count = p.len(),
                path = %devices_path.display(),
                "loaded device profiles"
            );
            p
        }
        Err(e) => {
            // Profiles are optional — log and continue with an empty
            // list so `serial.list_ports` and `serial.open(port=...)`
            // still work.
            warn!(error = %e, path = %devices_path.display(), "failed to load device profiles");
            Vec::new()
        }
    };

    // Open the audit journal. Failure is non-fatal: the server runs
    // in degraded mode (no journaling) so a missing /tmp or
    // permissions problem never blocks tool dispatch. The narrowed
    // tool-call-only journal hook lives inside `McpServer::call_tool`.
    let journal_path = config::journal_path();
    let journal = JournalWriter::try_open_arc(&journal_path).await;

    let sessions = Arc::new(SessionManager::new(TokioSerialBackend));
    let server = McpServer::new(sessions, Arc::new(profiles), journal);

    // Hand stdio to rmcp. `serve()` consumes the server and runs the
    // SDK dispatch loop; `.waiting()` blocks until the peer closes
    // the transport (EOF on stdin) or the service errors out.
    let svc = server
        .serve(transport::stdio())
        .await
        .expect("rmcp serve() must start on stdio");
    let _ = svc.waiting().await;

    Ok(())
}
