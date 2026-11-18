//! The automatic rule engine.
//!
//! Rules are evaluated locally - either for every message that arrives in an
//! inbox folder, or manually against an existing folder ("Regeln auf Ordner
//! anwenden").  The data model below is deliberately declarative so that the
//! evaluator (phase 2) and any future rule editor can share it without
//! duplicating semantics.
//!
//! GroupWise proxy accounts never evaluate rules, see
//! [`crate::models::AccountKind::supports_rules`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::models::ids::{utc_now, RuleId};

/// Field a condition inspects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionField {
    /// The `From` header.
    From,
    /// `To`, `Cc` and `Bcc` combined.
    ToCc,
    /// The subject line.
    Subject,
    /// From, To, Cc, subject and body combined ("Kombi").
    Combined,
    /// Presence of at least one attachment.
    HasAttachment,
    /// Size of the message in kilobytes.
    SizeKb,
}

impl ConditionField {
    /// Label shown in the rule editor.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::From => "Von",
            Self::ToCc => "An/Cc",
            Self::Subject => "Betreff",
            Self::Combined => "Kombination",
            Self::HasAttachment => "Hat Anhang",
            Self::SizeKb => "Größe",
        }
    }

    /// `true` when the field is a free-text field that needs an operator.
    #[must_use]
    pub const fn is_textual(self) -> bool {
        matches!(
            self,
            Self::From | Self::ToCc | Self::Subject | Self::Combined
        )
    }

    /// All fields that can be offered for a new condition.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::From,
            Self::ToCc,
            Self::Subject,
            Self::Combined,
            Self::HasAttachment,
            Self::SizeKb,
        ]
    }
}

/// Comparison operator of a condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOp {
    /// Substring match.
    Contains,
    /// Negated substring match.
    DoesNotContain,
    /// Exact match, case insensitive.
    Equals,
    /// Prefix match.
    StartsWith,
    /// Suffix match.
    EndsWith,
    /// Regular expression match (`regex-lite` syntax).
    Regex,
    /// Numeric greater-than.
    GreaterThan,
    /// Numeric less-than.
    LessThan,
}

impl ConditionOp {
    /// Label shown in the rule editor.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Contains => "enthält",
            Self::DoesNotContain => "enthält nicht",
            Self::Equals => "ist gleich",
            Self::StartsWith => "beginnt mit",
            Self::EndsWith => "endet mit",
            Self::Regex => "regulärer Ausdruck",
            Self::GreaterThan => "größer als",
            Self::LessThan => "kleiner als",
        }
    }

    /// Operators that make sense for a textual field.
    #[must_use]
    pub const fn textual() -> &'static [Self] {
        &[
            Self::Contains,
            Self::DoesNotContain,
            Self::Equals,
            Self::StartsWith,
            Self::EndsWith,
            Self::Regex,
        ]
    }

    /// Operators that make sense for a numeric field.
    #[must_use]
    pub const fn numeric() -> &'static [Self] {
        &[Self::GreaterThan, Self::LessThan]
    }
}

/// A single condition of a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    /// Field to inspect.
    pub field: ConditionField,
    /// Comparison to apply.
    pub op: ConditionOp,
    /// Comparison value; for [`ConditionField::SizeKb`] a decimal number.
    pub value: String,
}

impl Condition {
    /// Creates a condition.
    #[must_use]
    pub fn new(field: ConditionField, op: ConditionOp, value: impl Into<String>) -> Self {
        Self {
            field,
            op,
            value: value.into(),
        }
    }

    /// Convenience constructor for the most common case.
    #[must_use]
    pub fn contains(field: ConditionField, value: impl Into<String>) -> Self {
        Self::new(field, ConditionOp::Contains, value)
    }

