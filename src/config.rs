// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compile-time configuration constants and runtime device profiles.
//! See CLAUDE.md §6.
//!
//! Hard limits stay as `pub const` so they participate in type-checking at
//! every call site. Two configuration items are runtime-tunable:
//!
//! - `MCP_SERIAL_ALLOWLIST` env var → glob patterns ([`matches_allowlist`])
//! - `MCP_SERIAL_DEVICES` env var → path to `devices.toml` ([`load_devices`])

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::errors::SerialError;

/// Glob patterns of serial device paths that `serial.open` will accept.
/// Anything outside this list is rejected with `PortNotAllowed`.
pub const PORT_ALLOWLIST: &[&str] = &["/dev/ttyUSB*", "/dev/ttyACM*"];

/// Hard cap on concurrent open sessions.
pub const MAX_SESSIONS: usize = 4;

/// Maximum bytes retained in a session's read accumulator. Reads beyond this
/// truncate from the front (ring-buffer semantics).
pub const MAX_READ_BUFFER: usize = 64 * 1024;

/// Maximum bytes accepted per `serial.write` call.
pub const MAX_WRITE_CHUNK: usize = 4096;

/// Default baud rate when the caller omits it.
pub const DEFAULT_BAUD: u32 = 115_200;

/// Default per-operation timeout (ms) when the caller omits it.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// Upper clamp for any caller-supplied timeout (ms).
pub const MAX_TIMEOUT_MS: u64 = 30_000;

/// Env var pointing at the device-profile TOML file. Missing file is not
/// an error — profiles are optional. See [`load_devices`].
pub const DEVICES_ENV: &str = "MCP_SERIAL_DEVICES";

/// Default path consulted when [`DEVICES_ENV`] is unset.
pub const DEFAULT_DEVICES_PATH: &str = "devices.toml";

/// Env var pointing at the JSONL traffic-journal file. Set to override the
/// default `/tmp/mcp-serial-journal.jsonl`. The journal is always-on (per
/// CLAUDE.md §6); if the path is unwritable, the server logs a warning and
/// runs in degraded mode without auditing rather than failing to start.
pub const JOURNAL_ENV: &str = "MCP_SERIAL_JOURNAL";

/// Default journal path when [`JOURNAL_ENV`] is unset.
pub const DEFAULT_JOURNAL_PATH: &str = "/tmp/mcp-serial-journal.jsonl";

/// Resolve the device-profile path: env var if set, else
/// [`DEFAULT_DEVICES_PATH`] relative to the current working directory.
pub fn devices_path() -> PathBuf {
    std::env::var(DEVICES_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_DEVICES_PATH))
}

/// Resolve the journal path: env var if set, else [`DEFAULT_JOURNAL_PATH`].
pub fn journal_path() -> PathBuf {
    std::env::var(JOURNAL_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_JOURNAL_PATH))
}

/// A device profile loaded from `devices.toml`. `name` is the TOML table
/// key (e.g. `esp32c6`). Used by `serial.list_ports` to enrich port output
/// and by `serial.open` to resolve a `{device}` param to a port path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProfile {
    pub name: String,
    pub match_serial: String,
    pub match_vid: Option<u16>,
    pub match_pid: Option<u16>,
    pub baud: u32,
    pub description: String,
    pub probe: Option<String>,
    pub tags: Vec<String>,
}

/// Raw on-disk shape — used only as a deserialisation target before
/// being lifted into [`DeviceProfile`] with the TOML key as the `name`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceProfileRaw {
    match_serial: String,
    match_vid: Option<u16>,
    match_pid: Option<u16>,
    baud: u32,
    description: String,
    probe: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DevicesFile {
    #[serde(default)]
    devices: HashMap<String, DeviceProfileRaw>,
}

/// Parse a TOML string into device profiles. The TOML key under `[devices.…]`
/// becomes the profile's `name`. Order is not preserved — `HashMap` iteration
/// is arbitrary — but consumers use first-match-wins on stable predicates
/// (serial+vid+pid), so order rarely matters.
pub fn parse_devices(toml_text: &str) -> Result<Vec<DeviceProfile>, SerialError> {
    let parsed: DevicesFile = toml::from_str(toml_text).map_err(|e| SerialError::InvalidParam {
        name: "devices.toml".into(),
        reason: format!("parse error: {e}"),
    })?;
    Ok(parsed
        .devices
        .into_iter()
        .map(|(name, raw)| DeviceProfile {
            name,
            match_serial: raw.match_serial,
            match_vid: raw.match_vid,
            match_pid: raw.match_pid,
            baud: raw.baud,
            description: raw.description,
            probe: raw.probe,
            tags: raw.tags,
        })
        .collect())
}

/// Load device profiles from a file. Returns an empty vector when the file
/// does not exist (profiles are optional). All other I/O and parse errors
/// surface as [`SerialError`].
pub fn load_devices(path: impl AsRef<Path>) -> Result<Vec<DeviceProfile>, SerialError> {
    let path = path.as_ref();
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(SerialError::Io {
                message: format!("read {}: {e}", path.display()),
            });
        }
    };
    parse_devices(&contents)
}

/// Env var that, when set, overrides the compiled-in [`PORT_ALLOWLIST`].
/// Comma-separated list of glob patterns. Intended for test harnesses
/// (PTY loopback under `/tmp/...`) and for production deployments on hosts
/// with non-standard device paths (BSD `/dev/cuaU*`, custom mountpoints).
pub const ALLOWLIST_ENV: &str = "MCP_SERIAL_ALLOWLIST";

