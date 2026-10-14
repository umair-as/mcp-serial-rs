// SPDX-License-Identifier: MIT OR Apache-2.0

//! Server-owned command policy compiled once when a session opens.

//! Policies are immutable session state. They evaluate only complete
//! `serial.exec` commands; the MCP adapter refuses raw `serial.write` whenever
//! a policy is attached so callers cannot evade matching by splitting a command.
//!
//! Regexes judge one command line, so a guarded command must also be unable to
//! reach any *other* line: [`CommandPolicy::check`] first rejects control
//! characters (an embedded newline, CR, or line-editing key), a trailing
//! backslash (shell line continuation), and a per-call line ending that differs
//! from the session's (`none` then `lf` would join fragments across calls).
//! Deny rules remain defence in depth; see ADR 0007.

use regex::Regex;

use crate::errors::SerialError;

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
    /// `line_ending_overridden` is true when the call asks for a line ending
    /// other than the session's. An empty policy imposes nothing.
    pub fn check(&self, command: &str, line_ending_overridden: bool) -> Option<&str> {
        if self.is_empty() {
            return None;
        }
        if line_ending_overridden {
            return Some("shape.line_ending_override");
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
            Regex::new(pattern)
                .map(|regex| Rule {
                    name: format!("{}.{index}", prefix),
                    regex,
                })
                .map_err(|error| SerialError::InvalidParam {
                    name: "command_policy".into(),
                    reason: format!("invalid {prefix} regex at index {index}: {error}"),
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
        assert_eq!(policy.check("uname -a", false), None);
        // An anchored allow rule must not admit a second line.
        assert_eq!(
            policy.check("uname -a\nreboot", false),
            Some("shape.control_char")
        );
        assert_eq!(policy.check("uname\r", false), Some("shape.control_char"));
        assert_eq!(policy.check("uname\t", false), Some("shape.control_char"));
        assert_eq!(
            policy.check("uname\u{1b}[A", false),
            Some("shape.control_char")
        );
        assert_eq!(
            policy.check("uname\u{9b}A", false),
            Some("shape.control_char")
        );
        assert_eq!(
            policy.check("uname \\", false),
            Some("shape.line_continuation")
        );
        assert_eq!(
            policy.check("uname -a", true),
            Some("shape.line_ending_override")
        );
    }

    #[test]
    fn check_splits_cannot_assemble_a_denied_command() {
        let policy = CommandPolicy::compile(&[r"\breboot\b".into()], &[], &[], &[]).unwrap();
        // Fragments pass the regex individually; the shape gates stop the join.
        assert_eq!(
            policy.check("reb", true),
            Some("shape.line_ending_override")
        );
        assert_eq!(
            policy.check("reb\\", false),
            Some("shape.line_continuation")
        );
        assert_eq!(policy.check("reboot", false), Some("deny.global.0"));
    }

    #[test]
    fn check_imposes_nothing_without_a_policy() {
        let policy = CommandPolicy::default();
        assert_eq!(policy.check("a\nb\\", true), None);
    }

    #[test]
    fn invalid_regex_is_a_typed_error() {
        let err = CommandPolicy::compile(&["[".into()], &[], &[], &[]).unwrap_err();
        assert!(matches!(err, SerialError::InvalidParam { name, .. } if name == "command_policy"));
    }
}
