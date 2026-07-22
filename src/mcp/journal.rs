// SPDX-License-Identifier: MIT OR Apache-2.0

//! Summary shaping for rmcp tool-call journal entries.
//!
//! The audit journal is scoped to **tool calls only** — one `call` row
//! before dispatch and one `result` row after. Lifecycle traffic
//! (`initialize`, `tools/list`, `notifications/initialized`) is
//! intentionally NOT journaled here; it remains observable via `tracing`
//! on stderr.
//!
//! These helpers shape the `serde_json::Value` summaries written into
//! each tool's JSONL rows. The default journal is metadata-only: it records
//! bounded field names, byte counts, statuses, and error codes, but not raw
//! command or device-output text.

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use serde_json::{json, Value};

use crate::serial::journal::JournalEntry;

const MAX_ARG_KEYS: usize = 16;
const MAX_JOURNAL_FIELD_CHARS: usize = 128;

fn clipped(value: &str) -> String {
    value.chars().take(MAX_JOURNAL_FIELD_CHARS).collect()
}

/// Byte length of a string field, or `0` when missing / non-string.
/// Counts bytes (not chars) — clients compare against `MAX_WRITE_CHUNK`.
fn byte_len(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or(0)
}

fn object_keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .map(|obj| {
            obj.keys()
                .take(MAX_ARG_KEYS)
                .map(|key| clipped(key))
                .collect()
        })
        .unwrap_or_default()
}

fn optional_session_id(value: &Value) -> Option<String> {
    value.get("session_id").and_then(Value::as_str).map(clipped)
}

fn optional_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn optional_bool(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(clipped)
}

/// `session_id` for a `call` row, before the handler runs. Most tools
/// take it in their arguments; `serial.list_ports` and `serial.open`
/// have no session yet, so the `NO_SESSION` sentinel is used.
pub fn call_session_id(args: &Value) -> String {
    args.get("session_id")
        .and_then(Value::as_str)
        .map(clipped)
        .unwrap_or_else(|| JournalEntry::NO_SESSION.to_string())
}

/// `session_id` for a `result` row. For `serial.open`, prefer the
/// freshly-minted id from the structured result so an `open` call
/// pair can be reconstructed by `session_id`. For every other tool,
/// fall back to the call-time id.
pub fn result_session_id(
    tool: &str,
    args: &Value,
    result: &Result<&CallToolResult, &ErrorData>,
) -> String {
    if tool == "serial.open" {
        if let Ok(ok) = result {
            if let Some(sid) = ok
                .structured_content
                .as_ref()
                .and_then(|sc| sc.get("session_id"))
                .and_then(Value::as_str)
            {
                return clipped(sid);
            }
        }
    }
    call_session_id(args)
}

/// Shape the `summary` payload for a `call` row. Per-tool fields keep
/// large data clipped + sized; everything else passes through as-is.
pub fn call_summary(tool: &str, args: &Value) -> Value {
    match tool {
        "serial.open" => json!({
            // Records the requested `write_policy` for audit. `call_summary`
            // runs on *unvalidated* args (before deserialization), so this is
            // attacker-controlled and may not be a valid enum — clip it to a
            // bounded string like every other field so a huge value can't bloat
            // the row (or, past the 16 KiB row cap, drop it from the journal).
            "write_policy": args.get("write_policy").and_then(Value::as_str).map(clipped),
            "arg_keys": object_keys(args),
        }),
        "serial.write" => json!({
            "session_id": optional_session_id(args),
            "bytes": byte_len(args, "data"),
            "confirm": optional_bool(args, "confirm"),
            "arg_keys": object_keys(args),
        }),
        "serial.exec" => json!({
            "session_id": optional_session_id(args),
            "command_bytes": byte_len(args, "command"),
            "expect_bytes": byte_len(args, "expect"),
            "timeout_ms": optional_u64(args, "timeout_ms"),
            "confirm": optional_bool(args, "confirm"),
            "clear_before_write": optional_bool(args, "clear_before_write"),
            "normalize_output": optional_bool(args, "normalize_output"),
            "line_ending": optional_string(args, "line_ending"),
            "echo_mode": optional_string(args, "echo_mode"),
            "semantic_prompt": optional_string(args, "semantic_prompt"),
            "arg_keys": object_keys(args),
        }),
        _ => json!({
            "arg_keys": object_keys(args),
        }),
    }
}

