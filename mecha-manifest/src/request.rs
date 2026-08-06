//! A request type: the typed way in.
//!
//! The field model is lifted from a form system that has been lived with across
//! thirteen real forms — honours thesis, dissertation, annual review, award
//! nomination, fellowship application — because a shape someone has maintained
//! beats one invented in a design document. Three things came from it directly:
//! the **step** model, the **acknowledgment** (a required checkbox with a label,
//! a description and a link to what is being consented to, which is exactly the
//! consent-as-a-field design), and **drafts** — save and resume a partial
//! submission, which is what lets a reply carry a link back into the same typed
//! flow instead of asking a question in prose.
//!
//! **One deviation, recorded because it looks like a simplification and is
//! actually a correctness fix.** That system keeps a step's `fields`,
//! `requiredFields`, `hiddenFields` and `conditionalFields` as four parallel
//! lists. Here, fields are declared once in an ordered list on the request type
//! and each carries its own `required` and `show_when`; a step references them
//! by name. Parallel lists can disagree — a name in `requiredFields` and in
//! `hiddenFields` is a form that cannot be submitted — and there is nowhere for
//! that contradiction to live if a field owns both facts. The capability set is
//! unchanged.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

use crate::{Condition, ManifestError, Result};

/// One kind of ask, and everything the boundary needs to know about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestType {
    /// Stable id. It is a URL path segment, a filename and a tool-name
    /// component, so it is constrained to what is unambiguous in all three.
    pub id: String,
    /// Bumped deliberately. The manifest is a versioned schema format, and
    /// being forced to think about the bump is the point rather than friction.
    pub version: u32,
    /// Shown as the form's heading.
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// How long a submitted record may be retained, in days.
    ///
    /// Compliance work is deferred, deliberately — but the field is here from
    /// the start so turning it on later is a policy change rather than a
    /// migration. `None` means "no policy yet", which is honest; a default of
    /// forever dressed up as a number would not be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_days: Option<u32>,

    /// Ordered. The order is the form's order and the schema's property order,
    /// so it is part of the contract rather than an accident of a map.
    pub fields: Vec<Field>,

    /// Multi-step forms. Empty means one page — which is most of them, and a
    /// single implicit step is better than making every simple form declare a
    /// step it does not need.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,

    /// Required checkboxes shown before submission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acknowledgments: Vec<Acknowledgment>,

    /// How a submission proves an address before it costs anybody anything.
    ///
    /// **Absent means the type cannot be served as a form.** Verification is
    /// what stands between an unauthenticated endpoint and a queue that spends
    /// tokens per stranger, so a type with no verification is one the origin
    /// refuses to serve rather than one it serves unverified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,

    /// What the submitter is told once they have verified.
    ///
    /// Templated in the manifest, never composed live — see §14.4. Anything a
    /// stranger sees synchronously has to be computable by the origin alone, or
    /// the artifact stops working the moment its agent is away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<Confirmation>,
}

/// Proving that the address on a submission belongs to whoever sent it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    /// The field holding the address to verify.
    ///
    /// **Named, never guessed.** A form may hold two email fields — yours and
    /// your advisor's — and picking the first one would send a stranger's
    /// verification link to somebody who never asked for it. That is not a
    /// wrong default, it is unsolicited mail sent in the user's name.
    pub field: String,
    /// How long the link lives. Short on purpose: an unverified row is deleted
    /// rather than kept, so this is also how long an abandoned submission sits
    /// on the box.
    #[serde(default = "default_verification_hours")]
    pub expires_hours: u32,
}

fn default_verification_hours() -> u32 {
    48
}

/// The page a verified submitter lands on.
///
/// `body` may interpolate the submission's own values as `{field_name}`, which
/// is the submitter's own text handed back to them. Every placeholder must name
/// a real field — checked when the manifest loads, so a typo is a startup error
/// rather than a `{advisor_nmae}` on a stranger's screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirmation {
    pub heading: String,
    pub body: String,
    /// What to say about when they will hear back.
    ///
    /// Optional because the honest answer is sometimes nothing: an
    /// acknowledgment promising a reply within two days is a lie if nobody is
    /// attached for a week (§14.4), and no promise beats a broken one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_reply_within: Option<String>,
}

