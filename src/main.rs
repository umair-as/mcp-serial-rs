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

use rmcp::transport;
use rmcp::ServiceExt;
use tracing::{info, warn};

use mcp_serial_rs::config;
use mcp_serial_rs::mcp::McpServer;
use mcp_serial_rs::serial::journal::JournalWriter;
use mcp_serial_rs::serial::manager::{SessionManager, TokioSerialBackend};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    if let Some(code) = handle_cli_args(
        std::env::args().skip(1),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )? {
        if code == 0 {
            return Ok(());
        }
        std::process::exit(code);
    }

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
    // in degraded mode (no journaling) for missing paths, permissions
    // problems, or unsafe journal targets. Per-call journal I/O is
    // deadline-bounded inside `McpServer::call_tool`.
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

fn handle_cli_args<I, W, E>(args: I, stdout: &mut W, stderr: &mut E) -> std::io::Result<Option<i32>>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
    W: std::io::Write,
    E: std::io::Write,
{
    let mut exit_code = None;
    for arg in args {
        match arg.as_ref() {
            "--version" | "-V" => {
                writeln!(
                    stdout,
                    "{} {}",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                )?;
                exit_code = Some(0);
            }
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "{} {}\n\nUsage: {} [--version|--help]\n\nWith no flags, starts the MCP stdio server.",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION"),
                    env!("CARGO_PKG_NAME"),
                )?;
                exit_code = Some(0);
            }
            other => {
                writeln!(stderr, "unknown argument: {other}")?;
                exit_code = Some(2);
            }
        }
    }
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::handle_cli_args;

    #[test]
    fn version_flag_prints_and_exits() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code = handle_cli_args(["--version"], &mut stdout, &mut stderr).unwrap();
        assert_eq!(exit_code, Some(0));
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("{} {}\n", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn no_args_starts_server_path() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code =
            handle_cli_args(std::iter::empty::<&str>(), &mut stdout, &mut stderr).unwrap();
        assert_eq!(exit_code, None);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn unknown_arg_reports_usage_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code =
            handle_cli_args(["--definitely-invalid"], &mut stdout, &mut stderr).unwrap();
        assert_eq!(exit_code, Some(2));
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "unknown argument: --definitely-invalid\n"
        );
    }
}
