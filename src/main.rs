// SPDX-License-Identifier: MIT OR Apache-2.0

//! mcp-serial-rs entry point.
//!
//! Bootstraps the tokio runtime, configures `tracing` for stderr-only
//! output (stdout is reserved for MCP messages), loads the optional
//! device-profile TOML, opens the always-on JSONL audit log (refusing to
//! start without it), then hands stdio to the rmcp [`McpServer`] which
//! owns dispatch from that point on.

#![deny(clippy::all)]

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::transport;
use rmcp::ServiceExt;
use tracing::{error, info, warn};

use mcp_serial_rs::config;
use mcp_serial_rs::errors::SerialError;
use mcp_serial_rs::mcp::McpServer;
use mcp_serial_rs::serial::journal::JournalWriter;
use mcp_serial_rs::serial::manager::{SessionManager, TokioSerialBackend};

/// Load device profiles. They are optional, so a missing file or config
/// directory means an empty list. A file that exists but cannot be read or
/// parsed is an error: continuing would silently drop its `privileged` and
/// command-policy settings.
fn load_profiles() -> Result<Vec<config::DeviceProfile>, SerialError> {
    if config::legacy_devices_file_ignored() {
        warn!(
            file = config::LEGACY_DEVICES_FILE,
            "ignoring a device-profile file in the working directory; move it to the user \
             config directory or set MCP_SERIAL_DEVICES"
        );
    }
    let Some(path) = config::devices_path() else {
        warn!("no user config directory found; running without device profiles");
        return Ok(Vec::new());
    };
    if config::is_group_or_world_writable(&path) {
        warn!(
            path = %path.display(),
            "device-profile file is writable by other users, who could change its policies"
        );
    }
    let profiles = config::load_devices(&path).inspect_err(|e| {
        error!(
            error = %e,
            path = %path.display(),
            "refusing to start: fix or remove this device-profile file"
        );
    })?;
    info!(
        count = profiles.len(),
        path = %path.display(),
        "loaded device profiles"
    );
    Ok(profiles)
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let audit_log = match handle_cli_args(
        std::env::args().skip(1),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )? {
        CliAction::Exit(0) => return Ok(()),
        CliAction::Exit(code) => std::process::exit(code),
        CliAction::Serve { audit_log } => audit_log,
    };

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

    let Ok(profiles) = load_profiles() else {
        std::process::exit(1);
    };

    // Open the audit log before serving anything. It fails closed (ADR
    // 0003): without it the server must not touch a serial port, so an
    // open failure is fatal. `eprintln!` keeps the reason visible whatever
    // `RUST_LOG` says; stderr never carries MCP frames.
    let journal_path = audit_log.unwrap_or_else(config::journal_path);
    let journal = match JournalWriter::open(&journal_path).await {
        Ok(writer) => {
            info!(path = %journal_path.display(), "audit log opened");
            Arc::new(writer)
        }
        Err(e) => {
            eprintln!(
                "{}: refusing to start: cannot open audit log {}: {e}; pass --audit-log <path> \
                 or set {} to a file in a writable directory",
                env!("CARGO_PKG_NAME"),
                journal_path.display(),
                config::JOURNAL_ENV,
            );
            std::process::exit(1);
        }
    };

    let sessions = Arc::new(SessionManager::new(TokioSerialBackend));
    let server = McpServer::new(sessions, Arc::new(profiles), Some(journal));

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

/// What `main` does after parsing the command line.
#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    /// Print-only flags ran, or the arguments were invalid: exit with this code.
    Exit(i32),
    /// Start the MCP server, with an audit-log path from `--audit-log` if given.
    Serve { audit_log: Option<PathBuf> },
}