impl Confirmation {
    /// The body with `{field}` placeholders filled from a validated
    /// submission.
    ///
    /// Values are the submitter's own words, so they are returned to them and
    /// nowhere else — but they are still stranger-supplied text on a page we
    /// serve, so the caller escapes the result. Newlines are collapsed here
    /// because a value that carries them can otherwise reshape whatever it is
    /// interpolated into.
    pub fn render(&self, values: &Map<String, Value>) -> String {
        let mut out = self.body.clone();
        for (name, value) in values {
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let flattened = text.replace(['\n', '\r'], " ");
            out = out.replace(&format!("{{{name}}}"), &flattened);
        }
        out
    }
}

/// One page of a multi-step form.
///
/// Multi-step is **server-side** — one page per step, a POST between them — so
/// it works with JavaScript off and survives a closed tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Field names, in order, all of which must exist on the request type.
    pub fields: Vec<String>,
    /// Skip this step entirely when the condition does not hold. A field on a
    /// hidden step is not required, however it is declared — see
    /// [`RequestType::visible_fields`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_when: Option<Condition>,
}

/// A required checkbox with something to read first.
///
/// Consent as a field a human ticks, with a link to what they are consenting
/// to. Lifted whole, because it is already the answer to the consent question
/// the compliance work will ask.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acknowledgment {
    pub id: String,
    /// The checkbox's own label. Short.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What the label refers to, as a link the reader can open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_link: Option<String>,
}

/// One field.
///
/// Note the absent `deny_unknown_fields`: serde cannot combine it with the
/// `flatten` that gives us `kind = "email"` inline instead of a nested table.
/// A typo'd key in a *schema format* silently doing nothing is not acceptable,
/// so the check is done by hand against the raw TOML in [`check_field_keys`] —
/// per kind, so a `min` on a text field is caught too, not just a misspelling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    /// The name in the POST body, in the JSON Schema, and in the drained
    /// record. One name everywhere.
    pub name: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(flatten)]
    pub kind: FieldKind,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// Show this field only when the condition holds. A hidden field is never
    /// required and is never validated — the browser did not show it, so the
    /// server cannot insist on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_when: Option<Condition>,
}

/// What a field holds, and therefore how it validates, how it renders, and
/// whether a stranger's prose can be in it.
///
/// The HTML5 constraint attributes come straight off these variants — the
/// browser validates natively, announces errors to a screen reader natively,
/// and needs no JavaScript at all. For most of every form that is the entire
/// client-side story.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldKind {
    /// One line. `max_length` is **required**: these forms sit on an
    /// unauthenticated endpoint, and an uncapped text field is an unbounded
    /// write.
    Text {
        max_length: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    /// Many lines. Same cap rule, for the same reason.
    LongText {
        max_length: usize,
    },
    Email {
        #[serde(default = "default_email_cap")]
        max_length: usize,
    },
    /// A URL a stranger supplied.
    ///
    /// Data to show, never a thing to fetch. Nothing in this crate resolves
    /// one, and nothing downstream should either — a form field is not a reason
    /// to make an outbound request to an address a stranger chose.
    Url {
        #[serde(default = "default_url_cap")]
        max_length: usize,
    },
    /// `YYYY-MM-DD`. Bounds are inclusive and are literal dates, not offsets:
    /// an offset would have to be resolved against a clock, and the browser's
    /// clock is not the server's.
    Date {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<String>,
    },
    Integer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
    },
    /// One of ours. This is the Action-Selector shape doing the real work:
    /// nothing a stranger types can change what kind of thing their request is.
    Select {
        options: Vec<Choice>,
    },
    /// Several of ours.
    MultiSelect {
        options: Vec<Choice>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_choices: Option<usize>,
    },
    Bool,
}

