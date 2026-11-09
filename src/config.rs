// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compile-time configuration constants and runtime device profiles.
//! See CLAUDE.md ("Config & safety").
//!
//! Hard limits stay as `pub const` so they participate in type-checking at
//! every call site. Two configuration items are runtime-tunable:
//!
//! - `MCP_SERIAL_ALLOWLIST` env var → glob patterns ([`matches_allowlist`])
//! - `MCP_SERIAL_DEVICES` env var → path to `devices.toml` ([`load_devices`])

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::errors::SerialError;
use crate::serial::console::{ConsoleSettings, EchoMode, LineEnding, SemanticPrompt};

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

/// Maximum source length (bytes) of any regex: `read_until` patterns, `exec`
/// `expect`, and command-policy rules.
pub const MAX_PATTERN_LEN: usize = 1024;

/// Compiled-program and lazy-DFA cache limits for every regex. Well below the
/// `regex` crate defaults so one pattern cannot claim tens of MiB.
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;
pub const REGEX_DFA_SIZE_LIMIT: usize = 1 << 20;

/// Env var pointing at the device-profile TOML file. Missing file is not
/// an error — profiles are optional. See [`load_devices`].
pub const DEVICES_ENV: &str = "MCP_SERIAL_DEVICES";

/// File name of a device-profile list in the working directory. It is no
/// longer read: a repository an agent can edit must not be able to supply
/// or strip device policy. The server warns when it finds one.
pub const LEGACY_DEVICES_FILE: &str = "devices.toml";

/// Env var pointing at the JSONL traffic-journal file. Set to override the
/// default user-private state path. The journal is always-on (per
/// CLAUDE.md); if the path is unwritable, the server logs a warning and
/// runs in degraded mode without auditing rather than failing to start.
pub const JOURNAL_ENV: &str = "MCP_SERIAL_JOURNAL";

/// JSON array of server-owned deny regexes applied to every newly opened
/// session. Example: `["\\breboot\\b","\\bmkfs\\b"]`. Invalid JSON or
/// regexes reject the affected `serial.open` before any port I/O.
pub const DENY_PATTERNS_ENV: &str = "MCP_SERIAL_DENY_PATTERNS";

/// Fallback default journal path when neither [`JOURNAL_ENV`] nor
/// `XDG_STATE_HOME` can supply a user-private location.
pub const DEFAULT_JOURNAL_PATH: &str = ".mcp-serial-rs/audit.jsonl";

/// Resolve the device-profile path: [`DEVICES_ENV`] if set, else
/// `devices.toml` under the user's config directory. `None` when no config
/// directory can be determined; profiles are optional, so that is not an
/// error. The working directory is never searched; only a relative
/// [`DEVICES_ENV`] value resolves against it.
pub fn devices_path() -> Option<PathBuf> {
    resolve_devices_path(
        std::env::var_os(DEVICES_ENV),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// An empty [`DEVICES_ENV`] value counts as unset.
fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

fn resolve_devices_path(
    explicit: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(path) = non_empty(explicit) {
        return Some(PathBuf::from(path));
    }
    // The XDG spec says to ignore a relative value. Honouring one here would
    // put the file back under the working directory.
    let config_dir = xdg_config_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })?;
    Some(config_dir.join("mcp-serial-rs").join("devices.toml"))
}

/// True when a `devices.toml` sits in the working directory and is being
/// ignored because [`DEVICES_ENV`] does not point anywhere.
pub fn legacy_devices_file_ignored() -> bool {
    non_empty(std::env::var_os(DEVICES_ENV)).is_none() && Path::new(LEGACY_DEVICES_FILE).is_file()
}

/// True when someone other than the owner can modify the profile file, and
/// so could remove `privileged` or a command policy.
#[cfg(unix)]
pub fn is_group_or_world_writable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o022 != 0)
}

#[cfg(not(unix))]
pub fn is_group_or_world_writable(_path: &Path) -> bool {
    false
}

/// Resolve the journal path: env var if set, else [`DEFAULT_JOURNAL_PATH`].
pub fn journal_path() -> PathBuf {
    if let Ok(path) = std::env::var(JOURNAL_ENV) {
        return PathBuf::from(path);
    }
    if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(state_home)
            .join("mcp-serial-rs")
            .join("audit.jsonl");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("mcp-serial-rs")
            .join("audit.jsonl");
    }
    PathBuf::from(DEFAULT_JOURNAL_PATH)
}

pub fn global_deny_patterns() -> Result<Vec<String>, SerialError> {
    let Ok(value) = std::env::var(DENY_PATTERNS_ENV) else {
        return Ok(Vec::new());
    };
    parse_global_deny_patterns(&value)
}