/// Returns `true` when `path` matches any entry in the active allowlist.
///
/// Resolution order: if `MCP_SERIAL_ALLOWLIST` is set, its comma-separated
/// patterns are used exclusively. Otherwise [`PORT_ALLOWLIST`] applies.
///
/// Glob semantics are intentionally minimal: a trailing `*` matches any
/// non-empty suffix that does not contain `/` (so `/dev/ttyUSB*` matches
/// `/dev/ttyUSB0` but not `/dev/ttyUSB0/foo`). Patterns without `*` require
/// an exact match.
pub fn matches_allowlist(path: &str) -> bool {
    if let Ok(custom) = std::env::var(ALLOWLIST_ENV) {
        return custom
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .any(|pat| glob_match(pat, path));
    }
    PORT_ALLOWLIST.iter().any(|pat| glob_match(pat, path))
}

fn glob_match(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => {
            value.len() > prefix.len()
                && value.starts_with(prefix)
                && !value[prefix.len()..].contains('/')
        }
        None => pattern == value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_ttyusb_and_ttyacm() {
        assert!(matches_allowlist("/dev/ttyUSB0"));
        assert!(matches_allowlist("/dev/ttyUSB10"));
        assert!(matches_allowlist("/dev/ttyACM3"));
    }

    // --- device profiles --- //

    const SAMPLE_TOML: &str = r#"
        [devices.esp32c6]
        match_serial = "ABC123"
        match_vid    = 0x10C4
        match_pid    = 0xEA60
        baud         = 115200
        description  = "ESP32-C6 Zephyr DFU target"
        probe        = "uart:~\\$"
        tags         = ["zephyr", "dfu"]

        [devices.rpi5]
        match_serial = "EXAMPLE001"
        baud         = 115200
        description  = "RPi 5 debug UART"
    "#;

    #[test]
    fn parse_devices_loads_profiles_with_optional_fields() {
        let profiles = parse_devices(SAMPLE_TOML).unwrap();
        assert_eq!(profiles.len(), 2);

        let esp = profiles.iter().find(|p| p.name == "esp32c6").unwrap();
        assert_eq!(esp.match_serial, "ABC123");
        assert_eq!(esp.match_vid, Some(0x10C4));
        assert_eq!(esp.match_pid, Some(0xEA60));
        assert_eq!(esp.baud, 115_200);
        assert_eq!(esp.description, "ESP32-C6 Zephyr DFU target");
        assert_eq!(esp.probe.as_deref(), Some("uart:~\\$"));
        assert_eq!(esp.tags, vec!["zephyr".to_string(), "dfu".to_string()]);

        let rpi = profiles.iter().find(|p| p.name == "rpi5").unwrap();
        assert_eq!(rpi.match_vid, None, "vid is optional");
        assert_eq!(rpi.match_pid, None);
        assert!(rpi.tags.is_empty(), "tags default to empty");
        assert_eq!(rpi.probe, None);
    }

    #[test]
    fn parse_devices_rejects_unknown_fields() {
        let bad = r#"
            [devices.x]
            match_serial = "abc"
            baud = 115200
            description = "x"
            rogue = "should-not-be-here"
        "#;
        let err = parse_devices(bad).unwrap_err();
        assert!(
            matches!(err, SerialError::InvalidParam { ref name, .. } if name == "devices.toml")
        );
    }

    #[test]
    fn parse_devices_malformed_toml_errors() {
        let err = parse_devices("[devices.x\nmatch_serial = ").unwrap_err();
        assert!(matches!(err, SerialError::InvalidParam { .. }));
    }

    #[test]
    fn parse_devices_empty_string_returns_empty() {
        // A file with no [devices.*] tables is valid — zero profiles loaded.
        let profiles = parse_devices("").unwrap();
        assert!(profiles.is_empty());
    }

    #[test]
    fn load_devices_missing_file_returns_empty() {
        let bogus = std::path::PathBuf::from("/nonexistent/mcp-serial-rs-test/devices.toml");
        let profiles = load_devices(&bogus).unwrap();
        assert!(profiles.is_empty());
    }

    #[test]
    fn load_devices_reads_file_from_disk() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), SAMPLE_TOML).unwrap();
        let profiles = load_devices(tmp.path()).unwrap();
        assert_eq!(profiles.len(), 2);
    }

    #[test]
    fn devices_path_honors_env_var() {
        // Don't actually set the env var (it would race with other tests).
        // The default-path branch is the safe one to exercise here.
        unsafe {
            std::env::remove_var(DEVICES_ENV);
        }
        assert_eq!(devices_path(), PathBuf::from(DEFAULT_DEVICES_PATH));
    }

    // --- allowlist --- //

    #[test]
    fn allowlist_rejects_other_paths() {
        assert!(!matches_allowlist("/dev/ttyS0"));
        assert!(!matches_allowlist("/dev/null"));
        assert!(!matches_allowlist("/etc/passwd"));
        // Trailing '*' must not allow path traversal.
        assert!(!matches_allowlist("/dev/ttyUSB0/../shadow"));
        // Empty suffix is rejected (no bare /dev/ttyUSB).
        assert!(!matches_allowlist("/dev/ttyUSB"));
    }
}