fn default_email_cap() -> usize {
    254 // The practical maximum for an address, per RFC 5321's path limit.
}

fn default_url_cap() -> usize {
    2048
}

/// One option of a `select`. The value is ours; only the label is prose, and
/// the label is ours too — a stranger picks, they do not supply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

impl Field {
    /// Whether this field can hold prose a stranger wrote.
    ///
    /// **Derived, never declared.** A `select` carries one of our own values and
    /// a `date` carries a date; a `text` field always carries whatever someone
    /// typed. Letting a manifest mark a text field trusted would be the one
    /// switch that quietly turns the quarantine off, so there is no such key —
    /// the same reasoning that gives the learning system's provenance gate no
    /// override.
    pub fn is_free_text(&self) -> bool {
        matches!(
            self.kind,
            FieldKind::Text { .. }
                | FieldKind::LongText { .. }
                | FieldKind::Email { .. }
                | FieldKind::Url { .. }
        )
    }

    /// The cap this field enforces, when it has one.
    pub fn max_length(&self) -> Option<usize> {
        match &self.kind {
            FieldKind::Text { max_length, .. }
            | FieldKind::LongText { max_length }
            | FieldKind::Email { max_length }
            | FieldKind::Url { max_length } => Some(*max_length),
            _ => None,
        }
    }
}

impl RequestType {
    /// Parse and check a manifest. Both, always — a `RequestType` that exists
    /// has been checked, so no caller can forget.
    pub fn from_toml(text: &str) -> Result<Self> {
        let parsed: RequestType = toml::from_str(text)?;
        parsed.check()?;
        // Serde caught everything except the keys inside a `[[fields]]` table,
        // which `flatten` makes it structurally unable to police. Re-read the
        // raw document for exactly those.
        check_field_keys(text)?;
        Ok(parsed)
    }