    /// Validates the condition, compiling the regular expression if needed.
    pub fn validate(&self) -> Result<()> {
        if self.value.trim().is_empty() && self.field.is_textual() {
            return Err(Error::InvalidSettings {
                field: "rule.condition.value",
                message: "a text condition needs a comparison value".to_string(),
            });
        }
        match self.field {
            ConditionField::SizeKb => {
                if self.value.trim().parse::<f64>().is_err() {
                    return Err(Error::InvalidSettings {
                        field: "rule.condition.value",
                        message: format!("`{}` is not a number", self.value),
                    });
                }
                if !ConditionOp::numeric().contains(&self.op) {
                    return Err(Error::InvalidSettings {
                        field: "rule.condition.op",
                        message: format!(
                            "operator `{}` cannot be used on a size condition",
                            self.op.label()
                        ),
                    });
                }
            }
            ConditionField::HasAttachment => {
                // No value is needed, the presence of the header is the test.
            }
            _ => {
                if self.op == ConditionOp::Regex {
                    regex_lite::Regex::new(&self.value).map_err(|error| {
                        Error::InvalidSettings {
                            field: "rule.condition.value",
                            message: format!("invalid regular expression: {error}"),
                        }
                    })?;
                }
                if !ConditionOp::textual().contains(&self.op) {
                    return Err(Error::InvalidSettings {
                        field: "rule.condition.op",
                        message: format!(
                            "operator `{}` cannot be used on a text condition",
                            self.op.label()
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Parses the numeric value of a size condition.
    #[must_use]
    pub fn numeric_value(&self) -> Option<f64> {
        self.value.trim().parse::<f64>().ok()
    }
}

/// How several conditions are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conjunction {
    /// Every condition must match.
    #[default]
    And,
    /// At least one condition must match.
    Or,
}

impl Conjunction {
    /// Label shown in the rule editor.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::And => "UND",
            Self::Or => "ODER",
        }
    }
}

/// Something a rule does when it matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Move the message into the folder with the given full path.
    MoveTo {
        /// Full path of the destination folder as reported by the server.
        folder_path: String,
    },
    /// Mark the message as read.
    MarkRead,
    /// Set the `\Flagged` flag.
    Flag,
    /// Move the message to the trash folder.
    Delete,
    /// Stop evaluating further rules for this message.
    StopProcessing,
}

impl Action {
    /// Label shown in the rule editor.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::MoveTo { folder_path } => format!("Verschieben nach `{folder_path}`"),
            Self::MarkRead => "Gelesen markieren".to_string(),
            Self::Flag => "Markieren".to_string(),
            Self::Delete => "Löschen".to_string(),
            Self::StopProcessing => "Verarbeitung stoppen".to_string(),
        }
    }

    /// Validates the action.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::MoveTo { folder_path } if folder_path.trim().is_empty() => {
                Err(Error::InvalidSettings {
                    field: "rule.action.folder_path",
                    message: "a move action needs a destination folder".to_string(),
                })
            }
            _ => Ok(()),
        }
    }
}

/// A complete rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// Local identifier.
    pub id: RuleId,
    /// Name shown in the rule list.
    pub name: String,
    /// Disabled rules are skipped by the evaluator.
    #[serde(default = "crate::models::rule::default_true")]
    pub enabled: bool,
    /// Priority; lower values are evaluated first.
    #[serde(default)]
    pub priority: i32,
    /// How [`Rule::conditions`] are combined.
    #[serde(default)]
    pub conjunction: Conjunction,
    /// Conditions; an empty list matches every message.
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// Actions executed on a match, in order.
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Apply the rule automatically to messages arriving in an inbox.
    #[serde(default = "crate::models::rule::default_true")]
    pub apply_on_arrival: bool,
    /// Creation timestamp.
    #[serde(default = "utc_now")]
    pub created_at: DateTime<Utc>,
}

/// Serde default helper; public because `#[serde(default = "...")]` needs it.
#[must_use]
pub fn default_true() -> bool {
    true
}

