//! The pure part of the rule engine.

use crate::models::rule::{Condition, ConditionField, ConditionOp, Conjunction};
use crate::models::{Action, MessageHeader, Rule, RuleId, RuleSet};

use super::PlannedAction;

/// The result of checking one condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionCheck {
    /// The condition holds.
    Matched,
    /// The condition does not hold.
    NoMatch,
    /// The condition cannot be evaluated: wrong operator for the field, or a
    /// regular expression that does not compile.
    Invalid,
}

impl ConditionCheck {
    /// `true` for [`ConditionCheck::Matched`].
    #[must_use]
    pub const fn is_match(self) -> bool {
        matches!(self, Self::Matched)
    }

    /// `true` for [`ConditionCheck::Invalid`].
    #[must_use]
    pub const fn is_invalid(self) -> bool {
        matches!(self, Self::Invalid)
    }
}

/// What one rule did with a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMatch {
    /// Rule that matched.
    pub rule_id: RuleId,
    /// Rule name, copied here so the plan does not need the rule set.
    pub rule_name: String,
    /// Rule priority.
    pub priority: i32,
    /// The actions of the rule, in their declared order.
    pub actions: Vec<Action>,
    /// Whether the rule asked to stop the evaluation.
    pub stop_processing: bool,
}

/// The complete result of evaluating a rule set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleEvaluation {
    /// Rules that matched, in evaluation order.
    pub matched: Vec<RuleMatch>,
    /// Number of conditions that could not be evaluated at all.
    pub invalid_conditions: usize,
    /// `true` when the evaluation stopped early because of
    /// [`Action::StopProcessing`].
    pub stopped: bool,
    /// Number of rules that were looked at (excluding disabled ones).
    pub evaluated: usize,
}

impl RuleEvaluation {
    /// Flattens the matched rules into a priority-ordered action list.
    #[must_use]
    pub fn planned_actions(&self) -> Vec<PlannedAction> {
        let mut entries: Vec<PlannedAction> = self
            .matched
            .iter()
            .flat_map(|entry| {
                entry.actions.iter().cloned().map(|action| PlannedAction {
                    rule_id: entry.rule_id,
                    rule_name: entry.rule_name.clone(),
                    priority: entry.priority,
                    action,
                })
            })
            .collect();
        entries.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.rule_name.cmp(&right.rule_name))
        });
        entries
    }

    /// `true` when nothing matched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.matched.is_empty()
    }
}

/// Evaluates every enabled rule of a set against one message.
///
/// Disabled rules are skipped.  The evaluation stops after the first rule whose
/// actions contain [`Action::StopProcessing`], including that rule's other
/// actions.
///
/// `body` is the plain text of the message, if it is already available.  It is
/// only used by [`ConditionField::Combined`]; rules that need a body therefore
/// simply do not match while the body is still in the cache-miss state.
#[must_use]
pub fn evaluate(rules: &RuleSet, message: &MessageHeader, body: Option<&str>) -> RuleEvaluation {
    let mut result = RuleEvaluation::default();
    for rule in &rules.rules {
        if !rule.enabled {
            continue;
        }
        result.evaluated += 1;
        let (matched, invalid) = rule_matches(rule, message, body);
        result.invalid_conditions += invalid;
        if !matched {
            continue;
        }
        let stop_processing = rule
            .actions
            .iter()
            .any(|action| matches!(action, Action::StopProcessing));
        result.matched.push(RuleMatch {
            rule_id: rule.id,
            rule_name: rule.name.clone(),
            priority: rule.priority,
            actions: rule.actions.clone(),
            stop_processing,
        });
        if stop_processing {
            result.stopped = true;
            break;
        }
    }
    result
}

/// `true` when a single rule matches, plus the number of invalid conditions.
fn rule_matches(rule: &Rule, message: &MessageHeader, body: Option<&str>) -> (bool, usize) {
    if rule.conditions.is_empty() {
        // The documented behaviour: a rule without conditions matches every
        // message. It is how "stop processing" or "flag everything" is written.
        return (true, 0);
    }
    let mut invalid = 0;
    let mut matched_any = false;
    let mut matched_all = true;
    for condition in &rule.conditions {
        match check(condition, message, body) {
            ConditionCheck::Matched => {
                matched_any = true;
            }
            ConditionCheck::NoMatch => {
                matched_all = false;
            }
            ConditionCheck::Invalid => {
                invalid += 1;
                matched_all = false;
            }
        }
    }
    let matched = match rule.conjunction {
        Conjunction::And => matched_all && invalid == 0,
        Conjunction::Or => matched_any,
    };
    (matched, invalid)
}

