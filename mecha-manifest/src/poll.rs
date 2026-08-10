//! The general-purpose poll: typed questions out, typed ballots back.
//!
//! Grows the scheduling poll (booking.rs) into the design of
//! mecha's `docs/POLL-DESIGN.md`: a poll is a **list** of questions —
//! choice, ranking, likert, vas, text, times — answered by ballots the box
//! validates against each question's own vocabulary, tallied by pure
//! functions both ends can run.
//!
//! Three rules carry this module:
//!
//! - **Only declared vocabulary, only legal shapes.** For every kind but
//!   `text`, a ballot is enum values and small integers the question itself
//!   declared, which is what keeps poll pages out of the quarantine. `text`
//!   is the deliberate exception: its answers are prose, capped here and
//!   treated as `free_text` everywhere downstream.
//! - **Ballots, never counters.** Tallies are derived on demand from stored
//!   ballots; every visibility mode, edit, export and future tally method
//!   falls out of that. IRV and Borda are two reads of the same ranking
//!   ballots, not two ballot formats.
//! - **Absent is absent.** An unanswered question is no answer — never a
//!   default, never a midpoint. The same rule reaches into the VAS widget:
//!   an untouched slider must not submit at all, so nothing here has a
//!   "default value" to fall back to.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::request::valid_id;
use crate::{ManifestError, Result};

/// The most a `text` answer may be allowed to hold, whatever the manifest
/// says. The forms rule: an unauthenticated endpoint plus an uncapped text
/// field is an unbounded write.
pub const MAX_TEXT_ANSWER: usize = 10_000;

/// Below this many respondents, an `anonymous` poll's emitters withhold
/// per-option breakdowns — "the one person who strongly disagreed" is not an
/// aggregate. A starting value, recorded as open in the design doc; the
/// constant exists so every emitter asks the same question of the same
/// number.
pub const DEFAULT_SUPPRESSION_FLOOR: usize = 3;

/// One poll, as `polls create --spec` reads it and the box stores it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollSpec {
    pub title: String,
    /// RFC 3339, with offset. Enforced by whoever holds a clock; validated
    /// here only for shape, so a typo fails at authoring time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    pub questions: Vec<PollQuestion>,
    #[serde(default)]
    pub results: ResultsPolicy,
    #[serde(default)]
    pub audience: Audience,
}

/// One question. `prompt` is optional because a single-question poll's title
/// already is the prompt; `required` defaults to false because a survey
/// answer skipped is an answer withheld, and forcing a respondent to invent
/// one to reach the end is the midpoint-inventing bug wearing a UI.
///
/// Note the absent `deny_unknown_fields`: serde cannot combine it with the
/// `flatten` that gives us `kind = "likert"` inline. The check is done by
/// hand against the raw TOML in [`check_question_keys`], per kind — the same
/// arrangement, for the same reason, as `Field` in request.rs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollQuestion {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// Whether the choices run down the page or across it.
    ///
    /// Presentation, deliberately kept out of [`QuestionKind`]: the kind is
    /// what a question *means* and therefore what an answer may be, and a
    /// tally must never change because someone rearranged a page. Five points
    /// in a row is the conventional Likert item and reads as a scale; five
    /// options down the page reads as a list of things to pick between. The
    /// default is whatever each kind already rendered, so no existing spec
    /// changes meaning by upgrading.
    #[serde(default, skip_serializing_if = "Layout::is_default")]
    pub layout: Layout,
    #[serde(flatten)]
    pub kind: QuestionKind,
}

/// Which way a question's controls run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// Each kind's existing rendering: scales across, everything else down.
    #[default]
    Auto,
    /// One control per line.
    Vertical,
    /// Controls on one line, wrapping when they must.
    Horizontal,
}

impl Layout {
    fn is_default(&self) -> bool {
        matches!(self, Layout::Auto)
    }

    /// Whether this question's controls should run across the page, given what
    /// the kind renders as when nobody says.
    pub fn is_horizontal(&self, default_horizontal: bool) -> bool {
        match self {
            Layout::Auto => default_horizontal,
            Layout::Horizontal => true,
            Layout::Vertical => false,
        }
    }
}

/// What a question asks, and therefore what an answer may be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuestionKind {
    /// Pick between `min_choices` and `max_choices` of the options.
    /// 1/1 is single choice; 1/N is approval voting.
    Choice {
        #[serde(default = "one")]
        min_choices: usize,
        #[serde(default = "one")]
        max_choices: usize,
        options: Vec<PollOption>,
    },
    /// Rank the options. Partial rankings are legal — the top three of six
    /// is an opinion, not an error.
    Ranking { options: Vec<PollOption> },
    /// A discrete labeled scale — ordinal data, so the tally leads with the
    /// distribution and the median. Either every point is labeled
    /// (`labels`, length exactly `points` — a proper Likert item) or only
    /// the ends are (`label_min`/`label_max`), never both.
    Likert {
        points: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        labels: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label_min: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label_max: Option<String>,
    },
    /// A visual analogue scale: continuous 0–100. The anchors are required
    /// fields because an unanchored VAS measures nothing.
    Vas {
        anchor_min: String,
        anchor_max: String,
    },
    /// A free response. The one prose-bearing kind; `max_length` is
    /// required for the same reason `FieldKind::Text` requires it.
    Text { max_length: usize },
    /// The scheduling poll's question, unchanged: candidates arrive from
    /// the freebusy pipeline (never the spec), answers are the tri-state.
    /// A `times` question stands alone in its poll — nothing forbids mixing
    /// in principle; nothing asks for it, and the page layout assumes it.
    Times {
        timezone: String,
        duration_minutes: u32,
    },
}

fn one() -> usize {
    1
}

