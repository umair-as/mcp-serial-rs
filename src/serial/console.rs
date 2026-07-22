// SPDX-License-Identifier: MIT OR Apache-2.0

//! Profile-owned console execution settings and bounded transcript analysis.
//!
//! Raw serial bytes remain authoritative. Helpers in this module provide
//! opt-in line submission, echo-aware presentation, and conservative OSC 3008
//! extraction for shell-like profiles without changing generic serial defaults.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LineEnding {
    #[default]
    None,
    Lf,
    Cr,
    Crlf,
}

impl LineEnding {
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Self::None => b"",
            Self::Lf => b"\n",
            Self::Cr => b"\r",
            Self::Crlf => b"\r\n",
        }
    }

    pub fn append_to(self, command: &mut Vec<u8>) {
        command.extend_from_slice(self.bytes());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EchoMode {
    #[default]
    Unknown,
    None,
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SemanticPrompt {
    #[default]
    None,
    Osc3008,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConsoleSettings {
    pub line_ending: LineEnding,
    pub echo_mode: EchoMode,
    pub semantic_prompt: SemanticPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptAnalysis {
    pub normalized_output: String,
    pub command_output: Option<String>,
    pub command_output_source: Option<&'static str>,
    pub semantic_status: Option<&'static str>,
    pub exit_code: Option<i32>,
}

/// Return the exclusive byte offset of a declared line echo in `raw`.
///
/// A line-mode profile promises that the device echoes the command using the
/// same terminator the caller transmitted. `none` has no delimiter, so it
/// cannot safely establish an echo boundary.
pub fn echo_line_boundary(raw: &[u8], line_ending: LineEnding) -> Option<usize> {
    let terminator = line_ending.bytes();
    (!terminator.is_empty()).then_some(())?;
    raw.windows(terminator.len())
        .position(|window| window == terminator)
        .map(|start| start + terminator.len())
}

pub fn analyze_transcript(
    raw: &[u8],
    line_ending: LineEnding,
    echo_mode: EchoMode,
    semantic_prompt: SemanticPrompt,
) -> TranscriptAnalysis {
    let echo_boundary = (echo_mode == EchoMode::Line)
        .then(|| echo_line_boundary(raw, line_ending))
        .flatten();
    let normalized_output = normalize_exec_output(raw, echo_boundary);

    if semantic_prompt == SemanticPrompt::Osc3008 {
        if let Some(semantic) = parse_one_osc3008_command(raw) {
            return TranscriptAnalysis {
                normalized_output,
                command_output: Some(normalize_terminal_output(&String::from_utf8_lossy(
                    semantic.output,
                ))),
                command_output_source: Some("osc3008"),
                semantic_status: semantic.status,
                exit_code: semantic.exit_code,
            };
        }
    }

    let command_output = match echo_mode {
        EchoMode::Line => echo_boundary
            .map(|boundary| normalize_terminal_output(&String::from_utf8_lossy(&raw[boundary..]))),
        EchoMode::None => Some(normalized_output.clone()),
        EchoMode::Unknown => {
            (semantic_prompt != SemanticPrompt::None).then(|| normalized_output.clone())
        }
    };
    let source = command_output.as_ref().map(|_| "normalized_transcript");

    TranscriptAnalysis {
        normalized_output,
        command_output,
        command_output_source: source,
        semantic_status: None,
        exit_code: None,
    }
}

pub fn normalize_terminal_output(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some(']') => {
                    let _ = chars.next();
                    let mut prev_esc = false;
                    for c in chars.by_ref() {
                        if c == '\u{7}' || (prev_esc && c == '\\') {
                            break;
                        }
                        prev_esc = c == '\u{1b}';
                    }
                }
                Some('[') => {
                    let _ = chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if ch == '\r' {
            if chars.peek().copied() != Some('\n') {
                out.push('\n');
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn normalize_exec_output(raw: &[u8], echo_boundary: Option<usize>) -> String {
    let visible = echo_boundary.map_or(raw, |boundary| &raw[boundary..]);
    normalize_terminal_output(&String::from_utf8_lossy(visible))
}

struct OscCommand<'a> {
    output: &'a [u8],
    status: Option<&'static str>,
    exit_code: Option<i32>,
}

#[derive(Debug)]
struct OscMarker {
    start: usize,
    end: usize,
    id: String,
    kind: OscKind,
    command: bool,
    status: Option<&'static str>,
    exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OscKind {
    Start,
    End,
}

fn parse_one_osc3008_command(raw: &[u8]) -> Option<OscCommand<'_>> {
    let markers = osc3008_markers(raw)?;
    let starts: Vec<_> = markers
        .iter()
        .filter(|marker| marker.kind == OscKind::Start && marker.command)
        .collect();
    if starts.len() != 1 {
        return None;
    }
    let start = starts[0];
    let ends: Vec<_> = markers
        .iter()
        .filter(|marker| {
            marker.kind == OscKind::End && marker.id == start.id && marker.start >= start.end
        })
        .collect();
    if ends.len() != 1 {
        return None;
    }
    let end = ends[0];
    Some(OscCommand {
        output: &raw[start.end..end.start],
        status: end.status,
        exit_code: end.exit_code,
    })
}

fn osc3008_markers(raw: &[u8]) -> Option<Vec<OscMarker>> {
    const PREFIX: &[u8] = b"\x1b]3008;";
    let mut markers = Vec::new();
    let mut cursor = 0;
    while cursor + PREFIX.len() <= raw.len() {
        let Some(relative) = raw[cursor..]
            .windows(PREFIX.len())
            .position(|window| window == PREFIX)
        else {
            break;
        };
        let start = cursor + relative;
        let body_start = start + PREFIX.len();
        let (body_end, marker_end) = osc_end(raw, body_start)?;
        let body = std::str::from_utf8(&raw[body_start..body_end]).ok()?;
        markers.push(parse_osc_marker(start, marker_end, body)?);
        cursor = marker_end;
    }
    Some(markers)
}

fn osc_end(raw: &[u8], body_start: usize) -> Option<(usize, usize)> {
    let mut index = body_start;
    while index < raw.len() {
        if raw[index] == 0x07 {
            return Some((index, index + 1));
        }
        if raw[index] == 0x1b && raw.get(index + 1) == Some(&b'\\') {
            return Some((index, index + 2));
        }
        index += 1;
    }
    None
}

fn parse_osc_marker(start: usize, end: usize, body: &str) -> Option<OscMarker> {
    let mut fields = body.split(';');
    let first = fields.next()?;
    let (kind, id) = if let Some(id) = first.strip_prefix("start=") {
        (OscKind::Start, id)
    } else {
        let id = first.strip_prefix("end=")?;
        (OscKind::End, id)
    };
    if id.is_empty() || id.len() > 128 {
        return None;
    }

    let mut command = false;
    let mut status = None;
    let mut exit_code = None;
    for field in fields {
        if field == "type=command" {
            command = true;
        } else if field == "exit=success" {
            status = Some("success");
        } else if field == "exit=failure" {
            status = Some("failure");
        } else if let Some(value) = field.strip_prefix("status=") {
            exit_code = value.parse().ok();
        }
    }

    Some(OscMarker {
        start,
        end,
        id: id.to_string(),
        kind,
        command,
        status,
        exit_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_append_only_when_selected() {
        for (ending, expected) in [
            (LineEnding::None, b"uname -a".as_slice()),
            (LineEnding::Lf, b"uname -a\n".as_slice()),
            (LineEnding::Cr, b"uname -a\r".as_slice()),
            (LineEnding::Crlf, b"uname -a\r\n".as_slice()),
        ] {
            let mut command = b"uname -a".to_vec();
            ending.append_to(&mut command);
            assert_eq!(command, expected);
        }
    }

    #[test]
    fn osc3008_extracts_one_command_and_status() {
        let raw = b"echo\r\n\x1b]3008;start=id;type=command;cwd=/root\x1b\\OK\r\n\x1b]3008;end=id;exit=success;status=0\x1b\\prompt# ";
        let analysis = analyze_transcript(
            raw,
            LineEnding::Crlf,
            EchoMode::Line,
            SemanticPrompt::Osc3008,
        );
        assert_eq!(analysis.command_output.as_deref(), Some("OK\n"));
        assert_eq!(analysis.command_output_source, Some("osc3008"));
        assert_eq!(analysis.semantic_status, Some("success"));
        assert_eq!(analysis.exit_code, Some(0));
    }

    #[test]
    fn ambiguous_osc3008_falls_back_without_status_claim() {
        let pair = "\x1b]3008;start=id;type=command\x1b\\OK\x1b]3008;end=id;exit=success\x1b\\";
        let raw = format!("echo\r\n{pair}{pair}prompt# ");
        let analysis = analyze_transcript(
            raw.as_bytes(),
            LineEnding::Crlf,
            EchoMode::Line,
            SemanticPrompt::Osc3008,
        );
        assert_eq!(
            analysis.command_output_source,
            Some("normalized_transcript")
        );
        assert_eq!(analysis.semantic_status, None);
        assert_eq!(analysis.exit_code, None);
    }

    #[test]
    fn duplicate_matching_end_marker_is_ambiguous() {
        let raw = b"echo\r\n\x1b]3008;start=id;type=command\x1b\\OK\x1b]3008;end=id;exit=success\x1b\\\x1b]3008;end=id;exit=failure\x1b\\";
        let analysis = analyze_transcript(
            raw,
            LineEnding::Crlf,
            EchoMode::Line,
            SemanticPrompt::Osc3008,
        );
        assert_eq!(
            analysis.command_output_source,
            Some("normalized_transcript")
        );
        assert_eq!(analysis.semantic_status, None);
    }

    #[test]
    fn missing_osc3008_markers_fall_back_without_status_claim() {
        let raw = b"echo\r\nplain output\r\nroot# ";
        let analysis = analyze_transcript(
            raw,
            LineEnding::Crlf,
            EchoMode::Line,
            SemanticPrompt::Osc3008,
        );
        assert_eq!(
            analysis.command_output.as_deref(),
            Some("plain output\nroot# ")
        );
        assert_eq!(
            analysis.command_output_source,
            Some("normalized_transcript")
        );
        assert_eq!(analysis.semantic_status, None);
        assert_eq!(analysis.exit_code, None);
    }

    #[test]
    fn malformed_osc3008_marker_invalidates_semantic_parse() {
        let raw = b"echo\r\n\x1b]3008;not-a-marker\x1b\\\x1b]3008;start=id;type=command\x1b\\OK\x1b]3008;end=id;exit=success\x1b\\";
        let analysis = analyze_transcript(
            raw,
            LineEnding::Crlf,
            EchoMode::Line,
            SemanticPrompt::Osc3008,
        );
        assert_eq!(
            analysis.command_output_source,
            Some("normalized_transcript")
        );
        assert_eq!(analysis.semantic_status, None);
    }

    #[test]
    fn wrapped_echo_is_excluded_from_normalized_output() {
        let raw = "printf 'NARROW_LE\rEN'\r\nRESULT\r\nprompt# ";
        let analysis = analyze_transcript(
            raw.as_bytes(),
            LineEnding::Crlf,
            EchoMode::Line,
            SemanticPrompt::None,
        );
        assert_eq!(analysis.normalized_output, "RESULT\nprompt# ");
    }

    #[test]
    fn cr_and_crlf_echo_boundaries_exclude_only_the_echoed_line() {
        let cr = analyze_transcript(
            b"status\rOK\rprompt> ",
            LineEnding::Cr,
            EchoMode::Line,
            SemanticPrompt::None,
        );
        assert_eq!(cr.command_output.as_deref(), Some("OK\nprompt> "));

        let crlf = analyze_transcript(
            b"status\r\nOK\r\nprompt> ",
            LineEnding::Crlf,
            EchoMode::Line,
            SemanticPrompt::None,
        );
        assert_eq!(crlf.command_output.as_deref(), Some("OK\nprompt> "));
    }
}