/// Checks one condition. Public so the GUI can explain a rule in the editor.
#[must_use]
pub fn check(condition: &Condition, message: &MessageHeader, body: Option<&str>) -> ConditionCheck {
    match condition.field {
        ConditionField::HasAttachment => {
            let expected = truthy(&condition.value);
            let actual = message.has_attachments;
            match condition.op {
                ConditionOp::Equals | ConditionOp::Contains => bool_check(actual == expected),
                ConditionOp::DoesNotContain => bool_check(actual != expected),
                _ => ConditionCheck::Invalid,
            }
        }
        ConditionField::SizeKb => {
            let Ok(threshold) = condition.value.trim().parse::<u64>() else {
                return ConditionCheck::Invalid;
            };
            let size_kb = message.size_bytes / 1024;
            match condition.op {
                ConditionOp::GreaterThan => bool_check(size_kb > threshold),
                ConditionOp::LessThan => bool_check(size_kb < threshold),
                ConditionOp::Equals => bool_check(size_kb == threshold),
                _ => ConditionCheck::Invalid,
            }
        }
        ConditionField::From => textual(condition, address_text(&message.from)),
        ConditionField::ToCc => {
            let mut joined = address_text(&message.to);
            joined.push(' ');
            joined.push_str(&address_text(&message.cc));
            joined.push(' ');
            joined.push_str(&address_text(&message.bcc));
            textual(condition, joined)
        }
        ConditionField::Subject => textual(condition, message.subject.clone()),
        ConditionField::Combined => {
            let mut joined = message.subject.clone();
            joined.push(' ');
            joined.push_str(&address_text(&message.from));
            joined.push(' ');
            joined.push_str(&address_text(&message.to));
            joined.push(' ');
            joined.push_str(&address_text(&message.cc));
            if let Some(body) = body {
                joined.push(' ');
                joined.push_str(body);
            }
            textual(condition, joined)
        }
    }
}

/// `true` when the message matches the field the rules care about.
///
/// Convenience for the GUI's "Regeln prüfen" and for callers that only want the
/// boolean.
#[must_use]
pub fn matches_message(rules: &RuleSet, message: &MessageHeader, body: Option<&str>) -> bool {
    !evaluate(rules, message, body).is_empty()
}

/// Applies a textual operator, case-insensitively for everything but regex.
fn textual(condition: &Condition, haystack: String) -> ConditionCheck {
    let needle = condition.value.as_str();
    match condition.op {
        ConditionOp::Contains => bool_check(contains_ci(&haystack, needle)),
        ConditionOp::DoesNotContain => bool_check(!contains_ci(&haystack, needle)),
        ConditionOp::Equals => bool_check(haystack.trim().eq_ignore_ascii_case(needle.trim())),
        ConditionOp::StartsWith => bool_check(starts_ci(&haystack, needle)),
        ConditionOp::EndsWith => bool_check(ends_ci(&haystack, needle)),
        ConditionOp::Regex => match regex_lite::Regex::new(needle) {
            Ok(regex) => bool_check(regex.is_match(&haystack)),
            // A broken expression is reported, never fatal: a rule stored by
            // an older version must not stop the mail.
            Err(_) => ConditionCheck::Invalid,
        },
        ConditionOp::GreaterThan | ConditionOp::LessThan => ConditionCheck::Invalid,
    }
}

/// ASCII-case-insensitive `contains`, avoiding an allocation for ASCII input.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if !needle.is_ascii() || !haystack.is_ascii() {
        return haystack.to_lowercase().contains(&needle.to_lowercase());
    }
    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// ASCII-case-insensitive prefix check.
fn starts_ci(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len()
        && haystack.as_bytes()[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

/// ASCII-case-insensitive suffix check.
fn ends_ci(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len()
        && haystack.as_bytes()[haystack.len() - needle.len()..]
            .eq_ignore_ascii_case(needle.as_bytes())
}

/// `true`/`false` from the free-text value of a boolean condition.
fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "ja" | "yes" | "wahr"
    )
}

/// Wraps a boolean into a match result.
const fn bool_check(value: bool) -> ConditionCheck {
    if value {
        ConditionCheck::Matched
    } else {
        ConditionCheck::NoMatch
    }
}