/// One option of a `choice` or `ranking` question. The organizer's words —
/// escaped at render like every manifest string, and `link` is data to
/// show, never a thing to fetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// When a voter sees results, and whose names ride them. Both are promises
/// made before the vote, so they are fixed at creation — the store refuses
/// edits once a ballot exists, and nothing here offers a setter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultsPolicy {
    #[serde(default)]
    pub show: Show,
    /// Absent means the audience's own default: `named` for a roster,
    /// `anonymous` for a link — resolved by [`ResultsPolicy::identity`],
    /// never read raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<Identity>,
}

impl ResultsPolicy {
    /// The identity mode, resolved against the audience. A `link` audience
    /// has no names to show, so its default — and its only legal value —
    /// is `anonymous`; [`PollSpec::check`] refuses the contradiction rather
    /// than silently rewriting a promise.
    pub fn identity(&self, audience: AudienceKind) -> Identity {
        self.identity.unwrap_or(match audience {
            AudienceKind::Roster => Identity::Named,
            AudienceKind::Link => Identity::Anonymous,
        })
    }

    #[cfg(test)]
    fn with_identity(identity: Identity) -> Self {
        ResultsPolicy {
            show: Show::default(),
            identity: Some(identity),
        }
    }
}

/// When a voter sees results. `after_vote` is the default by decision:
/// independent ballots first, the summary as the reward for voting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Show {
    Live,
    #[default]
    AfterVote,
    AfterClose,
    Creator,
}

/// Whose names ride the results. `anonymous` is a serving policy — never
/// rendered, never drained, never exported — and every emitter that honours
/// it should also consult [`DEFAULT_SUPPRESSION_FLOOR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    Named,
    Creator,
    Anonymous,
}

/// Who may answer: a named roster with a capability URL each, or one shared
/// link whose dedup is a cookie and an honor system.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audience {
    #[serde(default)]
    pub kind: AudienceKind,
    /// Required for `link`, refused for `roster`. An open write endpoint is
    /// priced in advance: a bot run costs the poll its remaining capacity,
    /// never the box its disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ballots: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudienceKind {
    #[default]
    Roster,
    Link,
}

