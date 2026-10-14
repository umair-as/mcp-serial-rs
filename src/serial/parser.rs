// SPDX-License-Identifier: MIT OR Apache-2.0

//! `PatternMatcher`: regex-driven `read_until` accumulator for line / prompt
//! detection.
//!
//! Streaming semantics: callers push bytes as they arrive; the matcher
//! accumulates into an internal buffer (logically capped at
//! [`config::MAX_READ_BUFFER`]) and runs the compiled regex against the
//! lossy UTF-8 view of the live window. When the buffer grows past the cap,
//! the oldest bytes are dropped — so a pattern that spans the truncation
//! boundary will not be found. Set `MAX_READ_BUFFER` large enough to contain
//! expected prompts.
//!
//! Memory layout: we keep a single `Vec<u8>` plus a `head` cursor marking the
//! logical start. Push appends in O(amortized 1). Compaction (a real
//! `drain(..head)`) runs only when the dead prefix grows past `max_buffer`,
//! so the physical Vec is bounded at `2 * max_buffer` and per-byte cost is
//! O(1) amortized — important for long-running prompt-watching sessions.

use regex::{Regex, RegexBuilder};

use crate::config;
use crate::errors::SerialError;

/// Compile `pattern` within [`config::MAX_PATTERN_LEN`],
/// [`config::REGEX_SIZE_LIMIT`], and [`config::REGEX_DFA_SIZE_LIMIT`]. Every
/// regex the server builds goes through here. The error is a caller-facing
/// reason; callers wrap it in their own `InvalidParam`.
pub fn bounded_regex(pattern: &str) -> Result<Regex, String> {
    if pattern.len() > config::MAX_PATTERN_LEN {
        return Err(format!(
            "regex is {} bytes; max is {}",
            pattern.len(),
            config::MAX_PATTERN_LEN
        ));
    }
    RegexBuilder::new(pattern)
        .size_limit(config::REGEX_SIZE_LIMIT)
        .dfa_size_limit(config::REGEX_DFA_SIZE_LIMIT)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))
}

