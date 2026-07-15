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

use regex::Regex;

use crate::config;
use crate::errors::SerialError;

#[derive(Debug)]
pub struct PatternMatcher {
    regex: Regex,
    buffer: Vec<u8>,
    /// Logical start of the live window inside `buffer`. Bytes before this
    /// have been "dropped" but not yet physically removed.
    head: usize,
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
        let regex = Regex::new(pattern).map_err(|e| SerialError::InvalidParam {
            name: "pattern".into(),
            reason: format!("invalid regex: {e}"),
        })?;
        Ok(Self {
            regex,
            buffer: Vec::new(),
            head: 0,
            max_buffer,
        })
    }

    /// Append `data`, truncating the oldest bytes if the live window would
    /// exceed the cap. Returns `true` when the pattern now matches anywhere
    /// in the accumulated buffer.
    pub fn push(&mut self, data: &[u8]) -> bool {
        self.buffer.extend_from_slice(data);
        // Advance the logical head if the live window exceeds the cap.
        // This is O(1) — no actual memmove yet.
        let live_len = self.buffer.len() - self.head;
        if live_len > self.max_buffer {
            self.head += live_len - self.max_buffer;
        }
        // Physical compaction: only when the dead prefix grows past one cap,
        // amortising the O(n) drain cost across `max_buffer` pushes.
        if self.head > self.max_buffer {
            self.buffer.drain(..self.head);
            self.head = 0;
        }
        self.is_match()
    }

    /// Re-evaluate the pattern against the current buffer without pushing.
    pub fn is_match(&self) -> bool {
        self.match_details().is_some()
    }

    pub fn match_details(&self) -> Option<MatchDetails> {
        let text = String::from_utf8_lossy(self.live());
        self.regex.find(&text).map(|m| MatchDetails {
            text: m.as_str().to_string(),
            start_byte: m.start(),
            end_byte: m.end(),
        })
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
}