    /// Serialise back to TOML.
    ///
    /// Fallible, and not defensively: a [`Condition`]'s `value` is a JSON
    /// value, and TOML has no representation for null. A manifest parsed from
    /// TOML can never contain one — you cannot write it — but a `RequestType`
    /// built programmatically can, and a library that panics on data it accepts
    /// is a library that panics.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| ManifestError::invalid(format!("serialising `{}`: {e}", self.id)))
    }

    /// Everything that can be wrong with the manifest itself.
    ///
    /// Called on every parse, so a mistake surfaces where there is someone to
    /// tell rather than on the submission that hit it.
    pub fn check(&self) -> Result<()> {
        valid_id(&self.id, "request type id")?;
        if self.fields.is_empty() {
            return Err(ManifestError::invalid(format!(
                "request type `{}` declares no fields",
                self.id
            )));
        }

        let mut seen = BTreeSet::new();
        for field in &self.fields {
            valid_id(&field.name, "field name")?;
            if !seen.insert(field.name.as_str()) {
                return Err(ManifestError::invalid(format!(
                    "request type `{}` declares field `{}` twice",
                    self.id, field.name
                )));
            }
            // Rule 5. An unauthenticated endpoint with an uncapped text field
            // is an unbounded write, and `0` is the degenerate spelling of the
            // same mistake — it would reject every non-empty submission.
            if let Some(0) = field.max_length() {
                return Err(ManifestError::invalid(format!(
                    "field `{}` has max_length = 0, which rejects every value",
                    field.name
                )));
            }
            match &field.kind {
                FieldKind::Select { options } | FieldKind::MultiSelect { options, .. } => {
                    if options.is_empty() {
                        return Err(ManifestError::invalid(format!(
                            "field `{}` is a select with no options",
                            field.name
                        )));
                    }
                    let mut values = BTreeSet::new();
                    for option in options {
                        if !values.insert(option.value.as_str()) {
                            return Err(ManifestError::invalid(format!(
                                "field `{}` offers the value `{}` twice",
                                field.name, option.value
                            )));
                        }
                    }
                }
                FieldKind::Date { min, max } => {
                    for bound in [min, max].into_iter().flatten() {
                        if !is_iso_date(bound) {
                            return Err(ManifestError::invalid(format!(
                                "field `{}` has the date bound `{bound}`, which is not YYYY-MM-DD",
                                field.name
                            )));
                        }
                    }
                }
                _ => {}
            }
            if let Some(condition) = &field.show_when {
                self.check_condition(condition, &format!("field `{}`", field.name))?;
            }
        }

        for step in &self.steps {
            valid_id(&step.id, "step id")?;
            for name in &step.fields {
                if !seen.contains(name.as_str()) {
                    return Err(ManifestError::invalid(format!(
                        "step `{}` references field `{name}`, which does not exist",
                        step.id
                    )));
                }
            }
            if let Some(condition) = &step.show_when {
                self.check_condition(condition, &format!("step `{}`", step.id))?;
            }
        }
        // A field on no step is a field no browser ever renders. Silently
        // dropping it would mean a required field that can never be filled, so
        // once steps exist they have to cover everything.
        if !self.steps.is_empty() {
            let covered: BTreeSet<&str> = self
                .steps
                .iter()
                .flat_map(|s| s.fields.iter().map(String::as_str))
                .collect();
            for field in &self.fields {
                if !covered.contains(field.name.as_str()) {
                    return Err(ManifestError::invalid(format!(
                        "field `{}` appears on no step, so no form would render it",
                        field.name
                    )));
                }
            }
        }

        for ack in &self.acknowledgments {
            valid_id(&ack.id, "acknowledgment id")?;
            if seen.contains(ack.id.as_str()) {
                return Err(ManifestError::invalid(format!(
                    "acknowledgment `{}` collides with a field of the same name",
                    ack.id
                )));
            }
        }
        check_references(self)?;
        Ok(())
    }

    /// Whether this type can be served as a form at all.
    ///
    /// Verification is what stands between an unauthenticated endpoint and a
    /// queue that costs money per stranger, so a type without it is refused by
    /// the origin rather than served unverified. Asked here, on the type,
    /// because the server should not be deciding what makes a request type
    /// servable.
    pub fn servable(&self) -> Result<&Verification> {
        self.verification.as_ref().ok_or_else(|| {
            ManifestError::invalid(format!(
                "`{}` declares no [verification], so it cannot be served as a \
                 form: an unverified submission would cost somebody a triage \
                 run for an address nobody proved",
                self.id
            ))
        })
    }

    /// A condition may only read a field this type declares. Otherwise it is
    /// permanently false and the form has a section nobody can ever reach —
    /// which reads as a rendering bug for as long as it takes someone to
    /// discover the typo.
    fn check_condition(&self, condition: &Condition, owner: &str) -> Result<()> {
        let known = self.fields.iter().any(|f| f.name == condition.field)
            || self.acknowledgments.iter().any(|a| a.id == condition.field);
        if !known {
            return Err(ManifestError::invalid(format!(
                "{owner} has a show_when reading `{}`, which is not a field of `{}`",
                condition.field, self.id
            )));
        }
        Ok(())
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Which fields a submission actually has to satisfy.
    ///
    /// **The rule that an implementation validating fields independently gets
    /// wrong: a field is required only when it is visible.** A required field on
    /// a step whose `show_when` is false was never rendered, so demanding it
    /// would reject a submission the browser considered complete — and the
    /// server would be right by its own reading and wrong about the contract.
    ///
    /// Conditions read *submitted* values, so visibility is computed against the
    /// submission rather than against the manifest alone. One pass, in declared
    /// order: a condition may only read a field, never another condition, so
    /// there is nothing to iterate to a fixed point and no cycle to detect.
    pub fn visible_fields<'a>(&'a self, values: &serde_json::Map<String, Value>) -> Vec<&'a Field> {
        let hidden_by_step: BTreeSet<&str> = self
            .steps
            .iter()
            .filter(|step| step.show_when.as_ref().is_some_and(|c| !c.holds(values)))
            .flat_map(|step| step.fields.iter().map(String::as_str))
            .collect();
        self.fields
            .iter()
            .filter(|field| !hidden_by_step.contains(field.name.as_str()))
            .filter(|field| field.show_when.as_ref().is_none_or(|c| c.holds(values)))
            .collect()
    }

    /// The free-text fields, which is what the quarantine layer needs to know:
    /// these hold a stranger's prose and everything else does not.
    pub fn free_text_fields(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|f| f.is_free_text())
    }
}