fn parse_global_deny_patterns(value: &str) -> Result<Vec<String>, SerialError> {
    serde_json::from_str(value).map_err(|error| SerialError::InvalidParam {
        name: DENY_PATTERNS_ENV.into(),
        reason: format!("must be a JSON array of regex strings: {error}"),
    })
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
    /// Profile-owned defaults for command submission, echo handling, and
    /// semantic-prompt parsing. All fields default to raw-compatible values.
    pub console: ConsoleSettings,
    /// Server-owned deny and allow regexes for complete `serial.exec`
    /// commands. They are compiled when the session opens.
    pub deny_patterns: Vec<String>,
    pub allow_patterns: Vec<String>,
    /// When true, sessions opened via this profile default to a `confirm`
    /// write policy (each write is approved first; see `WritePolicy`). Set it on
    /// boards where the operator wants every write approved. A caller may
    /// still escalate to `deny` at `serial.open`, but cannot downgrade below
    /// `confirm`. Absent/false → `allow` default.
    pub privileged: bool,
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
    #[serde(default)]
    privileged: bool,
    #[serde(default)]
    line_ending: LineEnding,
    #[serde(default)]
    echo_mode: EchoMode,
    #[serde(default)]
    semantic_prompt: SemanticPrompt,
    #[serde(default)]
    deny_patterns: Vec<String>,
    #[serde(default)]
    allow_patterns: Vec<String>,
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
            console: ConsoleSettings {
                line_ending: raw.line_ending,
                echo_mode: raw.echo_mode,
                semantic_prompt: raw.semantic_prompt,
            },
            deny_patterns: raw.deny_patterns,
            allow_patterns: raw.allow_patterns,
            privileged: raw.privileged,
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
        privileged   = true
        line_ending   = "lf"
        echo_mode     = "line"
        semantic_prompt = "osc3008"
        deny_patterns = ["\\breboot\\b"]
        allow_patterns = ["^uname"]
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
        assert_eq!(esp.console, ConsoleSettings::default());
        assert!(!esp.privileged, "privileged defaults to false when absent");

        let rpi = profiles.iter().find(|p| p.name == "rpi5").unwrap();
        assert_eq!(rpi.match_vid, None, "vid is optional");
        assert_eq!(rpi.match_pid, None);
        assert!(rpi.tags.is_empty(), "tags default to empty");
        assert_eq!(rpi.probe, None);
        assert!(rpi.privileged, "privileged is honored when set");
        assert_eq!(rpi.console.line_ending, LineEnding::Lf);
        assert_eq!(rpi.console.echo_mode, EchoMode::Line);
        assert_eq!(rpi.console.semantic_prompt, SemanticPrompt::Osc3008);
        assert_eq!(rpi.deny_patterns, vec!["\\breboot\\b"]);
        assert_eq!(rpi.allow_patterns, vec!["^uname"]);
    }

    #[test]
    fn global_deny_patterns_require_a_json_array_of_strings() {
        assert_eq!(
            parse_global_deny_patterns("[\"reboot\"]").unwrap(),
            ["reboot"]
        );
        assert!(matches!(
            parse_global_deny_patterns("not-json"),
            Err(SerialError::InvalidParam { name, .. }) if name == DENY_PATTERNS_ENV
        ));
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
    fn devices_path_prefers_the_explicit_env_var() {
        assert_eq!(
            resolve_devices_path(
                Some("custom/profiles.toml".into()),
                Some("/xdg".into()),
                Some("/home/user".into()),
            ),
            Some(PathBuf::from("custom/profiles.toml"))
        );
    }

    #[test]
    fn devices_path_uses_the_user_config_directory() {
        let expected = PathBuf::from("/xdg/mcp-serial-rs/devices.toml");
        assert_eq!(
            resolve_devices_path(None, Some("/xdg".into()), Some("/home/user".into())),
            Some(expected)
        );
        let expected = PathBuf::from("/home/user/.config/mcp-serial-rs/devices.toml");
        assert_eq!(
            resolve_devices_path(None, None, Some("/home/user".into())),
            Some(expected)
        );
    }

    #[test]
    fn devices_path_never_falls_back_to_the_working_directory() {
        let expected = PathBuf::from("/home/user/.config/mcp-serial-rs/devices.toml");
        assert_eq!(
            resolve_devices_path(None, Some("relative".into()), Some("/home/user".into())),
            Some(expected),
            "a relative XDG_CONFIG_HOME must be ignored"
        );
        assert_eq!(resolve_devices_path(None, None, None), None);
        assert_eq!(resolve_devices_path(Some("".into()), None, None), None);
        assert_eq!(
            resolve_devices_path(None, Some("relative".into()), Some("also-relative".into())),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_writable_profile_files_are_detected() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        for (mode, expected) in [(0o600, false), (0o644, false), (0o664, true), (0o646, true)] {
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(is_group_or_world_writable(tmp.path()), expected, "{mode:o}");
        }
        assert!(!is_group_or_world_writable(Path::new("/nonexistent/x")));
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
