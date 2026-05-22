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
//! each tool's JSONL rows. Large free-form fields (`data` / `command` /
//! `output`) are clipped to [`JOURNAL_HEAD_CHARS`] with the byte-length
//! preserved alongside.

use rmcp::ErrorData;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::serial::journal::JournalEntry;

/// Max chars retained from any large free-form field when summarised
/// into the journal. Char-bounded (not byte-bounded) so the slice
/// always lands on a UTF-8 boundary.
const JOURNAL_HEAD_CHARS: usize = 128;

/// Truncate a string field's contents to [`JOURNAL_HEAD_CHARS`] for
/// inclusion in the summary. Returns empty when the field is missing
/// or not a string.
fn head(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.chars().take(JOURNAL_HEAD_CHARS).collect())
        .unwrap_or_default()
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

/// `session_id` for a `call` row, before the handler runs. Most tools
/// take it in their arguments; `serial.list_ports` and `serial.open`
/// have no session yet, so the `NO_SESSION` sentinel is used.
pub fn call_session_id(args: &Value) -> String {
    args.get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
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
                return sid.to_string();
            }
        }
    }
    call_session_id(args)
}

/// Shape the `summary` payload for a `call` row. Per-tool fields keep
/// large data clipped + sized; everything else passes through as-is.
pub fn call_summary(tool: &str, args: &Value) -> Value {
    match tool {
        "serial.write" => json!({
            "session_id": args.get("session_id"),
            "bytes": byte_len(args, "data"),
            "head": head(args, "data"),
        }),
        "serial.exec" => json!({
            "session_id": args.get("session_id"),
            "command_bytes": byte_len(args, "command"),
            "command_head": head(args, "command"),
            "expect": args.get("expect"),
            "timeout_ms": args.get("timeout_ms"),
        }),
        // Tools without large fields: pass the compact arguments
        // through so the call row stays useful for debugging.
        _ => json!({ "args": args }),
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
            "error_message": e.message,
        }),
        Ok(call_result) => {
            let sc = call_result.structured_content.as_ref();
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
                        "head": head(&data, "data"),
                    })
                }
                "serial.read_until" => {
                    let data = sc.cloned().unwrap_or(Value::Null);
                    json!({
                        "ok": true,
                        "bytes": byte_len(&data, "data"),
                        "head": head(&data, "data"),
                        "matched": data.get("matched"),
                    })
                }
                "serial.exec" => {
                    let out = sc.cloned().unwrap_or(Value::Null);
                    json!({
                        "ok": true,
                        "output_bytes": byte_len(&out, "output"),
                        "output_head": head(&out, "output"),
                        "exec_ok": out.get("ok"),
                    })
                }
                "serial.close" => json!({
                    "ok": true,
                    "closed": sc.and_then(|v| v.get("ok")),
                }),
                // Unknown tool name (shouldn't happen unless the
                // router somehow accepts it): include the raw
                // structured content for triage.
                _ => json!({ "ok": true, "structured_content": sc }),
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
    fn call_summary_truncates_write_head_to_128_chars() {
        let big = "x".repeat(200);
        let args = json!({"session_id": "s", "data": big.clone()});
        let summary = call_summary("serial.write", &args);
        assert_eq!(summary["bytes"], 200, "byte_len preserves the full size");
        let h = summary["head"].as_str().expect("head");
        assert_eq!(h.chars().count(), JOURNAL_HEAD_CHARS);
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
        assert_eq!(summary["error_message"], json!("unknown session"));
    }

    #[test]
    fn result_summary_for_list_ports_reports_count_from_array() {
        let result = ok_with(json!({"ports": [{"port": "/dev/ttyUSB0"}, {"port": "/dev/ttyUSB1"}]}));
        let summary = result_summary("serial.list_ports", &Ok(&result));
        assert_eq!(summary["ok"], json!(true));
        assert_eq!(summary["port_count"], json!(2));
    }

    #[test]
    fn result_summary_for_read_clips_data_head() {
        let big = "y".repeat(300);
        let result = ok_with(json!({"data": big}));
        let summary = result_summary("serial.read", &Ok(&result));
        assert_eq!(summary["bytes"], 300);
        assert_eq!(
            summary["head"].as_str().unwrap().chars().count(),
            JOURNAL_HEAD_CHARS
        );
    }
}