/// The keys a `[[fields]]` table may carry, given its `kind`.
///
/// Hand-maintained, which is a cost — but the alternative is a schema format
/// where `maxlength` (or `min` on a text field) is accepted and ignored, and a
/// cap that silently does not apply is the exact shape of failure this project
/// keeps naming. Adding a variant to [`FieldKind`] means adding a row here, and
/// there is a test that fails if one is forgotten.
fn allowed_keys(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "text" => &["max_length", "pattern"],
        "long_text" => &["max_length"],
        "email" | "url" => &["max_length"],
        "date" => &["min", "max"],
        "integer" => &["min", "max"],
        "select" => &["options"],
        "multi_select" => &["options", "max_choices"],
        "bool" => &[],
        _ => return None,
    })
}

/// Every key common to a field, whatever its kind.
const COMMON_FIELD_KEYS: [&str; 6] = ["name", "label", "help", "kind", "required", "show_when"];

/// Reject a `[[fields]]` key that no variant of that kind accepts.
fn check_field_keys(text: &str) -> Result<()> {
    let document: toml::Value = toml::from_str(text)?;
    let Some(fields) = document.get("fields").and_then(|f| f.as_array()) else {
        return Ok(());
    };
    for field in fields {
        let Some(table) = field.as_table() else {
            continue;
        };
        let name = table
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        let kind = table.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let Some(specific) = allowed_keys(kind) else {
            // An unknown kind is serde's error to give, and it gives a better
            // one than this could.
            continue;
        };
        for key in table.keys() {
            if COMMON_FIELD_KEYS.contains(&key.as_str()) || specific.contains(&key.as_str()) {
                continue;
            }
            return Err(ManifestError::invalid(format!(
                "field `{name}` has the key `{key}`, which a `{kind}` field does not \
                 take (it accepts: {})",
                specific.join(", ")
            )));
        }
    }
    Ok(())
}

/// Everything wrong with a `[verification]` or `[confirmation]` block.
///
/// Both refer to fields by name, and a name that does not exist is a runtime
/// surprise on a stranger's screen — or, worse, a verification link sent
/// nowhere. Checked at load, where it is our problem instead of theirs.
fn check_references(request_type: &RequestType) -> Result<()> {
    if let Some(verification) = &request_type.verification {
        let field = request_type.field(&verification.field).ok_or_else(|| {
            ManifestError::invalid(format!(
                "[verification] names `{}`, which is not a field on `{}`",
                verification.field, request_type.id
            ))
        })?;
        if !matches!(field.kind, FieldKind::Email { .. }) {
            return Err(ManifestError::invalid(format!(
                "[verification] names `{}`, which is a {:?} rather than an email \
                 field — a link can only be sent to an address",
                verification.field,
                std::mem::discriminant(&field.kind)
            )));
        }
        if !field.required {
            return Err(ManifestError::invalid(format!(
                "[verification] names `{}`, which is optional. A submission that \
                 left it blank could never be verified, and an unverified \
                 submission is one nobody ever reads.",
                verification.field
            )));
        }
        if verification.expires_hours == 0 {
            return Err(ManifestError::invalid(
                "[verification] expires_hours = 0 would expire every link before \
                 it could be clicked",
            ));
        }
    }

    if let Some(confirmation) = &request_type.confirmation {
        // Every `{placeholder}` names a real field, or a stranger reads
        // `{advisor_nmae}` on the page that is supposed to reassure them.
        let mut rest = confirmation.body.as_str();
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                return Err(ManifestError::invalid(
                    "[confirmation] body has a `{` with no `}`",
                ));
            };
            let name = &rest[open + 1..open + close];
            if request_type.field(name).is_none() {
                return Err(ManifestError::invalid(format!(
                    "[confirmation] interpolates `{{{name}}}`, which is not a field \
                     on `{}`",
                    request_type.id
                )));
            }
            rest = &rest[open + close + 1..];
        }
    }
    Ok(())
}