impl PollSpec {
    /// Parse and check, one call — a spec that parses but would misbehave
    /// is refused where there is someone to tell.
    pub fn from_toml(text: &str) -> Result<PollSpec> {
        let spec: PollSpec = toml::from_str(text)?;
        check_question_keys(text)?;
        spec.check()?;
        Ok(spec)
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| ManifestError::invalid(format!("serialising poll spec: {e}")))
    }

    pub fn question(&self, id: &str) -> Option<&PollQuestion> {
        self.questions.iter().find(|q| q.id == id)
    }

    /// Everything that can be wrong with the spec itself.
    pub fn check(&self) -> Result<()> {
        if self.title.trim().is_empty() {
            return Err(ManifestError::invalid("the poll has no title"));
        }
        if let Some(deadline) = &self.deadline {
            if chrono::DateTime::parse_from_rfc3339(deadline).is_err() {
                return Err(ManifestError::invalid(format!(
                    "deadline `{deadline}` is not an RFC 3339 instant \
                     (like 2026-08-13T17:00:00-04:00)"
                )));
            }
        }
        if self.questions.is_empty() {
            return Err(ManifestError::invalid("the poll declares no questions"));
        }
        let mut seen = BTreeSet::new();
        for question in &self.questions {
            valid_id(&question.id, "question id")?;
            if !seen.insert(question.id.as_str()) {
                return Err(ManifestError::invalid(format!(
                    "two questions share the id `{}`",
                    question.id
                )));
            }
            question.check()?;
        }
        if self.questions.len() > 1
            && self
                .questions
                .iter()
                .any(|q| matches!(q.kind, QuestionKind::Times { .. }))
        {
            return Err(ManifestError::invalid(
                "a `times` question stands alone in its poll",
            ));
        }
        match self.audience.kind {
            AudienceKind::Link => {
                match self.audience.max_ballots {
                    None | Some(0) => {
                        return Err(ManifestError::invalid(
                            "a link audience requires max_ballots (≥ 1): an open \
                             write endpoint is priced in advance",
                        ))
                    }
                    Some(_) => {}
                }
                if let Some(identity) = self.results.identity {
                    if identity != Identity::Anonymous {
                        return Err(ManifestError::invalid(
                            "a link audience has no names, so identity can only be \
                             `anonymous` — remove the line or change it",
                        ));
                    }
                }
                if self
                    .questions
                    .iter()
                    .any(|q| matches!(q.kind, QuestionKind::Times { .. }))
                {
                    return Err(ManifestError::invalid(
                        "a `times` poll is seeded for a known roster; a link \
                         audience cannot answer one",
                    ));
                }
            }
            AudienceKind::Roster => {
                if self.audience.max_ballots.is_some() {
                    return Err(ManifestError::invalid(
                        "max_ballots is for link audiences; a roster is already \
                         its own cap",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl PollQuestion {
    fn check(&self) -> Result<()> {
        let id = &self.id;
        match &self.kind {
            QuestionKind::Choice {
                min_choices,
                max_choices,
                options,
            } => {
                check_options(id, options)?;
                if *min_choices < 1 || min_choices > max_choices || *max_choices > options.len() {
                    return Err(ManifestError::invalid(format!(
                        "question `{id}`: choices must satisfy \
                         1 ≤ min_choices ({min_choices}) ≤ max_choices \
                         ({max_choices}) ≤ options ({})",
                        options.len()
                    )));
                }
            }
            QuestionKind::Ranking { options } => check_options(id, options)?,
            QuestionKind::Likert {
                points,
                labels,
                label_min,
                label_max,
            } => {
                if !(2..=11).contains(points) {
                    return Err(ManifestError::invalid(format!(
                        "question `{id}`: a likert scale has 2–11 points, not {points}"
                    )));
                }
                if let Some(labels) = labels {
                    if labels.len() != usize::from(*points) {
                        return Err(ManifestError::invalid(format!(
                            "question `{id}`: {} labels for {points} points — a \
                             proper likert item labels every point, exactly",
                            labels.len()
                        )));
                    }
                    if labels.iter().any(|l| l.trim().is_empty()) {
                        return Err(ManifestError::invalid(format!(
                            "question `{id}`: an empty likert label"
                        )));
                    }
                    if label_min.is_some() || label_max.is_some() {
                        return Err(ManifestError::invalid(format!(
                            "question `{id}`: full labels and endpoint labels are \
                             two spellings of one thing — use one"
                        )));
                    }
                }
            }
            QuestionKind::Vas {
                anchor_min,
                anchor_max,
            } => {
                if anchor_min.trim().is_empty() || anchor_max.trim().is_empty() {
                    return Err(ManifestError::invalid(format!(
                        "question `{id}`: an unanchored VAS measures nothing — \
                         both anchors need words"
                    )));
                }
            }
            QuestionKind::Text { max_length } => {
                if *max_length == 0 || *max_length > MAX_TEXT_ANSWER {
                    return Err(ManifestError::invalid(format!(
                        "question `{id}`: max_length must be 1–{MAX_TEXT_ANSWER}"
                    )));
                }
            }
            QuestionKind::Times {
                timezone,
                duration_minutes,
            } => {
                if timezone.parse::<chrono_tz::Tz>().is_err() {
                    return Err(ManifestError::invalid(format!(
                        "question `{id}`: `{timezone}` is not an IANA timezone"
                    )));
                }
                if *duration_minutes == 0 {
                    return Err(ManifestError::invalid(format!(
                        "question `{id}`: a zero-minute meeting"
                    )));
                }
            }
        }
        Ok(())
    }

    /// The declared options, for the kinds that have them.
    pub fn options(&self) -> &[PollOption] {
        match &self.kind {
            QuestionKind::Choice { options, .. } | QuestionKind::Ranking { options } => options,
            _ => &[],
        }
    }
}

fn check_options(question: &str, options: &[PollOption]) -> Result<()> {
    if options.len() < 2 {
        return Err(ManifestError::invalid(format!(
            "question `{question}`: fewer than two options is not a question"
        )));
    }
    let mut seen = BTreeSet::new();
    for option in options {
        valid_id(&option.id, "option id")?;
        if !seen.insert(option.id.as_str()) {
            return Err(ManifestError::invalid(format!(
                "question `{question}`: two options share the id `{}`",
                option.id
            )));
        }
        if option.label.trim().is_empty() {
            return Err(ManifestError::invalid(format!(
                "question `{question}`: option `{}` has no label",
                option.id
            )));
        }
        if let Some(link) = &option.link {
            if !link.starts_with("https://") && !link.starts_with("http://") {
                return Err(ManifestError::invalid(format!(
                    "question `{question}`: option `{}` links to `{link}`, \
                     which is not http(s)",
                    option.id
                )));
            }
        }
    }
    Ok(())
}

/// The keys each question kind accepts, checked by hand against the raw
/// TOML because serde's `flatten` forfeits `deny_unknown_fields` — a typo'd
/// key in a schema format silently doing nothing is not acceptable. The
/// same arrangement as `check_field_keys` in request.rs.
const COMMON_QUESTION_KEYS: &[&str] = &["id", "prompt", "required", "kind", "layout"];

fn allowed_question_keys(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "choice" => &["min_choices", "max_choices", "options"],
        "ranking" => &["options"],
        "likert" => &["points", "labels", "label_min", "label_max"],
        "vas" => &["anchor_min", "anchor_max"],
        "text" => &["max_length"],
        "times" => &["timezone", "duration_minutes"],
        _ => return None,
    })
}

fn check_question_keys(text: &str) -> Result<()> {
    let document: toml::Value = toml::from_str(text)?;
    let Some(questions) = document.get("questions").and_then(|q| q.as_array()) else {
        return Ok(());
    };
    for question in questions {
        let Some(table) = question.as_table() else {
            continue;
        };
        let id = table
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        let kind = table.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let Some(specific) = allowed_question_keys(kind) else {
            // An unknown kind is serde's error to give, and it gives a
            // better one than this could.
            continue;
        };
        for key in table.keys() {
            if COMMON_QUESTION_KEYS.contains(&key.as_str()) || specific.contains(&key.as_str()) {
                continue;
            }
            return Err(ManifestError::invalid(format!(
                "question `{id}` has the key `{key}`, which a `{kind}` question \
                 does not take (it accepts: {})",
                specific.join(", ")
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Ballots
// ---------------------------------------------------------------------------

/// One answer to one question. Serialised tagged, because two of the
/// variants are both lists of ids and an untagged reload would have to
/// guess which — and a ballot is data that outlives the process that wrote
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Answer {
    /// Selected option ids, submission order, deduplicated.
    Choice(Vec<String>),
    /// Ranked option ids, best first, distinct; possibly partial.
    Ranking(Vec<String>),
    /// 1..=points.
    Likert(u8),
    /// 0..=100.
    Vas(u8),
    /// Prose. Capped, and `free_text` everywhere downstream.
    Text(String),
}

/// One participant's answers, keyed by question id. Absent is absent.
pub type Ballot = BTreeMap<String, Answer>;

impl PollQuestion {
    /// Validate one raw answer against this question's vocabulary.
    ///
    /// `Ok(None)` is a deliberately-empty answer (an empty selection, a
    /// blank text) — an answer withdrawn, not an error. `Err` is a value
    /// the vocabulary does not contain, which the caller may refuse or
    /// ignore but must never store.
    ///
    /// `times` questions return `Err` here: their candidates live on the
    /// poll row, not the spec, and the box's existing tri-state path owns
    /// them until build-order step 2 moves it. Reaching this arm is a
    /// caller bug, and saying so beats guessing.
    pub fn validate_answer(&self, raw: &Value) -> std::result::Result<Option<Answer>, String> {
        match &self.kind {
            QuestionKind::Choice {
                max_choices,
                options,
                ..
            } => {
                let ids = id_list(raw)?;
                let mut picked = Vec::new();
                for id in ids {
                    if !options.iter().any(|o| o.id == id) {
                        return Err(format!("`{id}` is not an option here"));
                    }
                    if !picked.contains(&id) {
                        picked.push(id);
                    }
                }
                if picked.is_empty() {
                    return Ok(None);
                }
                if picked.len() > *max_choices {
                    return Err(format!(
                        "{} selections where at most {max_choices} are allowed",
                        picked.len()
                    ));
                }
                Ok(Some(Answer::Choice(picked)))
            }
            QuestionKind::Ranking { options } => {
                let ids = id_list(raw)?;
                let mut ranked = Vec::new();
                for id in ids {
                    if !options.iter().any(|o| o.id == id) {
                        return Err(format!("`{id}` is not an option here"));
                    }
                    if ranked.contains(&id) {
                        return Err(format!("`{id}` is ranked twice"));
                    }
                    ranked.push(id);
                }
                if ranked.is_empty() {
                    return Ok(None);
                }
                Ok(Some(Answer::Ranking(ranked)))
            }
            QuestionKind::Likert { points, .. } => {
                let value = integer(raw)?;
                if !(1..=i64::from(*points)).contains(&value) {
                    return Err(format!("{value} is off a 1–{points} scale"));
                }
                Ok(Some(Answer::Likert(value as u8)))
            }
            QuestionKind::Vas { .. } => {
                let value = integer(raw)?;
                if !(0..=100).contains(&value) {
                    return Err(format!("{value} is off the 0–100 line"));
                }
                Ok(Some(Answer::Vas(value as u8)))
            }
            QuestionKind::Text { max_length } => {
                let Some(text) = raw.as_str() else {
                    return Err("a text answer must be a string".into());
                };
                if text.trim().is_empty() {
                    return Ok(None);
                }
                let length = text.chars().count();
                if length > *max_length {
                    return Err(format!(
                        "{length} characters where at most {max_length} fit"
                    ));
                }
                Ok(Some(Answer::Text(text.to_string())))
            }
            QuestionKind::Times { .. } => {
                Err("times answers go through the poll row's candidate path".into())
            }
        }
    }

    /// Whether a stored answer satisfies the question enough to count the
    /// question as answered — the only place `min_choices` bites. Storage
    /// accepts a below-minimum selection (autosave arrives mid-thought);
    /// completion does not.
    pub fn answered(&self, answer: &Answer) -> bool {
        match (&self.kind, answer) {
            (QuestionKind::Choice { min_choices, .. }, Answer::Choice(picked)) => {
                picked.len() >= *min_choices
            }
            (QuestionKind::Ranking { .. }, Answer::Ranking(ranked)) => !ranked.is_empty(),
            (QuestionKind::Likert { .. }, Answer::Likert(_)) => true,
            (QuestionKind::Vas { .. }, Answer::Vas(_)) => true,
            (QuestionKind::Text { .. }, Answer::Text(text)) => !text.trim().is_empty(),
            _ => false,
        }
    }
}

fn id_list(raw: &Value) -> std::result::Result<Vec<String>, String> {
    match raw {
        Value::Array(items) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "option ids are strings".to_string())
            })
            .collect(),
        Value::String(one) => Ok(vec![one.clone()]),
        _ => Err("expected an option id or a list of them".into()),
    }
}

fn integer(raw: &Value) -> std::result::Result<i64, String> {
    match raw {
        Value::Number(n) => n.as_i64().ok_or_else(|| "not a whole number".into()),
        // Form posts arrive stringly; "3" is 3.
        Value::String(s) => s
            .trim()
            .parse()
            .map_err(|_| format!("`{s}` is not a number")),
        _ => Err("expected a number".into()),
    }
}

/// Validate a raw ballot wholesale: keep every legal answer, name every
/// illegal one. Unknown question ids are ignored, not stored — the same
/// posture as the tri-state POST. The caller decides whether errors refuse
/// the request or merely drop the fields; what it must never do is store
/// them.
pub fn validate_ballot(
    spec: &PollSpec,
    raw: &serde_json::Map<String, Value>,
) -> (Ballot, Vec<(String, String)>) {
    let mut ballot = Ballot::new();
    let mut errors = Vec::new();
    for question in &spec.questions {
        let Some(value) = raw.get(&question.id) else {
            continue;
        };
        match question.validate_answer(value) {
            Ok(Some(answer)) => {
                ballot.insert(question.id.clone(), answer);
            }
            Ok(None) => {}
            Err(why) => errors.push((question.id.clone(), why)),
        }
    }
    (ballot, errors)
}

// ---------------------------------------------------------------------------
// Tallies — pure reads of ballots, shared by the box's render and home's
// status. `text` has no tally, which is itself the honest answer.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChoiceTally {
    /// Ballots that answered this question.
    pub n: usize,
    /// (option id, votes), in the question's declared order.
    pub counts: Vec<(String, usize)>,
}

pub fn tally_choice(options: &[PollOption], answers: &[Answer]) -> ChoiceTally {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut n = 0;
    for answer in answers {
        let Answer::Choice(picked) = answer else {
            continue;
        };
        n += 1;
        for id in picked {
            *counts.entry(id.as_str()).or_default() += 1;
        }
    }
    ChoiceTally {
        n,
        counts: options
            .iter()
            .map(|o| {
                (
                    o.id.clone(),
                    counts.get(o.id.as_str()).copied().unwrap_or(0),
                )
            })
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LikertTally {
    pub n: usize,
    /// counts[i] is answers at point i+1.
    pub counts: Vec<usize>,
    /// The lead statistic: likert data is ordinal. Halfway values (3.5) are
    /// real — an even split has no middle answer.
    pub median: Option<f64>,
    /// The courtesy statistic, labelled as such wherever it is shown.
    pub mean: Option<f64>,
}

pub fn tally_likert(points: u8, answers: &[Answer]) -> LikertTally {
    let mut values: Vec<u8> = answers
        .iter()
        .filter_map(|a| match a {
            Answer::Likert(v) if (1..=points).contains(v) => Some(*v),
            _ => None,
        })
        .collect();
    values.sort_unstable();
    let mut counts = vec![0usize; usize::from(points)];
    for v in &values {
        counts[usize::from(*v) - 1] += 1;
    }
    LikertTally {
        n: values.len(),
        counts,
        median: median_of_sorted(&values),
        mean: mean_of(&values),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VasTally {
    pub n: usize,
    pub mean: Option<f64>,
    pub median: Option<f64>,
    /// Ten bins: 0–9, 10–19, …, 90–100 — the last takes the endpoint, so a
    /// full-scale answer lands in the top bin rather than an eleventh.
    pub deciles: [usize; 10],
}

pub fn tally_vas(answers: &[Answer]) -> VasTally {
    let mut values: Vec<u8> = answers
        .iter()
        .filter_map(|a| match a {
            Answer::Vas(v) if *v <= 100 => Some(*v),
            _ => None,
        })
        .collect();
    values.sort_unstable();
    let mut deciles = [0usize; 10];
    for v in &values {
        deciles[usize::from(*v / 10).min(9)] += 1;
    }
    VasTally {
        n: values.len(),
        mean: mean_of(&values),
        median: median_of_sorted(&values),
        deciles,
    }
}

fn mean_of(sorted: &[u8]) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    Some(sorted.iter().map(|v| f64::from(*v)).sum::<f64>() / sorted.len() as f64)
}

fn median_of_sorted(sorted: &[u8]) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        f64::from(sorted[mid])
    } else {
        f64::from(sorted[mid - 1] + sorted[mid]) / 2.0
    })
}

/// The words a set of text answers keeps saying — the input to a word
/// cloud, which is the one visualization prose supports without projecting
/// anyone's sentence.
///
/// Three rules make it safe and honest:
/// - **Counted per ballot, not per occurrence**: one answer repeating a
///   word fifty times scores 1, so nobody can shout their way to 72pt.
/// - **`min_count` is the projection guard**: at 2, a word reaches the
///   wall only when two different ballots chose it — a single troll's
///   slur never renders, without a profanity list to maintain.
/// - Stopwords and short tokens drop, because "the" at maximum size is
///   what every naive cloud shows.
///
/// Sorted by count then alphabetically, capped at 40 — deterministic, so
/// two ends render one cloud.
pub fn word_cloud(texts: &[&str], min_count: usize) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for text in texts {
        let words: BTreeSet<String> = text
            .split(|c: char| !c.is_alphanumeric() && c != '\'')
            .map(|w| w.trim_matches('\'').to_lowercase())
            .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(&w.as_str()))
            .collect();
        for word in words {
            *counts.entry(word).or_default() += 1;
        }
    }
    let mut cloud: Vec<(String, usize)> = counts
        .into_iter()
        .filter(|(_, count)| *count >= min_count.max(1))
        .collect();
    cloud.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    cloud.truncate(40);
    cloud
}

/// Function words that carry no signal at any size.
const STOPWORDS: &[&str] = &[
    "about", "after", "all", "also", "and", "any", "are", "because", "been", "before", "but",
    "can", "could", "did", "does", "for", "from", "had", "has", "have", "her", "him", "his", "how",
    "into", "its", "just", "like", "more", "most", "much", "not", "now", "one", "only", "our",
    "out", "over", "she", "should", "some", "than", "that", "the", "their", "them", "then",
    "there", "they", "this", "too", "very", "was", "were", "what", "when", "which", "who", "why",
    "will", "with", "would", "you", "your",
];

/// One IRV counting round: who held how many first preferences among the
/// options still standing, who was eliminated on it, and how many ballots
/// had nobody left to prefer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankingRound {
    /// (option id, first preferences), declared order, remaining options
    /// only.
    pub counts: Vec<(String, usize)>,
    pub eliminated: Option<String>,
    pub exhausted: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankingTally {
    pub n: usize,
    /// The live-page statistic: first preferences over the full field. The
    /// full rounds are rendered only on a closed poll, where they are true.
    pub first_preferences: Vec<(String, usize)>,
    pub rounds: Vec<RankingRound>,
    pub winner: Option<String>,
    /// Borda scores from the same ballots: the first of m options earns m,
    /// the next m−1, and so on; unranked options earn nothing. A second
    /// read, not a second ballot format.
    pub borda: Vec<(String, usize)>,
}

/// Instant-runoff over possibly-partial rankings, deterministic to the
/// last tie: the eliminated option each round is the fewest-first-
/// preferences holder, ties broken toward the lexicographically smallest
/// id — arbitrary, but the same answer every run, which is what lets two
/// ends compute one result.
pub fn tally_ranking(options: &[PollOption], answers: &[Answer]) -> RankingTally {
    let ballots: Vec<&Vec<String>> = answers
        .iter()
        .filter_map(|a| match a {
            Answer::Ranking(ranked) if !ranked.is_empty() => Some(ranked),
            _ => None,
        })
        .collect();
    let n = ballots.len();

    let count_firsts = |remaining: &BTreeSet<&str>| -> (BTreeMap<String, usize>, usize) {
        let mut counts: BTreeMap<String, usize> =
            remaining.iter().map(|id| ((*id).to_string(), 0)).collect();
        let mut exhausted = 0;
        for ballot in &ballots {
            match ballot.iter().find(|id| remaining.contains(id.as_str())) {
                Some(first) => *counts.get_mut(first.as_str()).expect("counted") += 1,
                None => exhausted += 1,
            }
        }
        (counts, exhausted)
    };

    let declared_order = |counts: &BTreeMap<String, usize>| -> Vec<(String, usize)> {
        options
            .iter()
            .filter(|o| counts.contains_key(&o.id))
            .map(|o| (o.id.clone(), counts[&o.id]))
            .collect()
    };

    let mut remaining: BTreeSet<&str> = options.iter().map(|o| o.id.as_str()).collect();
    let (initial, _) = count_firsts(&remaining);
    let first_preferences = declared_order(&initial);

    let mut rounds = Vec::new();
    let mut winner = None;
    while !remaining.is_empty() && n > 0 {
        let (counts, exhausted) = count_firsts(&remaining);
        let active = n - exhausted;
        let leader = counts
            .iter()
            .max_by_key(|(id, c)| (**c, std::cmp::Reverse(id.as_str())))
            .map(|(id, c)| (id.clone(), *c))
            .expect("remaining is non-empty");
        if leader.1 * 2 > active || remaining.len() == 1 {
            rounds.push(RankingRound {
                counts: declared_order(&counts),
                eliminated: None,
                exhausted,
            });
            // A last option standing with zero active ballots won nothing.
            winner = (leader.1 > 0).then_some(leader.0);
            break;
        }
        let lowest = counts
            .iter()
            .min_by_key(|(id, c)| (**c, id.as_str()))
            .map(|(id, _)| id.clone())
            .expect("remaining is non-empty");
        rounds.push(RankingRound {
            counts: declared_order(&counts),
            eliminated: Some(lowest.clone()),
            exhausted,
        });
        remaining.remove(lowest.as_str());
    }

    let mut borda: BTreeMap<&str, usize> = options.iter().map(|o| (o.id.as_str(), 0)).collect();
    let m = options.len();
    for ballot in &ballots {
        for (position, id) in ballot.iter().enumerate() {
            if let Some(score) = borda.get_mut(id.as_str()) {
                *score += m - position;
            }
        }
    }

    RankingTally {
        n,
        first_preferences,
        rounds,
        winner,
        borda: options
            .iter()
            .map(|o| (o.id.clone(), borda[o.id.as_str()]))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(ids: &[&str]) -> Vec<PollOption> {
        ids.iter()
            .map(|id| PollOption {
                id: (*id).to_string(),
                label: id.to_uppercase(),
                detail: None,
                link: None,
            })
            .collect()
    }

    fn spec(toml: &str) -> PollSpec {
        PollSpec::from_toml(toml).expect("a valid spec")
    }

    const PAPER_VOTE: &str = r#"
        title = "Which paper?"
        [[questions]]
        id = "paper"
        kind = "choice"
        [[questions.options]]
        id = "a"
        label = "Paper A"
        [[questions.options]]
        id = "b"
        label = "Paper B"
    "#;

    #[test]
    fn defaults_are_after_vote_named_roster_optional_single_choice() {
        let poll = spec(PAPER_VOTE);
        assert_eq!(poll.results.show, Show::AfterVote);
        assert_eq!(poll.results.identity(poll.audience.kind), Identity::Named);
        assert_eq!(poll.audience.kind, AudienceKind::Roster);
        let question = &poll.questions[0];
        assert!(!question.required);
        assert!(matches!(
            question.kind,
            QuestionKind::Choice {
                min_choices: 1,
                max_choices: 1,
                ..
            }
        ));
    }

    #[test]
    fn a_link_audience_defaults_anonymous_and_refuses_named() {
        let linked = format!("{PAPER_VOTE}\n[audience]\nkind = \"link\"\nmax_ballots = 100\n");
        let poll = spec(&linked);
        assert_eq!(
            poll.results.identity(poll.audience.kind),
            Identity::Anonymous
        );

        let named = format!(
            "{PAPER_VOTE}\n[results]\nidentity = \"named\"\n\
             [audience]\nkind = \"link\"\nmax_ballots = 100\n"
        );
        let err = PollSpec::from_toml(&named).unwrap_err().to_string();
        assert!(err.contains("anonymous"), "{err}");
    }

    #[test]
    fn a_link_audience_requires_a_ballot_cap_and_a_roster_refuses_one() {
        let uncapped = format!("{PAPER_VOTE}\n[audience]\nkind = \"link\"\n");
        let err = PollSpec::from_toml(&uncapped).unwrap_err().to_string();
        assert!(err.contains("max_ballots"), "{err}");

        let capped_roster = format!("{PAPER_VOTE}\n[audience]\nmax_ballots = 5\n");
        let err = PollSpec::from_toml(&capped_roster).unwrap_err().to_string();
        assert!(err.contains("roster"), "{err}");
    }

    #[test]
    fn a_typoed_question_key_names_the_question_and_the_kind() {
        let text = r#"
            title = "T"
            [[questions]]
            id = "q"
            kind = "likert"
            points = 5
            max_length = 12
        "#;
        let err = PollSpec::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("`q`") && err.contains("max_length"), "{err}");
    }

    #[test]
    fn likert_labels_must_cover_every_point_exactly_and_not_double_up() {
        let short = r#"
            title = "T"
            [[questions]]
            id = "q"
            kind = "likert"
            points = 5
            labels = ["low", "high"]
        "#;
        assert!(PollSpec::from_toml(short).is_err());

        let both = r#"
            title = "T"
            [[questions]]
            id = "q"
            kind = "likert"
            points = 2
            labels = ["low", "high"]
            label_min = "low"
        "#;
        assert!(PollSpec::from_toml(both).is_err());
    }

    #[test]
    fn a_times_question_stands_alone_and_never_behind_a_link() {
        let mixed = r#"
            title = "T"
            [[questions]]
            id = "when"
            kind = "times"
            timezone = "America/New_York"
            duration_minutes = 60
            [[questions]]
            id = "q"
            kind = "vas"
            anchor_min = "a"
            anchor_max = "b"
        "#;
        let err = PollSpec::from_toml(mixed).unwrap_err().to_string();
        assert!(err.contains("stands alone"), "{err}");

        let linked = r#"
            title = "T"
            [[questions]]
            id = "when"
            kind = "times"
            timezone = "America/New_York"
            duration_minutes = 60
            [audience]
            kind = "link"
            max_ballots = 10
        "#;
        assert!(PollSpec::from_toml(linked).is_err());
    }

    #[test]
    fn text_requires_a_sane_cap() {
        let uncapped = r#"
            title = "T"
            [[questions]]
            id = "q"
            kind = "text"
        "#;
        // serde's own error: max_length is not optional.
        assert!(PollSpec::from_toml(uncapped).is_err());

        let zero = r#"
            title = "T"
            [[questions]]
            id = "q"
            kind = "text"
            max_length = 0
        "#;
        assert!(PollSpec::from_toml(zero).is_err());
    }

    // ---- answers ---------------------------------------------------------

    fn question(kind: QuestionKind) -> PollQuestion {
        PollQuestion {
            id: "q".into(),
            prompt: None,
            required: false,
            layout: Layout::Auto,
            kind,
        }
    }

    #[test]
    fn choice_accepts_only_declared_ids_within_the_cap_and_dedupes() {
        let q = question(QuestionKind::Choice {
            min_choices: 1,
            max_choices: 2,
            options: opts(&["a", "b", "c"]),
        });
        let ok = q
            .validate_answer(&serde_json::json!(["b", "b", "a"]))
            .unwrap();
        assert_eq!(ok, Some(Answer::Choice(vec!["b".into(), "a".into()])));
        assert!(q.validate_answer(&serde_json::json!(["z"])).is_err());
        assert!(q
            .validate_answer(&serde_json::json!(["a", "b", "c"]))
            .is_err());
        assert_eq!(q.validate_answer(&serde_json::json!([])).unwrap(), None);
    }

    #[test]
    fn below_minimum_stores_but_does_not_count_as_answered() {
        let q = question(QuestionKind::Choice {
            min_choices: 2,
            max_choices: 3,
            options: opts(&["a", "b", "c"]),
        });
        let one = q
            .validate_answer(&serde_json::json!(["a"]))
            .unwrap()
            .expect("stored");
        assert!(!q.answered(&one));
        let two = q
            .validate_answer(&serde_json::json!(["a", "b"]))
            .unwrap()
            .expect("stored");
        assert!(q.answered(&two));
    }

    #[test]
    fn ranking_refuses_repeats_and_undeclared_ids_but_allows_partial() {
        let q = question(QuestionKind::Ranking {
            options: opts(&["a", "b", "c"]),
        });
        let partial = q.validate_answer(&serde_json::json!(["c", "a"])).unwrap();
        assert_eq!(partial, Some(Answer::Ranking(vec!["c".into(), "a".into()])));
        assert!(q.validate_answer(&serde_json::json!(["a", "a"])).is_err());
        assert!(q.validate_answer(&serde_json::json!(["a", "z"])).is_err());
    }

    #[test]
    fn scales_take_stringly_form_values_and_refuse_the_off_scale() {
        let likert = question(QuestionKind::Likert {
            points: 5,
            labels: None,
            label_min: None,
            label_max: None,
        });
        assert_eq!(
            likert.validate_answer(&serde_json::json!("4")).unwrap(),
            Some(Answer::Likert(4))
        );
        assert!(likert.validate_answer(&serde_json::json!(0)).is_err());
        assert!(likert.validate_answer(&serde_json::json!(6)).is_err());

        let vas = question(QuestionKind::Vas {
            anchor_min: "a".into(),
            anchor_max: "b".into(),
        });
        assert_eq!(
            vas.validate_answer(&serde_json::json!(100)).unwrap(),
            Some(Answer::Vas(100))
        );
        assert!(vas.validate_answer(&serde_json::json!(101)).is_err());
    }

    #[test]
    fn text_caps_by_characters_and_blank_is_no_answer() {
        let q = question(QuestionKind::Text { max_length: 5 });
        assert!(q.validate_answer(&serde_json::json!("abcdef")).is_err());
        // Five multi-byte characters are five characters, not fifteen bytes.
        assert_eq!(
            q.validate_answer(&serde_json::json!("ééééé")).unwrap(),
            Some(Answer::Text("ééééé".into()))
        );
        assert_eq!(q.validate_answer(&serde_json::json!("   ")).unwrap(), None);
    }

    #[test]
    fn a_ballot_keeps_the_legal_names_the_illegal_and_ignores_the_unknown() {
        let poll = spec(PAPER_VOTE);
        let raw = serde_json::json!({
            "paper": ["a"],
            "paper2": ["a"],          // not a question: ignored
        });
        let (ballot, errors) = validate_ballot(&poll, raw.as_object().unwrap());
        assert_eq!(ballot.len(), 1);
        assert!(errors.is_empty());

        let raw = serde_json::json!({ "paper": ["z"] });
        let (ballot, errors) = validate_ballot(&poll, raw.as_object().unwrap());
        assert!(ballot.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "paper");
    }

    #[test]
    fn answers_reload_as_what_they_were() {
        for answer in [
            Answer::Choice(vec!["a".into()]),
            Answer::Ranking(vec!["a".into(), "b".into()]),
            Answer::Likert(3),
            Answer::Vas(67),
            Answer::Text("words".into()),
        ] {
            let json = serde_json::to_string(&answer).unwrap();
            assert_eq!(serde_json::from_str::<Answer>(&json).unwrap(), answer);
        }
    }

    // ---- tallies ---------------------------------------------------------

    #[test]
    fn choice_tally_counts_in_declared_order_including_zeroes() {
        let options = opts(&["a", "b", "c"]);
        let answers = vec![
            Answer::Choice(vec!["b".into()]),
            Answer::Choice(vec!["b".into(), "a".into()]),
            Answer::Likert(3), // wrong kind: ignored, not miscounted
        ];
        let tally = tally_choice(&options, &answers);
        assert_eq!(tally.n, 2);
        assert_eq!(
            tally.counts,
            vec![("a".into(), 1), ("b".into(), 2), ("c".into(), 0)]
        );
    }

    #[test]
    fn likert_median_splits_the_even_case_and_the_mean_rides_along() {
        let answers: Vec<Answer> = [2u8, 4, 4, 5].map(Answer::Likert).into();
        let tally = tally_likert(5, &answers);
        assert_eq!(tally.n, 4);
        assert_eq!(tally.counts, vec![0, 1, 0, 2, 1]);
        assert_eq!(tally.median, Some(4.0));
        assert_eq!(tally.mean, Some(3.75));

        let even: Vec<Answer> = [2u8, 5].map(Answer::Likert).into();
        assert_eq!(tally_likert(5, &even).median, Some(3.5));
        assert_eq!(tally_likert(5, &[]).median, None);
    }

    #[test]
    fn vas_deciles_put_the_endpoint_in_the_top_bin() {
        let answers: Vec<Answer> = [0u8, 9, 10, 55, 99, 100].map(Answer::Vas).into();
        let tally = tally_vas(&answers);
        assert_eq!(tally.n, 6);
        assert_eq!(tally.deciles, [2, 1, 0, 0, 0, 1, 0, 0, 0, 2]);
        assert_eq!(tally.median, Some(32.5));
    }

    fn ranked(ballots: &[&[&str]]) -> Vec<Answer> {
        ballots
            .iter()
            .map(|b| Answer::Ranking(b.iter().map(|s| (*s).to_string()).collect()))
            .collect()
    }

    #[test]
    fn irv_declares_a_first_round_majority_without_eliminating_anyone() {
        let options = opts(&["a", "b", "c"]);
        let tally = tally_ranking(
            &options,
            &ranked(&[&["a", "b"], &["a"], &["a", "c"], &["b"], &["c"]]),
        );
        assert_eq!(tally.winner.as_deref(), Some("a"));
        assert_eq!(tally.rounds.len(), 1);
        assert_eq!(tally.rounds[0].eliminated, None);
    }

    #[test]
    fn irv_transfers_the_eliminated_options_ballots() {
        // b and c split 2/2; a holds 3 of 7 — no majority. Eliminating the
        // lexicographically-smallest tied loser (b) sends both its ballots
        // to c, which then beats a 4–3.
        let options = opts(&["a", "b", "c"]);
        let tally = tally_ranking(
            &options,
            &ranked(&[
                &["a"],
                &["a"],
                &["a"],
                &["b", "c"],
                &["b", "c"],
                &["c"],
                &["c"],
            ]),
        );
        assert_eq!(tally.rounds[0].eliminated.as_deref(), Some("b"));
        assert_eq!(tally.winner.as_deref(), Some("c"));
        assert_eq!(
            tally.first_preferences,
            vec![("a".into(), 3), ("b".into(), 2), ("c".into(), 2)]
        );
    }

    #[test]
    fn irv_counts_exhausted_ballots_out_of_the_majority_base() {
        // Round 1: b=2, a=1, c=1 — no majority; a and c tie for lowest and
        // the lexicographic rule eliminates a. Round 2: b=2, c=2 — still no
        // majority; b goes. Round 3: both b-only ballots are exhausted, so c
        // wins with 2 of the 2 *active* ballots — exactly half of all four,
        // which would be no majority at all if exhausted ballots stayed in
        // the base.
        let options = opts(&["a", "b", "c"]);
        let tally = tally_ranking(&options, &ranked(&[&["b"], &["b"], &["a", "c"], &["c"]]));
        assert_eq!(tally.winner.as_deref(), Some("c"));
        assert_eq!(tally.rounds.len(), 3);
        let last = tally.rounds.last().unwrap();
        assert_eq!(last.exhausted, 2);
        assert_eq!(last.counts, vec![("c".to_string(), 2)]);
    }

    #[test]
    fn borda_reads_the_same_ballots_with_unranked_earning_nothing() {
        let options = opts(&["a", "b", "c"]);
        let tally = tally_ranking(&options, &ranked(&[&["a", "b"], &["b"]]));
        // a: 3 (first of three). b: 2 (second) + 3 (first) = 5. c: 0.
        assert_eq!(
            tally.borda,
            vec![("a".into(), 3), ("b".into(), 5), ("c".into(), 0)]
        );
    }

    #[test]
    fn irv_with_no_ballots_names_no_winner() {
        let tally = tally_ranking(&opts(&["a", "b"]), &[]);
        assert_eq!(tally.winner, None);
        assert_eq!(tally.n, 0);
        assert!(tally.rounds.is_empty());
    }

    #[test]
    fn the_cloud_counts_ballots_not_repetitions_and_guards_the_wall() {
        let texts = [
            "More coffee! coffee coffee COFFEE",
            "the coffee machine is broken",
            "slides posted earlier, please",
        ];
        let cloud = word_cloud(&texts, 2);
        // Repetition inside one answer counts once; two ballots make two.
        assert_eq!(cloud, vec![("coffee".to_string(), 2)]);
        // At min_count 1 the singletons appear, stopwords and shorts never.
        let all = word_cloud(&texts, 1);
        assert!(all.iter().any(|(w, _)| w == "slides"));
        assert!(all.iter().all(|(w, _)| w != "the" && w != "is"));
    }

    #[test]
    fn identity_is_a_promise_with_an_explicit_spelling() {
        // The resolver, not the raw field, is the API: a roster defaults
        // named, and an explicit creator survives resolution.
        let policy = ResultsPolicy::with_identity(Identity::Creator);
        assert_eq!(policy.identity(AudienceKind::Roster), Identity::Creator);
        assert_eq!(
            ResultsPolicy::default().identity(AudienceKind::Roster),
            Identity::Named
        );
    }
}