impl Rule {
    /// Creates an enabled rule without conditions or actions.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: RuleId::new(),
            name: name.into(),
            enabled: true,
            priority: 0,
            conjunction: Conjunction::And,
            conditions: Vec::new(),
            actions: Vec::new(),
            apply_on_arrival: true,
            created_at: Utc::now(),
        }
    }

    /// Validates the whole rule.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::InvalidSettings {
                field: "rule.name",
                message: "a rule needs a name".to_string(),
            });
        }
        for condition in &self.conditions {
            condition.validate()?;
        }
        for action in &self.actions {
            action.validate()?;
        }
        Ok(())
    }

    /// `true` when the rule would actually do something.
    #[must_use]
    pub fn is_effective(&self) -> bool {
        !self.actions.is_empty()
            && self
                .actions
                .iter()
                .any(|a| !matches!(a, Action::StopProcessing))
    }

    /// `true` when the rule stops further processing.
    #[must_use]
    pub fn stops_processing(&self) -> bool {
        self.actions
            .iter()
            .any(|action| matches!(action, Action::StopProcessing))
    }

    /// Returns the destination folder of the first move action, if any.
    #[must_use]
    pub fn move_target(&self) -> Option<&str> {
        self.actions.iter().find_map(|action| match action {
            Action::MoveTo { folder_path } => Some(folder_path.as_str()),
            _ => None,
        })
    }
}