/// Shape the `summary` payload for a `result` row. Errors carry the
/// pinned JSON-RPC code + message so tail-only consumers can branch
/// without parsing human-readable text.
pub fn result_summary(tool: &str, result: &Result<&CallToolResult, &ErrorData>) -> Value {
    match result {
        Err(e) => json!({
            "ok": false,
            "error_code": e.code.0,
        }),
        Ok(call_result) => {
            let sc = call_result.structured_content.as_ref();
            if call_result.is_error == Some(true) {
                let error = sc.and_then(|v| v.get("error"));
                return json!({
                    "ok": false,
                    "is_error": true,
                    "error_code": error.and_then(|e| e.get("code")),
                    "error_type": error.and_then(|e| e.get("type")),
                });
            }
            match tool {
                "serial.list_ports" => json!({
                    "ok": true,
                    "port_count": sc
                        .and_then(|v| v.get("ports"))
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0),
                }),
                "serial.open" => json!({
                    "ok": true,
                    "session_id": sc.and_then(|v| v.get("session_id")),
                }),
                "serial.write" => json!({
                    "ok": true,
                    "bytes_written": sc.and_then(|v| v.get("bytes_written")),
                }),
                "serial.read" => {
                    let data = sc.cloned().unwrap_or(Value::Null);
                    json!({
                        "ok": true,
                        "bytes": byte_len(&data, "data"),
                        "status": data.get("status"),
                        "truncated": data.get("truncated"),
                    })
                }
                "serial.read_until" => {
                    let data = sc.cloned().unwrap_or(Value::Null);
                    json!({
                        "ok": true,
                        "bytes": byte_len(&data, "data"),
                        "status": data.get("status"),
                        "matched": data.get("matched"),
                        "truncated": data.get("truncated"),
                    })
                }
                "serial.exec" => {
                    let out = sc.cloned().unwrap_or(Value::Null);
                    json!({
                        "ok": true,
                        "output_bytes": byte_len(&out, "output"),
                        "status": out.get("status"),
                        "exec_ok": out.get("ok"),
                        "command_written": out.get("command_written"),
                        "truncated": out.get("truncated"),
                        "command_output_source": out.get("command_output_source"),
                        "semantic_status": out.get("semantic_status"),
                        "exit_code": out.get("exit_code"),
                    })
                }
                "serial.close" => json!({
                    "ok": true,
                    "closed": sc.and_then(|v| v.get("ok")),
                }),
                _ => json!({ "ok": true, "has_structured_content": sc.is_some() }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, ErrorCode};

    fn ok_with(sc: Value) -> CallToolResult {
        CallToolResult::structured(sc)
    }

    #[test]
    fn call_summary_records_write_size_without_payload() {
        let big = "x".repeat(500);
        let args = json!({"session_id": "s".repeat(10_000), "data": big.clone(), "confirm": true});
        let summary = call_summary("serial.write", &args);
        assert_eq!(summary["bytes"], 500, "byte_len preserves the full size");
        assert_eq!(
            summary["session_id"].as_str().unwrap().chars().count(),
            MAX_JOURNAL_FIELD_CHARS
        );
        // `confirm` is recorded as metadata (bool), never the data payload.
        assert_eq!(summary["confirm"], json!(true));
        assert!(summary.get("head").is_none());
        assert!(!summary.to_string().contains(&big));
        assert!(summary.to_string().len() < big.len());
    }

    #[test]
    fn open_call_summary_clips_write_policy() {
        // `call_summary` runs before arg deserialization, so `write_policy` is
        // unvalidated and attacker-controlled. It must be clipped like every
        // other field — never passed through verbatim (which could bloat the
        // row past the 16 KiB cap and drop the audit record).
        let big = "z".repeat(50_000);
        let args = json!({"port": "/dev/ttyUSB0", "write_policy": big.clone()});
        let summary = call_summary("serial.open", &args);
        assert_eq!(
            summary["write_policy"].as_str().unwrap().chars().count(),
            MAX_JOURNAL_FIELD_CHARS,
        );
        assert!(!summary.to_string().contains(&big));
        // A non-string value coerces to null rather than passing through.
        let odd = json!({"port": "/dev/ttyUSB0", "write_policy": {"nested": "x"}});
        assert!(call_summary("serial.open", &odd)["write_policy"].is_null());
    }

    #[test]
    fn unknown_call_summary_is_bounded_and_metadata_only() {
        let big = "secret".repeat(1000);
        let args = json!({"session_id": "s", "data": big, "other": "value"});
        let summary = call_summary("unknown.tool", &args);
        assert!(summary.get("args").is_none());
        assert!(summary.to_string().len() < 256);
        assert!(!summary.to_string().contains("secret"));
    }

    #[test]
    fn call_summary_bounds_keys_and_omits_regex_text() {
        let huge_key = "k".repeat(10_000);
        let secret = "TOP_SECRET_SENTINEL";
        let args = json!({
            "session_id": "s",
            huge_key: "value",
            "expect": format!("bad-regex-{secret}[")
        });
        let summary = call_summary("serial.exec", &args);
        assert!(
            summary.to_string().len() < 512,
            "summary should stay bounded: {summary}"
        );
        assert!(
            !summary.to_string().contains(secret),
            "regex text must not be journaled by default"
        );
        let keys = summary["arg_keys"].as_array().expect("arg_keys");
        assert!(keys
            .iter()
            .all(|key| key.as_str().unwrap().chars().count() <= MAX_JOURNAL_FIELD_CHARS));
    }

    #[test]
    fn exec_call_summary_records_bounded_console_metadata_without_command() {
        let secret = "TOP_SECRET_COMMAND";
        let oversized_mode = "m".repeat(10_000);
        let args = json!({
            "session_id": "s",
            "command": secret,
            "expect": "prompt",
            "line_ending": "lf",
            "echo_mode": oversized_mode,
            "semantic_prompt": "osc3008",
        });
        let summary = call_summary("serial.exec", &args);
        assert_eq!(summary["line_ending"], "lf");
        assert_eq!(summary["semantic_prompt"], "osc3008");
        assert_eq!(
            summary["echo_mode"].as_str().unwrap().chars().count(),
            MAX_JOURNAL_FIELD_CHARS
        );
        assert!(!summary.to_string().contains(secret));
    }

    #[test]
    fn call_session_id_returns_sentinel_when_field_missing() {
        let args = json!({"port": "/dev/ttyUSB0"});
        assert_eq!(call_session_id(&args), JournalEntry::NO_SESSION);
    }

    #[test]
    fn result_session_id_uses_open_result_for_serial_open() {
        let result = ok_with(json!({"session_id": "abcdef0123456789"}));
        let args = json!({"port": "/dev/ttyUSB0"});
        let sid = result_session_id("serial.open", &args, &Ok(&result));
        assert_eq!(sid, "abcdef0123456789");
    }

    #[test]
    fn result_summary_includes_pinned_error_code() {
        let err = ErrorData::new(ErrorCode(-32003), "unknown session", None);
        let summary = result_summary("serial.write", &Err(&err));
        assert_eq!(summary["ok"], json!(false));
        assert_eq!(summary["error_code"], json!(-32003));
        assert!(summary.get("error_message").is_none());
    }

    #[test]
    fn result_summary_for_list_ports_reports_count_from_array() {
        let result =
            ok_with(json!({"ports": [{"port": "/dev/ttyUSB0"}, {"port": "/dev/ttyUSB1"}]}));
        let summary = result_summary("serial.list_ports", &Ok(&result));
        assert_eq!(summary["ok"], json!(true));
        assert_eq!(summary["port_count"], json!(2));
    }

    #[test]
    fn result_summary_for_read_records_size_without_payload() {
        let big = "y".repeat(300);
        let result = ok_with(json!({"data": big}));
        let summary = result_summary("serial.read", &Ok(&result));
        assert_eq!(summary["bytes"], 300);
        assert!(summary.get("head").is_none());
    }
}