/// Joins the display names and addresses of an address list.
fn address_text(addresses: &[crate::models::MailAddress]) -> String {
    let mut out = String::new();
    for address in addresses {
        if !out.is_empty() {
            out.push(' ');
        }
        if let Some(name) = &address.name {
            out.push_str(name);
            out.push(' ');
        }
        out.push_str(&address.email);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::rule::{Condition, ConditionField, ConditionOp, Conjunction};
    use crate::models::{AccountId, FolderId, MailAddress, Rule};

    fn message() -> MessageHeader {
        let mut message = MessageHeader::new(AccountId::new(), FolderId::new());
        message.from = vec![
            MailAddress::with_name("Chef@Example.org", "Der Chef"),
            MailAddress::new("zweiter@example.org"),
        ];
        message.to = vec![MailAddress::new("ich@example.org")];
        message.subject = "Rechnung 4711".to_string();
        message.size_bytes = 300 * 1024;
        message
    }

    fn rule(conditions: Vec<Condition>) -> Rule {
        Rule {
            id: RuleId::new(),
            name: "Test".to_string(),
            enabled: true,
            priority: 0,
            conjunction: Conjunction::And,
            conditions,
            actions: vec![Action::Flag],
            apply_on_arrival: true,
            created_at: crate::models::ids::utc_now(),
        }
    }

    #[test]
    fn text_fields_match_case_insensitively() {
        let header = message();
        for (field, value, expected) in [
            (ConditionField::From, "chef@example.org", true),
            (ConditionField::From, "CHEF@EXAMPLE.ORG", true),
            (ConditionField::From, "Der Chef", true),
            (ConditionField::From, "fremd", false),
            (ConditionField::Subject, "rechnung", true),
            (ConditionField::Subject, "4711", true),
            (ConditionField::ToCc, "ich@example.org", true),
        ] {
            let condition = Condition::contains(field, value);
            assert_eq!(
                check(&condition, &header, None).is_match(),
                expected,
                "{field:?} contains {value}"
            );
        }
    }

    #[test]
    fn operators_behave() {
        let header = message();
        assert!(check(
            &Condition::new(
                ConditionField::Subject,
                ConditionOp::Equals,
                "rechnung 4711"
            ),
            &header,
            None
        )
        .is_match());
        assert!(check(
            &Condition::new(ConditionField::Subject, ConditionOp::StartsWith, "Rech"),
            &header,
            None
        )
        .is_match());
        assert!(check(
            &Condition::new(ConditionField::Subject, ConditionOp::EndsWith, "4711"),
            &header,
            None
        )
        .is_match());
        assert!(check(
            &Condition::new(
                ConditionField::Subject,
                ConditionOp::DoesNotContain,
                "Angebot"
            ),
            &header,
            None
        )
        .is_match());
        assert!(check(
            &Condition::new(ConditionField::Subject, ConditionOp::Regex, r"Rechnung \d+"),
            &header,
            None
        )
        .is_match());
    }

    #[test]
    fn a_broken_regex_is_invalid_not_fatal() {
        let header = message();
        let condition = Condition::new(ConditionField::Subject, ConditionOp::Regex, "(unclosed");
        assert_eq!(check(&condition, &header, None), ConditionCheck::Invalid);
        let set = RuleSet {
            rules: vec![rule(vec![condition])],
        };
        let evaluation = evaluate(&set, &header, None);
        assert!(evaluation.is_empty());
        assert_eq!(evaluation.invalid_conditions, 1);
    }

    #[test]
    fn a_field_operator_mismatch_is_invalid() {
        let header = message();
        let condition = Condition::new(ConditionField::SizeKb, ConditionOp::Regex, "4711");
        assert_eq!(check(&condition, &header, None), ConditionCheck::Invalid);
        let condition = Condition::new(ConditionField::Subject, ConditionOp::GreaterThan, "3");
        assert_eq!(check(&condition, &header, None), ConditionCheck::Invalid);
    }

    #[test]
    fn numeric_conditions_compare_kilobytes() {
        let header = message();
        assert!(check(
            &Condition::new(ConditionField::SizeKb, ConditionOp::GreaterThan, "200"),
            &header,
            None
        )
        .is_match());
        assert!(
            !check(
                &Condition::new(ConditionField::SizeKb, ConditionOp::LessThan, "200"),
                &header,
                None
            )
            .is_match(),
            "less-than must not match a larger size"
        );
        assert_eq!(
            check(
                &Condition::new(ConditionField::SizeKb, ConditionOp::GreaterThan, "viel"),
                &header,
                None
            ),
            ConditionCheck::Invalid
        );
    }

    #[test]
    fn the_attachment_condition_reads_the_header() {
        let mut header = message();
        assert!(check(
            &Condition::new(ConditionField::HasAttachment, ConditionOp::Equals, "false"),
            &header,
            None
        )
        .is_match());
        header.has_attachments = true;
        assert!(check(
            &Condition::new(ConditionField::HasAttachment, ConditionOp::Contains, "ja"),
            &header,
            None
        )
        .is_match());
        assert!(
            !check(
                &Condition::new(
                    ConditionField::HasAttachment,
                    ConditionOp::DoesNotContain,
                    "true"
                ),
                &header,
                None
            )
            .is_match(),
            "does-not-contain must not match an attachment"
        );
    }

    #[test]
    fn all_requires_every_condition_and_any_requires_one() {
        let header = message();
        let matches = Condition::contains(ConditionField::Subject, "Rechnung");
        let misses = Condition::contains(ConditionField::Subject, "Angebot");

        let mut all = rule(vec![matches.clone(), misses.clone()]);
        all.conjunction = Conjunction::And;
        assert!(evaluate(&RuleSet { rules: vec![all] }, &header, None).is_empty());

        let mut any = rule(vec![matches, misses]);
        any.conjunction = Conjunction::Or;
        assert!(!evaluate(&RuleSet { rules: vec![any] }, &header, None).is_empty());
    }

    #[test]
    fn a_rule_without_conditions_matches_everything() {
        let header = message();
        let set = RuleSet {
            rules: vec![rule(Vec::new())],
        };
        assert_eq!(evaluate(&set, &header, None).matched.len(), 1);
        assert!(matches_message(&set, &header, None));
    }

    #[test]
    fn the_combined_field_sees_the_body_when_it_is_available() {
        let header = message();
        let condition = Condition::contains(ConditionField::Combined, "Sonderzeichen");
        assert_eq!(check(&condition, &header, None), ConditionCheck::NoMatch);
        assert!(check(&condition, &header, Some("ein Sonderzeichen im Text")).is_match());
    }

    #[test]
    fn disabled_rules_are_not_evaluated() {
        let header = message();
        let mut disabled = rule(Vec::new());
        disabled.enabled = false;
        let evaluation = evaluate(
            &RuleSet {
                rules: vec![disabled],
            },
            &header,
            None,
        );
        assert_eq!(evaluation.evaluated, 0);
        assert!(evaluation.is_empty());
    }

    #[test]
    fn stop_processing_truncates_the_result() {
        let header = message();
        let mut first = rule(Vec::new());
        first.actions = vec![Action::StopProcessing, Action::Flag];
        let second = rule(Vec::new());
        let evaluation = evaluate(
            &RuleSet {
                rules: vec![first, second],
            },
            &header,
            None,
        );
        assert_eq!(evaluation.matched.len(), 1);
        assert!(evaluation.stopped);
        assert_eq!(evaluation.evaluated, 1);
    }

    #[test]
    fn planned_actions_are_sorted_by_priority() {
        let mut low = rule(Vec::new());
        low.priority = 9;
        low.name = "später".to_string();
        low.actions = vec![Action::MarkRead];
        let mut high = rule(Vec::new());
        high.priority = 1;
        high.name = "früh".to_string();
        high.actions = vec![Action::Flag];
        let evaluation = evaluate(
            &RuleSet {
                rules: vec![low, high],
            },
            &message(),
            None,
        );
        let actions = evaluation.planned_actions();
        assert_eq!(actions[0].rule_name, "früh");
        assert_eq!(actions[1].rule_name, "später");
    }

    #[test]
    fn empty_and_unicode_needles_do_not_panic() {
        assert!(contains_ci("Grüße", ""));
        assert!(!contains_ci("Grüße", "GRÜSSE"));
        assert!(contains_ci("Grüße aus München", "grüße"));
        // `Ü` is not ASCII, so the ASCII fast path steps aside and the
        // lowercase fallback handles the case folding correctly.
        assert!(contains_ci("München", "MÜNCHEN"));
        assert!(contains_ci("MÜNCHEN", "münchen"));
        assert!(!starts_ci("Über", "ü"));
        assert!(ends_ci("Über", "R"));
        assert!(ends_ci("Über", "ber"));
    }
}
