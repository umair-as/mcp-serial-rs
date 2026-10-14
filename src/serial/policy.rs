// SPDX-License-Identifier: MIT OR Apache-2.0

//! Server-owned command policy compiled once when a session opens.

//! Policies are immutable session state. They evaluate only complete
//! `serial.exec` commands; the MCP adapter refuses raw `serial.write` whenever
//! a policy is attached so callers cannot evade matching by splitting a command.
//!
//! Regexes judge one command line, so a guarded command must also be unable to
//! reach any *other* line: [`CommandPolicy::check`] first rejects control
//! characters (an embedded newline, CR, or line-editing key), a trailing
//! backslash (shell line continuation), a per-call line ending that differs
//! from the session's, and any transmission without a terminator. Unterminated
//! fragments would sit in the shell's line buffer until a later call or session
//! submitted them as one line.
//! Deny rules remain defence in depth; see ADR 0007.

use regex::Regex;

use crate::errors::SerialError;
use crate::serial::console::LineEnding;
use crate::serial::parser::bounded_regex;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct CommandPolicySummary {
    pub deny_rule_count: usize,
    pub allow_rule_count: usize,
    pub mutation_via_exec_only: bool,
}

#[derive(Debug)]
struct Rule {
    name: String,
    regex: Regex,
}

#[derive(Debug, Default)]
pub struct CommandPolicy {
    deny: Vec<Rule>,
    allow: Vec<Rule>,
}

impl CommandPolicy {
    pub fn compile(
        global_deny: &[String],
        profile_deny: &[String],
        profile_allow: &[String],
        caller_deny: &[String],
    ) -> Result<Self, SerialError> {
        Ok(Self {
            deny: compile_rules("deny.global", global_deny)?
                .into_iter()
                .chain(compile_rules("deny.profile", profile_deny)?)
                .chain(compile_rules("deny.caller", caller_deny)?)
                .collect(),
            allow: compile_rules("allow.profile", profile_allow)?,
        })
    }

    /// Gate a complete `serial.exec` command. Returns the generated id of the
    /// structural check or rule that blocks it, or `None` when it may run.
    /// `session_line_ending` is the session default; `requested` is the
    /// per-call `line_ending`, if any. An empty policy imposes nothing.
    pub fn check(
        &self,
        command: &str,
        session_line_ending: LineEnding,
        requested: Option<LineEnding>,
    ) -> Option<&str> {
        if self.is_empty() {
            return None;
        }
        if requested.is_some_and(|line_ending| line_ending != session_line_ending) {
            return Some("shape.line_ending_override");
        }
        if session_line_ending == LineEnding::None {
            return Some("shape.no_line_ending");
        }
        if command.chars().any(char::is_control) {
            return Some("shape.control_char");
        }
        if command.ends_with('\\') {
            return Some("shape.line_continuation");
        }
        self.matched_rule(command)
    }

    pub fn matched_rule(&self, command: &str) -> Option<&str> {
        if let Some(rule) = self.deny.iter().find(|rule| rule.regex.is_match(command)) {
            return Some(rule.name.as_str());
        }
        (!self.allow.is_empty() && !self.allow.iter().any(|rule| rule.regex.is_match(command)))
            .then_some("allow.profile.no_match")
    }

    pub fn is_empty(&self) -> bool {
        self.deny.is_empty() && self.allow.is_empty()
    }

    pub fn summary(&self) -> CommandPolicySummary {
        CommandPolicySummary {
            deny_rule_count: self.deny.len(),
            allow_rule_count: self.allow.len(),
            mutation_via_exec_only: !self.is_empty(),
        }
    }
}

fn compile_rules(prefix: &str, patterns: &[String]) -> Result<Vec<Rule>, SerialError> {
    patterns
        .iter()
        .enumerate()
        .map(|(index, pattern)| {
            bounded_regex(pattern)
                .map(|regex| Rule {
                    name: format!("{}.{index}", prefix),
                    regex,
                })
                .map_err(|error| SerialError::InvalidParam {
                    name: "command_policy".into(),
                    reason: format!("{prefix} regex at index {index}: {error}"),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_rules_win_over_allow_rules() {
        let policy = CommandPolicy::compile(
            &["reboot".into()],
            &[],
            &["^reboot$".into(), "^uname".into()],
            &[],
        )
        .unwrap();
        assert_eq!(policy.matched_rule("reboot"), Some("deny.global.0"));
        assert_eq!(policy.matched_rule("uname -a"), None);
        assert_eq!(policy.matched_rule("id"), Some("allow.profile.no_match"));
    }

    #[test]
    fn caller_rules_only_add_restrictions() {
        let policy =
            CommandPolicy::compile(&[], &["mkfs".into()], &[], &["reboot".into()]).unwrap();
        assert_eq!(policy.matched_rule("mkfs /dev/sda"), Some("deny.profile.0"));
        assert_eq!(policy.matched_rule("reboot"), Some("deny.caller.0"));
    }

    #[test]
    fn check_rejects_commands_that_can_reach_another_line() {
        let policy = CommandPolicy::compile(&[], &[], &["^uname".into()], &[]).unwrap();
        let check = |command| policy.check(command, LineEnding::Lf, None);
        assert_eq!(check("uname -a"), None);
        // An anchored allow rule must not admit a second line.
        assert_eq!(check("uname -a\nreboot"), Some("shape.control_char"));
        assert_eq!(check("uname\r"), Some("shape.control_char"));
        assert_eq!(check("uname\t"), Some("shape.control_char"));
        assert_eq!(check("uname\u{1b}[A"), Some("shape.control_char"));
        assert_eq!(check("uname\u{9b}A"), Some("shape.control_char"));
        assert_eq!(check("uname \\"), Some("shape.line_continuation"));
        assert_eq!(
            policy.check("uname -a", LineEnding::Lf, Some(LineEnding::None)),
            Some("shape.line_ending_override")
        );
        // Restating the session's own ending is not an override.
        assert_eq!(
            policy.check("uname -a", LineEnding::Lf, Some(LineEnding::Lf)),
            None
        );
    }

    #[test]
    fn check_splits_cannot_assemble_a_denied_command() {
        let policy = CommandPolicy::compile(&[r"\breboot\b".into()], &[], &[], &[]).unwrap();
        // Fragments pass the regex individually; the shape gates stop the join.
        assert_eq!(
            policy.check("reb", LineEnding::Lf, Some(LineEnding::None)),
            Some("shape.line_ending_override")
        );
        // An unterminated session cannot leave fragments in the line buffer
        // for a later call or session to submit.
        assert_eq!(
            policy.check("reb", LineEnding::None, None),
            Some("shape.no_line_ending")
        );
        assert_eq!(
            policy.check("reb\\", LineEnding::Lf, None),
            Some("shape.line_continuation")
        );
        assert_eq!(
            policy.check("reboot", LineEnding::Lf, None),
            Some("deny.global.0")
        );
    }

    #[test]
    fn check_imposes_nothing_without_a_policy() {
        let policy = CommandPolicy::default();
        assert_eq!(
            policy.check("a\nb\\", LineEnding::None, Some(LineEnding::Crlf)),
            None
        );
    }

    #[test]
    fn invalid_regex_is_a_typed_error() {
        let err = CommandPolicy::compile(&["[".into()], &[], &[], &[]).unwrap_err();
        assert!(matches!(err, SerialError::InvalidParam { name, .. } if name == "command_policy"));
    }
}