/// An id is a URL path segment, a filename, a JSON key and part of a generated
/// tool name. Keep it to what is unambiguous in all four — the same rule
/// trigger and producer names follow in mecha.
///
/// Public because the server checks an id that arrived off the wire against
/// exactly this rule. It is a path segment on a public origin, and the two ends
/// disagreeing about what one may contain is how a traversal gets in.
pub fn valid_id(id: &str, what: &str) -> Result<()> {
    if id.is_empty() {
        return Err(ManifestError::invalid(format!("{what} is empty")));
    }
    if id.len() > 64 {
        return Err(ManifestError::invalid(format!(
            "{what} `{id}` is too long (64 characters max)"
        )));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(ManifestError::invalid(format!(
            "{what} `{id}` may only contain lowercase letters, digits, `-` and `_`"
        )));
    }
    Ok(())
}

/// `YYYY-MM-DD`, checked without a date library — this crate has no dependency
/// on one, and the wire format is fixed. Calendar validity (a real February
/// 30th) is left to whoever has a calendar; what matters here is that the shape
/// is one the browser's `min`/`max` attributes will also accept.
pub(crate) fn is_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let month: u32 = text[5..7].parse().unwrap_or(0);
    let day: u32 = text[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

#[cfg(test)]
mod tests {
    /// A link can only be sent to an address, and only to the *right* one. A
    /// form with two email fields is ordinary — yours and your advisor's — and
    /// guessing would mail a stranger's verification to somebody who never
    /// asked for it.
    #[test]
    fn verification_must_name_a_required_email_field() {
        let base = r#"
            id = "meeting"
            version = 1
            title = "Meeting"
            [[fields]]
            name = "requester_email"
            label = "Your email"
            kind = "email"
            required = true
            [[fields]]
            name = "advisor_email"
            label = "Your advisor"
            kind = "email"
            [[fields]]
            name = "topic"
            label = "Topic"
            kind = "text"
            max_length = 200
            required = true
        "#;
        RequestType::from_toml(&format!(
            "{base}\n[verification]\nfield = \"requester_email\""
        ))
        .unwrap();

        for (block, expected) in [
            ("field = \"nobody\"", "not a field"),
            ("field = \"topic\"", "rather than an email"),
            // Optional: a submission that left it blank could never be
            // verified, and an unverified submission is one nobody reads.
            ("field = \"advisor_email\"", "optional"),
            (
                "field = \"requester_email\"\nexpires_hours = 0",
                "expire every link",
            ),
        ] {
            let err = RequestType::from_toml(&format!("{base}\n[verification]\n{block}"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{block}: {err}");
        }
    }

    /// A typo in a confirmation body is a `{advisor_nmae}` on a stranger's
    /// screen at the moment they are being reassured.
    #[test]
    fn a_confirmation_interpolates_only_real_fields() {
        let base = r#"
            id = "meeting"
            version = 1
            title = "Meeting"
            [[fields]]
            name = "topic"
            label = "Topic"
            kind = "text"
            max_length = 200
            required = true
        "#;
        let good = RequestType::from_toml(&format!(
            "{base}\n[confirmation]\nheading = \"Thanks\"\nbody = \"About {{topic}}.\""
        ))
        .unwrap();
        let mut values = Map::new();
        values.insert("topic".into(), Value::String("a grant".into()));
        assert_eq!(good.confirmation.unwrap().render(&values), "About a grant.");

        let err = RequestType::from_toml(&format!(
            "{base}\n[confirmation]\nheading = \"Thanks\"\nbody = \"About {{topci}}.\""
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a field"), "{err}");
    }

    /// A value carrying newlines can reshape whatever it is interpolated into,
    /// so it arrives as one line.
    #[test]
    fn an_interpolated_value_cannot_add_lines() {
        let confirmation = Confirmation {
            heading: "Thanks".into(),
            body: "About {topic}.".into(),
            expect_reply_within: None,
        };
        let mut values = Map::new();
        values.insert(
            "topic".into(),
            Value::String("a grant\n\nSubject: something else".into()),
        );
        let rendered = confirmation.render(&values);
        assert!(!rendered.contains('\n'), "{rendered}");
    }

    /// A type with no verification is refused as a form rather than served
    /// unverified: it is the difference between an unauthenticated endpoint
    /// and an unauthenticated endpoint that spends money.
    #[test]
    fn a_type_without_verification_is_not_servable() {
        let toml = r#"
            id = "meeting"
            version = 1
            title = "Meeting"
            [[fields]]
            name = "topic"
            label = "Topic"
            kind = "text"
            max_length = 200
        "#;
        let parsed = RequestType::from_toml(toml).unwrap();
        let err = parsed.servable().unwrap_err().to_string();
        assert!(err.contains("cannot be served as a form"), "{err}");
    }

    use super::*;
    use serde_json::json;

    fn meeting() -> RequestType {
        RequestType::from_toml(
            r#"
id = "meeting"
version = 1
title = "Request a meeting"

[[fields]]
name = "requester_email"
label = "Your email"
kind = "email"
required = true

[[fields]]
name = "kind"
label = "What kind of meeting"
kind = "select"
required = true
options = [
  { value = "advising", label = "Advising" },
  { value = "other", label = "Something else" },
]

[[fields]]
name = "other_detail"
label = "Say more"
kind = "long_text"
max_length = 500
required = true
show_when = { field = "kind", op = "eq", value = "other" }
"#,
        )
        .unwrap()
    }

    /// TOML cannot represent null, and a `Condition`'s value is a JSON value.
    /// Parsing can never produce one; constructing can.
    #[test]
    fn a_value_toml_cannot_represent_is_an_error_rather_than_a_panic() {
        let mut t = meeting();
        t.fields[2].show_when = Some(Condition {
            field: "kind".into(),
            operator: crate::Operator::Eq,
            value: Some(serde_json::Value::Null),
        });
        assert!(t.to_toml().is_err());
    }

    #[test]
    fn a_manifest_round_trips_through_toml() {
        let original = meeting();
        let again = RequestType::from_toml(&original.to_toml().unwrap()).unwrap();
        assert_eq!(again.fields.len(), 3);
        assert_eq!(again.id, "meeting");
        assert_eq!(
            again.fields[2].show_when.as_ref().unwrap().field,
            "kind",
            "a condition survives the round trip"
        );
    }

    /// The rule an independent-field validator gets wrong.
    #[test]
    fn a_required_field_is_not_required_while_it_is_hidden() {
        let t = meeting();
        let hidden = t.visible_fields(json!({"kind": "advising"}).as_object().unwrap());
        assert_eq!(hidden.len(), 2, "other_detail is not shown for `advising`");
        assert!(!hidden.iter().any(|f| f.name == "other_detail"));

        let shown = t.visible_fields(json!({"kind": "other"}).as_object().unwrap());
        assert_eq!(shown.len(), 3);
    }

    #[test]
    fn a_step_that_is_hidden_hides_every_field_on_it() {
        let t = RequestType::from_toml(
            r#"
id = "two-step"
version = 1
title = "Two steps"

[[fields]]
name = "wants_travel"
label = "Travel needed"
kind = "bool"

[[fields]]
name = "origin_city"
label = "Flying from"
kind = "text"
max_length = 80
required = true

[[steps]]
id = "basics"
title = "Basics"
fields = ["wants_travel"]

[[steps]]
id = "travel"
title = "Travel"
fields = ["origin_city"]
show_when = { field = "wants_travel", op = "is_true" }
"#,
        )
        .unwrap();

        let no = t.visible_fields(json!({"wants_travel": false}).as_object().unwrap());
        assert_eq!(no.len(), 1);
        let yes = t.visible_fields(json!({"wants_travel": true}).as_object().unwrap());
        assert_eq!(yes.len(), 2);
    }

    #[test]
    fn free_text_is_derived_from_the_kind_and_not_declarable() {
        let t = meeting();
        let free: Vec<&str> = t.free_text_fields().map(|f| f.name.as_str()).collect();
        assert_eq!(free, ["requester_email", "other_detail"]);
        assert!(!t.field("kind").unwrap().is_free_text(), "a select is ours");
    }

    #[test]
    fn an_uncapped_text_field_will_not_parse() {
        // The cap is not optional in the type, so the manifest simply does not
        // load — which is the strongest form this rule can take.
        let err = RequestType::from_toml(
            r#"
id = "uncapped"
version = 1
title = "x"
[[fields]]
name = "note"
label = "Note"
kind = "text"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("max_length"), "unexpected: {err}");
    }

    #[test]
    fn manifest_mistakes_are_refused_at_parse_time() {
        let cases = [
            (
                "max_length = 0",
                r#"id="a"
version=1
title="t"
[[fields]]
name="n"
label="N"
kind="text"
max_length=0
"#,
                "rejects every value",
            ),
            (
                "a duplicate field",
                r#"id="a"
version=1
title="t"
[[fields]]
name="n"
label="N"
kind="bool"
[[fields]]
name="n"
label="N again"
kind="bool"
"#,
                "twice",
            ),
            (
                "a condition on an unknown field",
                r#"id="a"
version=1
title="t"
[[fields]]
name="n"
label="N"
kind="bool"
show_when={field="nope",op="is_true"}
"#,
                "not a field",
            ),
            (
                "a step naming an unknown field",
                r#"id="a"
version=1
title="t"
[[fields]]
name="n"
label="N"
kind="bool"
[[steps]]
id="s"
title="S"
fields=["nope"]
"#,
                "does not exist",
            ),
            (
                "a field on no step",
                r#"id="a"
version=1
title="t"
[[fields]]
name="n"
label="N"
kind="bool"
[[fields]]
name="m"
label="M"
kind="bool"
[[steps]]
id="s"
title="S"
fields=["n"]
"#,
                "no step",
            ),
            (
                "an empty select",
                r#"id="a"
version=1
title="t"
[[fields]]
name="n"
label="N"
kind="select"
options=[]
"#,
                "no options",
            ),
            (
                "a bad date bound",
                r#"id="a"
version=1
title="t"
[[fields]]
name="n"
label="N"
kind="date"
min="2026-13-01"
"#,
                "YYYY-MM-DD",
            ),
        ];
        for (what, toml, expected) in cases {
            let err = RequestType::from_toml(toml).unwrap_err().to_string();
            assert!(
                err.contains(expected),
                "{what}: expected {expected:?} in {err:?}"
            );
        }
    }

    #[test]
    fn iso_dates_are_checked_for_shape_and_range() {
        for good in ["2026-01-01", "2026-12-31"] {
            assert!(is_iso_date(good), "{good}");
        }
        for bad in ["2026-1-1", "26-01-01", "2026-00-01", "2026-01-32", "", "x"] {
            assert!(!is_iso_date(bad), "{bad}");
        }
    }
}