fn handle_cli_args<I, W, E>(args: I, stdout: &mut W, stderr: &mut E) -> std::io::Result<CliAction>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
    W: std::io::Write,
    E: std::io::Write,
{
    let mut saw_info = false;
    let mut saw_unknown = false;
    let mut audit_log = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_ref() {
            "--version" | "-V" => {
                writeln!(
                    stdout,
                    "{} {}",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                )?;
                saw_info = true;
            }
            "--help" | "-h" => {
                writeln!(
                    stdout,
                    "{} {}\n\nUsage: {} [--audit-log <path>] [--version|--help]\n\nWith no flags, starts the MCP stdio server.\n\nDevice profiles are optional. They are read from MCP_SERIAL_DEVICES, else\n$XDG_CONFIG_HOME/mcp-serial-rs/devices.toml (by default\n~/.config/mcp-serial-rs/devices.toml). The working directory is not searched.\n\nThe audit log is required. It is written to --audit-log <path>, else\nMCP_SERIAL_JOURNAL, else $XDG_STATE_HOME/mcp-serial-rs/audit.jsonl (by default\n~/.local/state/mcp-serial-rs/audit.jsonl). If it cannot be opened the server\nexits; if a write fails later, serial tools other than serial.close are refused.",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION"),
                    env!("CARGO_PKG_NAME"),
                )?;
                saw_info = true;
            }
            "--audit-log" => match args.next() {
                Some(path) if !path.as_ref().is_empty() => {
                    audit_log = Some(PathBuf::from(path.as_ref()));
                }
                _ => {
                    writeln!(stderr, "--audit-log requires a path")?;
                    saw_unknown = true;
                }
            },
            other => match other.strip_prefix("--audit-log=") {
                Some(path) if !path.is_empty() => audit_log = Some(PathBuf::from(path)),
                Some(_) => {
                    writeln!(stderr, "--audit-log requires a path")?;
                    saw_unknown = true;
                }
                None => {
                    writeln!(stderr, "unknown argument: {other}")?;
                    saw_unknown = true;
                }
            },
        }
    }
    // An unknown argument is a usage error (exit 2) regardless of position and
    // must not be cleared by a later --help/--version. Recognized print-only
    // flags exit 0; otherwise the caller starts the MCP server.
    Ok(if saw_unknown {
        CliAction::Exit(2)
    } else if saw_info {
        CliAction::Exit(0)
    } else {
        CliAction::Serve { audit_log }
    })
}

#[cfg(test)]
mod tests {
    use super::{handle_cli_args, CliAction};
    use std::path::PathBuf;

    #[test]
    fn version_flag_prints_and_exits() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code = handle_cli_args(["--version"], &mut stdout, &mut stderr).unwrap();
        assert_eq!(exit_code, CliAction::Exit(0));
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
        assert_eq!(exit_code, CliAction::Serve { audit_log: None });
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn unknown_arg_reports_usage_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code =
            handle_cli_args(["--definitely-invalid"], &mut stdout, &mut stderr).unwrap();
        assert_eq!(exit_code, CliAction::Exit(2));
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "unknown argument: --definitely-invalid\n"
        );
    }

    #[test]
    fn unknown_before_help_stays_usage_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code = handle_cli_args(["--bad", "--help"], &mut stdout, &mut stderr).unwrap();
        assert_eq!(
            exit_code,
            CliAction::Exit(2),
            "unknown arg must not be cleared by --help"
        );
    }

    #[test]
    fn unknown_before_version_stays_usage_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code = handle_cli_args(["--bad", "--version"], &mut stdout, &mut stderr).unwrap();
        assert_eq!(exit_code, CliAction::Exit(2));
    }

    #[test]
    fn help_then_unknown_is_usage_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code = handle_cli_args(["--help", "--bad"], &mut stdout, &mut stderr).unwrap();
        assert_eq!(
            exit_code,
            CliAction::Exit(2),
            "later unknown arg must force exit 2"
        );
    }

    #[test]
    fn audit_log_flag_takes_a_path_in_either_form() {
        for args in [
            vec!["--audit-log", "/run/audit.jsonl"],
            vec!["--audit-log=/run/audit.jsonl"],
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let action = handle_cli_args(args.clone(), &mut stdout, &mut stderr).unwrap();
            assert_eq!(
                action,
                CliAction::Serve {
                    audit_log: Some(PathBuf::from("/run/audit.jsonl"))
                },
                "{args:?}"
            );
            assert!(stdout.is_empty() && stderr.is_empty());
        }
    }

    #[test]
    fn audit_log_flag_without_path_is_usage_error() {
        for args in [vec!["--audit-log"], vec!["--audit-log="]] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let action = handle_cli_args(args.clone(), &mut stdout, &mut stderr).unwrap();
            assert_eq!(action, CliAction::Exit(2), "{args:?}");
            assert_eq!(
                String::from_utf8(stderr).unwrap(),
                "--audit-log requires a path\n"
            );
        }
    }
}
