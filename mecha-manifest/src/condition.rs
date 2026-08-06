//! Declarative conditions — the one arm of a two-arm union we keep.
//!
//! The form system this borrows from allows a step's `showWhen`, `validate`,
//! `skipWhen` and `canProceed` to be *either* a declarative condition or an
//! arbitrary closure. A closure cannot cross to a Rust server, and the server
//! has to evaluate exactly the rules the browser did — a client-side check is a
//! convenience and never a control. So the manifest takes only
//! `field`/`operator`/`value`, and the same struct is read by the Rust
//! evaluator here and by a few hundred lines of vanilla JavaScript in the
//! browser.
//!
//! The operator set is deliberately small and **total**: every operator has a
//! defined answer for a missing field and for a value of the wrong type, and
//! that answer is always "the condition does not hold". A condition that
//! errored would leave the browser and the server free to disagree about what
//! happened, which is the failure this whole shape exists to prevent.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One condition: read a field's submitted value, compare it to a constant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// The field whose value is read. Never an expression.
    pub field: String,
    #[serde(rename = "op")]
    pub operator: Operator,
    /// The constant compared against. Unused by `present`/`absent`, and
    /// omitting it there is not an error — a condition is data, and refusing to
    /// load a manifest over an ignored key buys nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    Eq,
    Ne,
    /// The field's value is one of a list.
    In,
    NotIn,
    /// Numeric comparisons. A non-numeric value on either side does not hold.
    Gt,
    Gte,
    Lt,
    Lte,
    /// Submitted, and not empty. An empty string is absent — a text input that
    /// was shown and left blank submits `""`, and treating that as present
    /// would make every optional field satisfy its own dependants.
    Present,
    Absent,
    /// A checkbox that is ticked. Distinct from `present`, which a `false`
    /// checkbox also satisfies.
    IsTrue,
    IsFalse,
}

impl Condition {
    /// Whether this condition holds for a submission.
    ///
    /// Total by construction: a missing field, a value of the wrong type, and a
    /// malformed comparison all answer `false` rather than erroring. The
    /// JavaScript evaluator must match this exactly, including the cases that
    /// look like mistakes.
    pub fn holds(&self, values: &serde_json::Map<String, Value>) -> bool {
        let actual = values.get(&self.field);
        match self.operator {
            Operator::Present => is_present(actual),
            Operator::Absent => !is_present(actual),
            Operator::IsTrue => actual == Some(&Value::Bool(true)),
            Operator::IsFalse => actual == Some(&Value::Bool(false)),
            Operator::Eq => actual.is_some() && actual == self.value.as_ref(),
            // Note the asymmetry, which is deliberate: `ne` against a field
            // that was never submitted does *not* hold. Otherwise every
            // condition of the form "x is not 'other'" would fire on a form
            // where x was never shown, which is the opposite of the intent.
            Operator::Ne => actual.is_some() && actual != self.value.as_ref(),
            Operator::In => match (&actual, &self.value) {
                (Some(a), Some(Value::Array(list))) => list.contains(a),
                _ => false,
            },
            Operator::NotIn => match (&actual, &self.value) {
                (Some(a), Some(Value::Array(list))) => !list.contains(a),
                _ => false,
            },
            Operator::Gt | Operator::Gte | Operator::Lt | Operator::Lte => {
                match (number(actual), number(self.value.as_ref())) {
                    (Some(a), Some(b)) => match self.operator {
                        Operator::Gt => a > b,
                        Operator::Gte => a >= b,
                        Operator::Lt => a < b,
                        Operator::Lte => a <= b,
                        _ => unreachable!("outer match narrowed to the four"),
                    },
                    _ => false,
                }
            }
        }
    }
}

