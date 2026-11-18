//! The rule engine: a pure evaluator plus the plan it produces.
//!
//! Rules are data (see [`crate::models::rule`]); this module decides what they
//! mean for one message.  The split matters:
//!
//! * [`evaluate`] is *pure*. It looks at a message and returns which rules
//!   matched and what should happen. It never touches the network, the
//!   database or the clock. That makes it cheap to run over every arriving
//!   message and trivial to test.
//! * [`RulePlan`] is the merged result: the caller applies it. The engine, the
//!   CLI and the GUI all apply the same plan the same way.
//!
//! ## Matching rules
//!
//! * A rule with **no conditions matches every message**. That is the
//!   documented behaviour and the reason `StopProcessing` alone is useful.
//! * `Conjunction::All` requires every condition, `Conjunction::Any` one.
//! * A condition with a **field/operator mismatch** (a regex on `SizeKb`, a
//!   `greater than` on `Subject`) never matches. It is reported as
//!   [`RuleEvaluation::invalid_conditions`] instead of failing the whole run:
//!   one broken rule must not stop the mail.
//! * A **regular expression that does not compile** never matches, for the same
//!   reason. Compilation is attempted per evaluation and the result is *not*
//!   cached in the plan, so the evaluator stays stateless.
//!
//! ## Applying a plan
//!
//! [`RulePlan`] folds the matched actions together. Conflicting actions are
//! resolved by **the lowest rule priority wins** (`MoveTo`), and the remaining
//! flags are additive. `StopProcessing` stops the *evaluation*, so later rules
//! do not even run.

mod evaluate;

pub use evaluate::{evaluate, matches_message, ConditionCheck, RuleEvaluation};

use crate::models::{Action, RuleId, RuleSet};

/// One action in a plan, together with the rule that asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAction {
    /// Rule that produced the action.
    pub rule_id: RuleId,
    /// Rule name, for the activity log.
    pub rule_name: String,
    /// Priority of the rule; lower wins.
    pub priority: i32,
    /// The action itself.
    pub action: Action,
}

/// The merged outcome of evaluating a rule set against one message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RulePlan {
    /// The destination folder, if any rule asked for one.
    pub move_to: Option<String>,
    /// Mark the message as read.
    pub mark_read: bool,
    /// Set the flagged flag.
    pub flag: bool,
    /// Move the message to the trash.
    pub delete: bool,
    /// Actions in order, for auditing and for the activity log.
    pub actions: Vec<PlannedAction>,
    /// `true` when the plan changes nothing at all.
    pub matched_nothing: bool,
}

impl RulePlan {
    /// Builds the plan from a set of matched actions.
    ///
    /// The inputs must already be sorted by priority; the caller gets them
    /// that way from [`RuleSet::sort`].
    #[must_use]
    pub fn from_actions(actions: Vec<PlannedAction>) -> Self {
        let mut plan = Self {
            matched_nothing: actions.is_empty(),
            actions,
            ..Self::default()
        };
        for entry in &plan.actions {
            match &entry.action {
                Action::MoveTo { folder_path } => {
                    // A `Delete` is stronger than a move, and the first (=
                    // highest priority) move wins over a later one.
                    if plan.move_to.is_none() && !plan.delete {
                        plan.move_to = Some(folder_path.clone());
                    }
                }
                Action::MarkRead => plan.mark_read = true,
                Action::Flag => plan.flag = true,
                Action::Delete => {
                    plan.delete = true;
                    plan.move_to = None;
                }
                Action::StopProcessing => {}
            }
        }
        plan
    }

    /// `true` when the plan asks for anything at all.
    #[must_use]
    pub fn is_effective(&self) -> bool {
        self.move_to.is_some() || self.mark_read || self.flag || self.delete
    }

