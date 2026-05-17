//! mcp-serial-rs entry point.
//!
//! Bootstraps the tokio runtime, initialises `tracing` (to stderr only —
//! stdout is reserved for JSON-RPC), and runs the stdio dispatch loop.
//! Routing is delegated to [`mcp_serial_rs::tools::dispatch`].

#![deny(clippy::all)]

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info, warn};

use mcp_serial_rs::config;
use mcp_serial_rs::protocol::{self, Response};
use mcp_serial_rs::serial::manager::{SessionManager, TokioSerialBackend};
use mcp_serial_rs::tools::{self, State};

#[tokio::main]
async fn main() -> std::io::Result<()> {
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
            // Profiles are optional — log and continue with an empty list so
            // serial.list_ports / serial.open(port=...) still work.
            warn!(error = %e, path = %devices_path.display(), "failed to load device profiles");
            Vec::new()
        }
    };

    let state = State::new(
        Arc::new(SessionManager::new(TokioSerialBackend)),
        Arc::new(profiles),
    );

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Value>(&line) {
            // Phase 1: valid JSON. Now try to interpret it as a Request.
            Ok(value) => match serde_json::from_value(value.clone()) {
                Ok(req) => tools::dispatch(&state, req).await,
                Err(e) => {
                    warn!(error = %e, "invalid request shape");
                    Some(Response::failure(
                        extract_id(&value),
                        protocol::Error::new(
                            protocol::INVALID_REQUEST,
                            format!("invalid JSON-RPC request: {e}"),
                        ),
                    ))
                }
            },
            // Phase 0: not valid JSON at all.
            Err(e) => {
                warn!(error = %e, "parse error");
                Some(Response::failure(
                    Value::Null,
                    protocol::Error::new(protocol::PARSE_ERROR, format!("invalid JSON: {e}")),
                ))
            }
        };

        let Some(response) = response else {
            continue;
        };

        let mut bytes = match serde_json::to_vec(&response) {
            Ok(b) => b,
            Err(e) => {
                error!(error = %e, "failed to serialise response");
                continue;
            }
        };
        bytes.push(b'\n');
        stdout.write_all(&bytes).await?;
        stdout.flush().await?;
    }

    Ok(())
}

/// Best-effort id extraction from a Value whose JSON-RPC shape failed validation.
/// Per spec, `id` must be string / number / null; anything else falls back to null.
fn extract_id(v: &Value) -> Value {
    match v.get("id") {
        Some(id) if id.is_string() || id.is_number() || id.is_null() => id.clone(),
        _ => Value::Null,
    }
}