#[derive(Debug)]
pub struct PatternMatcher {
    regex: Regex,
    buffer: Vec<u8>,
    /// Logical start of the live window inside `buffer`. Bytes before this
    /// have been "dropped" but not yet physically removed.
    head: usize,
    /// Byte offset inside the live window before which matches are ignored.
    /// `serial.exec` uses this to exclude a line-mode command echo without
    /// discarding it from the authoritative transcript.
    search_start: usize,
    max_buffer: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchDetails {
    pub text: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

impl PatternMatcher {
    /// Compile `pattern` and start with an empty buffer of capacity
    /// [`config::MAX_READ_BUFFER`]. An empty pattern is rejected — it would
    /// match every push and is almost certainly a caller bug.
    pub fn new(pattern: &str) -> Result<Self, SerialError> {
        Self::with_capacity(pattern, config::MAX_READ_BUFFER)
    }

    /// Explicit-capacity variant; used by tests that want to exercise the
    /// truncation boundary cheaply.
    pub fn with_capacity(pattern: &str, max_buffer: usize) -> Result<Self, SerialError> {
        if pattern.is_empty() {
            return Err(SerialError::InvalidParam {
                name: "pattern".into(),
                reason: "must not be empty".into(),
            });
        }
        let regex = bounded_regex(pattern).map_err(|reason| SerialError::InvalidParam {
            name: "pattern".into(),
            reason,
        })?;
        Ok(Self {
            regex,
            buffer: Vec::new(),
            head: 0,
            search_start: 0,
            max_buffer,
        })
    }

    /// Append `data`, truncating the oldest bytes if the live window would
    /// exceed the cap. Returns `true` when the pattern now matches anywhere
    /// in the accumulated buffer.
    pub fn push(&mut self, data: &[u8]) -> bool {
        self.push_without_matching(data);
        self.is_match()
    }

    /// Append `data` without evaluating the regex. This lets line-echo
    /// execution defer matching until its declared echo boundary is visible.
    pub fn push_without_matching(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
        // Advance the logical head if the live window exceeds the cap.
        // This is O(1) — no actual memmove yet.
        let live_len = self.buffer.len() - self.head;
        if live_len > self.max_buffer {
            let dropped = live_len - self.max_buffer;
            self.head += dropped;
            self.search_start = self.search_start.saturating_sub(dropped);
        }
        // Physical compaction: only when the dead prefix grows past one cap,
        // amortising the O(n) drain cost across `max_buffer` pushes.
        if self.head > self.max_buffer {
            self.buffer.drain(..self.head);
            self.head = 0;
        }
    }

    /// Re-evaluate the pattern against the current buffer without pushing.
    pub fn is_match(&self) -> bool {
        self.match_details().is_some()
    }

    pub fn match_details(&self) -> Option<MatchDetails> {
        let live = self.live();
        // Fast path: a valid UTF-8 window is its own lossy decoding, so raw and
        // text offsets coincide and no boundary table is needed. This runs on
        // every push, so avoiding the per-byte allocation matters.
        if let Ok(text) = std::str::from_utf8(live) {
            let mut search_start = self.search_start.min(text.len());
            while !text.is_char_boundary(search_start) {
                search_start += 1;
            }
            return self.find_from(text, search_start);
        }
        let (text, search_start) = decode_lossy_at(live, self.search_start);
        self.find_from(&text, search_start)
    }

    fn find_from(&self, text: &str, search_start: usize) -> Option<MatchDetails> {
        self.regex
            .find(&text[search_start..])
            .map(|m| MatchDetails {
                text: m.as_str().to_string(),
                start_byte: search_start + m.start(),
                end_byte: search_start + m.end(),
            })
    }

    /// Ignore matches before `offset` bytes into the current live window.
    /// The bytes remain available through [`Self::buffer`] and
    /// [`Self::into_buffer`].
    pub fn set_search_start(&mut self, offset: usize) {
        self.search_start = offset.min(self.live().len());
    }

    pub fn buffer(&self) -> &[u8] {
        self.live()
    }

    /// Consume the matcher, returning the live window as an owned `Vec`.
    pub fn into_buffer(mut self) -> Vec<u8> {
        if self.head > 0 {
            self.buffer.drain(..self.head);
        }
        self.buffer
    }

    fn live(&self) -> &[u8] {
        &self.buffer[self.head..]
    }
}

/// Decode `raw` exactly as `String::from_utf8_lossy` does (the text returned
/// to MCP callers) and map one raw byte offset into it. An offset inside a
/// multi-byte character, or at or inside a malformed sequence, advances to the
/// next character boundary so regex slicing always remains valid. Only the
/// single offset is mapped: this runs on every push, and a per-byte boundary
/// table made a hostile byte-at-a-time stream quadratic in allocations.
fn decode_lossy_at(raw: &[u8], raw_offset: usize) -> (String, usize) {
    let raw_offset = raw_offset.min(raw.len());
    let mut text = String::with_capacity(raw.len());
    let mut mapped = None;
    let mut raw_pos = 0;
    for chunk in raw.utf8_chunks() {
        let valid = chunk.valid();
        if mapped.is_none() && raw_offset < raw_pos + valid.len() {
            let mut offset = raw_offset - raw_pos;
            while !valid.is_char_boundary(offset) {
                offset += 1;
            }
            mapped = Some(text.len() + offset);
        }
        text.push_str(valid);
        raw_pos += valid.len();
        let invalid = chunk.invalid();
        if !invalid.is_empty() {
            text.push('\u{fffd}');
            if mapped.is_none() && raw_offset <= raw_pos + invalid.len() {
                mapped = Some(text.len());
            }
            raw_pos += invalid.len();
        }
    }
    let mapped = mapped.unwrap_or(text.len());
    (text, mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_string_matches() {
        let mut m = PatternMatcher::new("ALL KATS PASSED").unwrap();
        assert!(!m.push(b"running self-test...\n"));
        assert!(m.push(b"ALL KATS PASSED\n"));
        assert_eq!(m.buffer(), b"running self-test...\nALL KATS PASSED\n");
    }

    #[test]
    fn regex_matches() {
        let mut m = PatternMatcher::new(r"booted in \d+ms").unwrap();
        assert!(!m.push(b"booting...\n"));
        assert!(m.push(b"booted in 142ms\n"));
    }

    #[test]
    fn match_spans_multiple_pushes() {
        // Pattern straddles two byte chunks — emulates the realistic case
        // where data arrives in arbitrary fragments from the UART.
        let mut m = PatternMatcher::new("hello").unwrap();
        assert!(!m.push(b"hel"));
        assert!(!m.push(b"l")); // still no match — "hell"
        assert!(m.push(b"o world"));
    }

    #[test]
    fn empty_pattern_is_rejected() {
        let err = PatternMatcher::new("").unwrap_err();
        assert!(matches!(err, SerialError::InvalidParam { ref name, .. } if name == "pattern"));
    }

    #[test]
    fn invalid_regex_is_rejected() {
        let err = PatternMatcher::new("(unclosed").unwrap_err();
        assert!(
            matches!(err, SerialError::InvalidParam { ref reason, .. } if reason.contains("invalid regex")),
            "got: {err:?}"
        );
    }

    #[test]
    fn buffer_truncates_at_max() {
        // 8-byte cap; push 12 bytes total in two halves.
        let mut m = PatternMatcher::with_capacity("zzz", 8).unwrap();
        assert!(!m.push(b"AAAA"));
        assert!(!m.push(b"BBBBCCCC"));
        assert_eq!(m.buffer().len(), 8, "should be capped at max_buffer");
        // Oldest bytes (the AAAAs) must have been dropped.
        assert!(!m.buffer().starts_with(b"A"));
        assert_eq!(m.buffer(), b"BBBBCCCC");
    }

    #[test]
    fn truncation_preserves_match_in_recent_window() {
        // The pattern lives in the freshest bytes — must still match after
        // older data is dropped from the front.
        let mut m = PatternMatcher::with_capacity("READY>", 16).unwrap();
        m.push(b"junkjunkjunkjunkjunk"); // 20 bytes — buffer trimmed to last 16
        assert!(m.push(b" READY> "));
    }

    #[test]
    fn anchored_regex_matches_against_buffer_start() {
        let mut m = PatternMatcher::new(r"^boot").unwrap();
        // Multi-line mode is off by default, so ^ anchors at buffer start.
        assert!(!m.push(b"warming\nboot ok\n"));
        let mut m2 = PatternMatcher::new(r"^boot").unwrap();
        assert!(m2.push(b"boot ok\n"));
    }

    #[test]
    fn lazy_compaction_bounds_physical_buffer() {
        // With max_buffer=8 and lazy compaction, the underlying Vec grows
        // at most to (2 * max_buffer)=16 between compactions. After many
        // overflowing pushes the live window stays exactly max_buffer.
        let mut m = PatternMatcher::with_capacity("zzz", 8).unwrap();
        for _ in 0..10_000 {
            m.push(b"AAAAAAAA"); // 8 bytes each — guarantees overflow every push
        }
        assert_eq!(m.buffer().len(), 8);
        assert!(m.buffer().iter().all(|&b| b == b'A'));
        // Sanity: into_buffer compacts before returning.
        let v = m.into_buffer();
        assert_eq!(v.len(), 8);
    }

    #[test]
    fn is_match_recomputes_without_push() {
        let mut m = PatternMatcher::new("done").unwrap();
        assert!(!m.is_match());
        m.push(b"done\n");
        assert!(m.is_match());
        assert!(m.is_match(), "is_match must be idempotent");
    }

    #[test]
    fn search_start_excludes_echo_but_preserves_transcript() {
        let mut m = PatternMatcher::new("READY").unwrap();
        assert!(m.push(b"echo READY\n"));
        m.set_search_start(11);
        assert!(!m.is_match());
        assert!(m.push(b"actual READY\n"));
        assert_eq!(m.buffer(), b"echo READY\nactual READY\n");
        assert_eq!(m.match_details().unwrap().start_byte, 18);
    }

    #[test]
    fn match_details_use_offsets_in_the_single_lossy_output_string() {
        let mut m = PatternMatcher::new("READY").unwrap();
        m.push("éREADY".as_bytes());
        // This raw-byte boundary is inside the two-byte UTF-8 encoding of é.
        // It must advance to a character boundary in the one lossy string that
        // callers receive, never splice two separately decoded fragments.
        m.set_search_start(1);
        let details = m.match_details().unwrap();
        assert_eq!(details.start_byte, "é".len());
        assert_eq!(details.end_byte, "éREADY".len());
        assert_eq!(
            &String::from_utf8_lossy(m.buffer())[details.start_byte..details.end_byte],
            "READY"
        );
    }

    #[test]
    fn match_details_remain_sliceable_after_invalid_utf8() {
        let mut m = PatternMatcher::new("READY").unwrap();
        m.push(b"\xffREADY");
        m.set_search_start(1);
        let details = m.match_details().unwrap();
        let output = String::from_utf8_lossy(m.buffer());
        assert_eq!(details.start_byte, '\u{fffd}'.len_utf8());
        assert_eq!(&output[details.start_byte..details.end_byte], "READY");
    }

    #[test]
    fn search_start_inside_multibyte_char_advances_on_valid_utf8() {
        // "é" is two bytes; a raw offset of 1 lands inside it.
        let mut m = PatternMatcher::new("^.").unwrap();
        m.push_without_matching("éa".as_bytes());
        m.set_search_start(1);
        let details = m.match_details().unwrap();
        assert_eq!((details.text.as_str(), details.start_byte), ("a", 2));
    }

    #[test]
    fn decode_lossy_at_rounds_offsets_past_malformed_and_partial_sequences() {
        // a, lone 0xFF, b, truncated 3-byte sequence. Text: a + U+FFFD + b + U+FFFD.
        let raw = b"a\xFFb\xE2\x82";
        let expected = String::from_utf8_lossy(raw);
        for (offset, mapped) in [(0, 0), (1, 4), (2, 4), (3, 8), (4, 8), (5, 8), (9, 8)] {
            let (text, at) = decode_lossy_at(raw, offset);
            assert_eq!(text, expected);
            assert_eq!(at, mapped, "raw offset {offset}");
        }
    }

    #[test]
    fn bounded_regex_rejects_long_or_oversized_patterns() {
        let long = "a".repeat(config::MAX_PATTERN_LEN + 1);
        assert!(bounded_regex(&long).unwrap_err().contains("max is"));
        assert!(bounded_regex(&"a".repeat(config::MAX_PATTERN_LEN)).is_ok());
        // Short source, huge compiled program.
        let err = bounded_regex(r"\w{1000}\w{1000}\w{1000}").unwrap_err();
        assert!(err.contains("invalid regex"), "{err}");
    }

    #[test]
    fn truncation_adjusts_search_start() {
        let mut m = PatternMatcher::with_capacity("READY", 12).unwrap();
        m.push(b"echo READY\n");
        m.set_search_start(11);
        assert!(!m.push(b"xx"));
        assert!(!m.is_match());
        assert!(m.push(b"READY"));
    }
}