/// The ordered collection of rules stored in `rules.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSet {
    /// Rules in evaluation order.
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// Creates an empty rule set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sort order used by the evaluator: ascending priority, stable by name.
    pub fn sort(&mut self) {
        self.rules.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.name.cmp(&right.name))
        });
    }

    /// Returns the enabled rules in evaluation order.
    #[must_use]
    pub fn enabled_in_order(&self) -> Vec<&Rule> {
        let mut rules: Vec<&Rule> = self.rules.iter().filter(|rule| rule.enabled).collect();
        rules.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.name.cmp(&right.name))
        });
        rules
    }

    /// Adds a rule after validating it.
    pub fn add(&mut self, rule: Rule) -> Result<RuleId> {
        rule.validate()?;
        let id = rule.id;
        self.rules.push(rule);
        Ok(id)
    }

    /// Removes a rule and reports whether it existed.
    pub fn remove(&mut self, id: RuleId) -> bool {
        let before = self.rules.len();
        self.rules.retain(|rule| rule.id != id);
        self.rules.len() != before
    }

    /// Finds a rule by id.
    #[must_use]
    pub fn get(&self, id: RuleId) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.id == id)
    }

    /// Mutable lookup.
    pub fn get_mut(&mut self, id: RuleId) -> Option<&mut Rule> {
        self.rules.iter_mut().find(|rule| rule.id == id)
    }

    /// Validates every rule in the set.
    pub fn validate(&self) -> Result<()> {
        for rule in &self.rules {
            rule.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_condition_requires_a_value() {
        let condition = Condition::new(ConditionField::Subject, ConditionOp::Contains, "   ");
        assert!(condition.validate().is_err());
    }

    #[test]
    fn size_condition_requires_a_number() {
        let condition = Condition::new(ConditionField::SizeKb, ConditionOp::GreaterThan, "12");
        condition.validate().unwrap();
        assert_eq!(condition.numeric_value(), Some(12.0));

        let broken = Condition::new(ConditionField::SizeKb, ConditionOp::GreaterThan, "big");
        assert!(broken.validate().is_err());
    }

    #[test]
    fn size_condition_rejects_text_operator() {
        let condition = Condition::new(ConditionField::SizeKb, ConditionOp::Contains, "12");
        assert!(condition.validate().is_err());
    }

    #[test]
    fn text_condition_rejects_numeric_operator() {
        let condition = Condition::new(ConditionField::Subject, ConditionOp::GreaterThan, "x");
        assert!(condition.validate().is_err());
    }

    #[test]
    fn regex_conditions_are_compiled_during_validation() {
        let good = Condition::new(ConditionField::Subject, ConditionOp::Regex, r"^\[spam\]");
        good.validate().unwrap();
        let bad = Condition::new(ConditionField::Subject, ConditionOp::Regex, "(unclosed");
        assert!(bad.validate().is_err());
    }

    #[test]
    fn has_attachment_needs_no_value() {
        let condition = Condition::new(ConditionField::HasAttachment, ConditionOp::Equals, "");
        condition.validate().unwrap();
    }

    #[test]
    fn rule_validation_checks_name_conditions_and_actions() {
        let mut rule = Rule::new("");
        assert!(rule.validate().is_err());
        rule.name = "Rechnungen".to_string();
        rule.validate().unwrap();

        rule.conditions
            .push(Condition::contains(ConditionField::From, "billing@"));
        rule.actions.push(Action::MoveTo {
            folder_path: "Rechnungen".to_string(),
        });
        rule.validate().unwrap();

        rule.actions.push(Action::MoveTo {
            folder_path: "   ".to_string(),
        });
        assert!(rule.validate().is_err());
    }

    #[test]
    fn action_labels_are_human_readable() {
        assert_eq!(
            Action::MoveTo {
                folder_path: "Archiv".to_string()
            }
            .label(),
            "Verschieben nach `Archiv`"
        );
        assert_eq!(Action::MarkRead.label(), "Gelesen markieren");
    }

    #[test]
    fn rule_effectiveness_and_move_target() {
        let mut rule = Rule::new("x");
        assert!(!rule.is_effective());
        rule.actions.push(Action::StopProcessing);
        assert!(!rule.is_effective());
        assert!(rule.stops_processing());
        rule.actions.push(Action::MoveTo {
            folder_path: "Archiv".to_string(),
        });
        assert!(rule.is_effective());
        assert_eq!(rule.move_target(), Some("Archiv"));
    }

    #[test]
    fn rule_set_orders_by_priority() {
        let mut set = RuleSet::new();
        let mut low = Rule::new("low");
        low.priority = 10;
        let mut high = Rule::new("high");
        high.priority = -5;
        set.add(low).unwrap();
        set.add(high).unwrap();
        let ordered = set.enabled_in_order();
        assert_eq!(ordered[0].name, "high");
        assert_eq!(ordered[1].name, "low");
    }

    #[test]
    fn rule_set_skips_disabled_rules() {
        let mut set = RuleSet::new();
        let mut rule = Rule::new("off");
        rule.enabled = false;
        set.add(rule).unwrap();
        assert!(set.enabled_in_order().is_empty());
    }

    #[test]
    fn rule_set_add_remove_get() {
        let mut set = RuleSet::new();
        let id = set.add(Rule::new("a")).unwrap();
        assert!(set.get(id).is_some());
        assert!(set.get_mut(id).is_some());
        assert!(set.remove(id));
        assert!(!set.remove(id));
        assert!(set.get(id).is_none());
    }

    #[test]
    fn rule_set_rejects_invalid_rules() {
        let mut set = RuleSet::new();
        assert!(set.add(Rule::new("")).is_err());
        assert!(set.rules.is_empty());
    }

    #[test]
    fn rules_round_trip_through_json() {
        let mut rule = Rule::new("Alle Rechnungen");
        rule.conjunction = Conjunction::Or;
        rule.conditions.push(Condition::new(
            ConditionField::Combined,
            ConditionOp::Regex,
            "rechnung|invoice",
        ));
        rule.actions.push(Action::MoveTo {
            folder_path: "Rechnungen".to_string(),
        });
        rule.actions.push(Action::StopProcessing);

        let json = serde_json::to_string_pretty(&rule).unwrap();
        let back: Rule = serde_json::from_str(&json).unwrap();
        assert_eq!(rule, back);

        let mut set = RuleSet::new();
        set.add(rule).unwrap();
        let json = serde_json::to_string(&set).unwrap();
        let back: RuleSet = serde_json::from_str(&json).unwrap();
        assert_eq!(set, back);
    }
}