fn is_present(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        _ => true,
    }
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        // A form POSTs strings. Parsing one here is what lets a numeric
        // condition work on the raw body without the caller having to coerce
        // first — and coercion happening in two places is how the browser and
        // the server come to disagree.
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values(pairs: Value) -> serde_json::Map<String, Value> {
        pairs.as_object().unwrap().clone()
    }

    fn cond(field: &str, operator: Operator, value: Option<Value>) -> Condition {
        Condition {
            field: field.into(),
            operator,
            value,
        }
    }

    #[test]
    fn present_treats_a_blank_string_as_absent() {
        let v = values(json!({"a": "", "b": "  ", "c": "x", "d": null, "e": []}));
        for (field, expected) in [
            ("a", false),
            ("b", false),
            ("c", true),
            ("d", false),
            ("e", false),
            ("missing", false),
        ] {
            assert_eq!(
                cond(field, Operator::Present, None).holds(&v),
                expected,
                "present({field})"
            );
            assert_eq!(
                cond(field, Operator::Absent, None).holds(&v),
                !expected,
                "absent({field})"
            );
        }
    }

    /// The asymmetry worth having a test on: `ne` against a field that was
    /// never submitted does not hold, or "x is not 'other'" would fire on every
    /// form where x was never shown.
    #[test]
    fn ne_against_an_unsubmitted_field_does_not_hold() {
        let v = values(json!({"kind": "talk"}));
        assert!(cond("kind", Operator::Ne, Some(json!("other"))).holds(&v));
        assert!(!cond("absent", Operator::Ne, Some(json!("other"))).holds(&v));
        assert!(!cond("kind", Operator::Eq, Some(json!("other"))).holds(&v));
        assert!(cond("kind", Operator::Eq, Some(json!("talk"))).holds(&v));
    }

    #[test]
    fn a_checkbox_distinguishes_is_true_from_present() {
        let v = values(json!({"consent": false}));
        assert!(cond("consent", Operator::Present, None).holds(&v));
        assert!(!cond("consent", Operator::IsTrue, None).holds(&v));
        assert!(cond("consent", Operator::IsFalse, None).holds(&v));
    }

    /// A form POSTs strings, so a numeric comparison has to work on one — and
    /// the coercion has to live here rather than in each caller, or the browser
    /// and the server end up coercing differently.
    #[test]
    fn numeric_comparisons_coerce_a_posted_string_and_never_error() {
        let v = values(json!({"n": "12", "junk": "twelve"}));
        assert!(cond("n", Operator::Gt, Some(json!(10))).holds(&v));
        assert!(cond("n", Operator::Lte, Some(json!(12))).holds(&v));
        assert!(!cond("n", Operator::Gt, Some(json!(12))).holds(&v));
        // Every malformed comparison answers "does not hold", both directions.
        assert!(!cond("junk", Operator::Gt, Some(json!(1))).holds(&v));
        assert!(!cond("n", Operator::Gt, Some(json!("ten"))).holds(&v));
        assert!(!cond("missing", Operator::Lt, Some(json!(1))).holds(&v));
    }

    #[test]
    fn in_requires_an_array_and_falls_to_false_otherwise() {
        let v = values(json!({"k": "b"}));
        assert!(cond("k", Operator::In, Some(json!(["a", "b"]))).holds(&v));
        assert!(!cond("k", Operator::In, Some(json!(["a"]))).holds(&v));
        assert!(cond("k", Operator::NotIn, Some(json!(["a"]))).holds(&v));
        // Not an array: no error, no hold.
        assert!(!cond("k", Operator::In, Some(json!("b"))).holds(&v));
        assert!(!cond("k", Operator::NotIn, Some(json!("b"))).holds(&v));
    }

    #[test]
    fn a_condition_round_trips_through_toml() {
        let parsed: Condition = toml::from_str(
            r#"field = "kind"
op = "in"
value = ["talk", "seminar"]
"#,
        )
        .unwrap();
        assert_eq!(
            parsed,
            cond("kind", Operator::In, Some(json!(["talk", "seminar"])))
        );
    }
}