    /// A one-line summary for the activity log, in German.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.matched_nothing {
            return "Keine Regel traf zu".to_string();
        }
        let mut parts = Vec::new();
        if let Some(path) = &self.move_to {
            parts.push(format!("verschieben nach `{path}`"));
        }
        if self.delete {
            parts.push("löschen".to_string());
        }
        if self.mark_read {
            parts.push("gelesen markieren".to_string());
        }
        if self.flag {
            parts.push("markieren".to_string());
        }
        if parts.is_empty() {
            return format!("{} Regel(n) ohne Wirkung", self.actions.len());
        }
        parts.join(", ")
    }

    /// The names of the rules that matched, in evaluation order.
    #[must_use]
    pub fn rule_names(&self) -> Vec<&str> {
        self.actions
            .iter()
            .map(|entry| entry.rule_name.as_str())
            .collect()
    }
}

/// Evaluates a whole rule set against a message and merges the actions.
///
/// This is the entry point the sync engine, the CLI and the GUI use.  It is
/// deliberately infallible: the worst a broken rule can do is not match.
#[must_use]
pub fn plan(
    rules: &RuleSet,
    message: &crate::models::MessageHeader,
    body: Option<&str>,
) -> RulePlan {
    let evaluation = evaluate(rules, message, body);
    RulePlan::from_actions(evaluation.planned_actions())
}

/// Applies only the rules whose `apply_on_arrival` flag is set.
///
/// The engine calls this for messages that just arrived; the "Regeln jetzt
/// anwenden" command in the GUI calls [`plan`] for everything.
#[must_use]
pub fn plan_on_arrival(
    rules: &RuleSet,
    message: &crate::models::MessageHeader,
    body: Option<&str>,
) -> RulePlan {
    let arrival: RuleSet = RuleSet {
        rules: rules
            .rules
            .iter()
            .filter(|rule| rule.apply_on_arrival)
            .cloned()
            .collect(),
    };
    plan(&arrival, message, body)
}

/// The rules that would match, for the "Regeln prüfen" preview in the GUI.
#[must_use]
pub fn preview(
    rules: &RuleSet,
    message: &crate::models::MessageHeader,
    body: Option<&str>,
) -> Vec<(RuleId, String, RulePlan)> {
    let evaluation = evaluate(rules, message, body);
    evaluation
        .matched
        .iter()
        .filter_map(|entry| {
            let rule = rules.rules.iter().find(|rule| rule.id == entry.rule_id)?;
            Some((
                rule.id,
                rule.name.clone(),
                RulePlan::from_actions(vec![PlannedAction {
                    rule_id: rule.id,
                    rule_name: rule.name.clone(),
                    priority: rule.priority,
                    action: Action::StopProcessing,
                }]),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::rule::ConditionField;
    use crate::models::{AccountId, FolderId, MessageHeader, Rule};

    fn rule(name: &str, priority: i32, actions: Vec<Action>) -> Rule {
        Rule {
            id: RuleId::new(),
            name: name.to_string(),
            enabled: true,
            priority,
            conjunction: crate::models::rule::Conjunction::And,
            conditions: vec![crate::models::rule::Condition::contains(
                ConditionField::From,
                "chef@example.org",
            )],
            actions,
            apply_on_arrival: true,
            created_at: crate::models::ids::utc_now(),
        }
    }

    fn message() -> MessageHeader {
        let mut message = MessageHeader::new(AccountId::new(), FolderId::new());
        message.from = vec![crate::models::MailAddress::new("chef@example.org")];
        message.subject = "Rechnung 4711".to_string();
        message
    }

    #[test]
    fn an_empty_rule_set_changes_nothing() {
        let plan = plan(&RuleSet::new(), &message(), None);
        assert!(plan.matched_nothing);
        assert!(!plan.is_effective());
        assert_eq!(plan.describe(), "Keine Regel traf zu");
    }

    #[test]
    fn a_matching_rule_moves_the_message() {
        let mut rules = RuleSet::new();
        rules.rules.push(rule(
            "Chef",
            0,
            vec![Action::MoveTo {
                folder_path: "Chef".to_string(),
            }],
        ));
        let plan = plan(&rules, &message(), None);
        assert_eq!(plan.move_to.as_deref(), Some("Chef"));
        assert!(plan.is_effective());
        assert_eq!(plan.rule_names(), vec!["Chef"]);
        assert!(plan.describe().contains("verschieben nach `Chef`"));
    }

    #[test]
    fn a_non_matching_rule_is_ignored() {
        let mut rules = RuleSet::new();
        rules.rules.push(rule(
            "Chef",
            0,
            vec![Action::MoveTo {
                folder_path: "Chef".to_string(),
            }],
        ));
        let mut other = message();
        other.from = vec![crate::models::MailAddress::new("fremd@example.org")];
        assert!(plan(&rules, &other, None).matched_nothing);
    }

    #[test]
    fn the_higher_priority_move_wins() {
        let mut rules = RuleSet::new();
        rules.rules.push(rule(
            "zweitrangig",
            10,
            vec![Action::MoveTo {
                folder_path: "Spam".to_string(),
            }],
        ));
        rules.rules.push(rule(
            "wichtig",
            1,
            vec![Action::MoveTo {
                folder_path: "Wichtig".to_string(),
            }],
        ));
        rules.sort();
        let plan = plan(&rules, &message(), None);
        assert_eq!(plan.move_to.as_deref(), Some("Wichtig"));
    }

    #[test]
    fn delete_beats_move_to() {
        let mut rules = RuleSet::new();
        rules.rules.push(rule(
            "verschieben",
            0,
            vec![Action::MoveTo {
                folder_path: "Archiv".to_string(),
            }],
        ));
        rules.rules.push(rule("löschen", 1, vec![Action::Delete]));
        rules.sort();
        let plan = plan(&rules, &message(), None);
        assert!(plan.delete);
        assert_eq!(plan.move_to, None);
    }

    #[test]
    fn flags_accumulate_across_rules() {
        let mut rules = RuleSet::new();
        rules.rules.push(rule("lesen", 0, vec![Action::MarkRead]));
        rules.rules.push(rule("flagge", 1, vec![Action::Flag]));
        rules.sort();
        let plan = plan(&rules, &message(), None);
        assert!(plan.mark_read);
        assert!(plan.flag);
        assert_eq!(plan.actions.len(), 2);
    }

    #[test]
    fn stop_processing_halts_the_evaluation() {
        let mut rules = RuleSet::new();
        let mut first = rule("stopp", 0, vec![Action::StopProcessing, Action::Flag]);
        first.conditions.clear();
        rules.rules.push(first);
        let mut second = rule(
            "danach",
            1,
            vec![Action::MoveTo {
                folder_path: "Nie".to_string(),
            }],
        );
        second.conditions.clear();
        rules.rules.push(second);
        rules.sort();
        let plan = plan(&rules, &message(), None);
        assert!(plan.flag);
        assert_eq!(plan.move_to, None);
        // The rule asked for two actions, so its name is reported twice: the
        // list is an action list, not a rule list.
        assert_eq!(plan.rule_names(), vec!["stopp", "stopp"]);
    }

    #[test]
    fn a_disabled_rule_is_skipped() {
        let mut rules = RuleSet::new();
        let mut disabled = rule("aus", 0, vec![Action::Flag]);
        disabled.enabled = false;
        rules.rules.push(disabled);
        assert!(plan(&rules, &message(), None).matched_nothing);
    }

    #[test]
    fn on_arrival_skips_manual_rules() {
        let mut rules = RuleSet::new();
        let mut manual = rule("nur manuell", 0, vec![Action::Flag]);
        manual.apply_on_arrival = false;
        rules.rules.push(manual);
        assert!(plan_on_arrival(&rules, &message(), None).matched_nothing);
        assert!(plan(&rules, &message(), None).flag);
    }

    #[test]
    fn the_preview_lists_the_matching_rules() {
        let mut rules = RuleSet::new();
        rules.rules.push(rule("Chef", 0, vec![Action::Flag]));
        rules
            .rules
            .push(rule("Chef auch", 1, vec![Action::MarkRead]));
        let preview = preview(&rules, &message(), None);
        assert_eq!(preview.len(), 2);
        assert_eq!(preview[0].1, "Chef");
    }

    #[test]
    fn actions_without_effect_are_reported_as_such() {
        let plan = RulePlan::from_actions(vec![PlannedAction {
            rule_id: RuleId::new(),
            rule_name: "stopp".to_string(),
            priority: 0,
            action: Action::StopProcessing,
        }]);
        assert!(!plan.is_effective());
        assert!(plan.describe().contains("ohne Wirkung"));
    }
}
