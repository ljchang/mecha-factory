//! The component gallery: every field kind, every state, in every built-in
//! theme, rendered by the renderer that serves the real forms.
//!
//! ```sh
//! cargo run --example gallery -- /tmp/gallery
//! xdg-open /tmp/gallery/index.html
//! ```
//!
//! **Generated, never drawn.** A gallery of hand-written HTML is a lie the
//! first time `form.rs` changes, and a lie about what a form looks like is the
//! most convincing kind — nobody diffs a screenshot. So every page here comes
//! out of [`RequestType::form`] and [`RequestType::upload_form`], from manifests
//! written in the same TOML a tenant writes, and the same bytes are what the
//! documentation site embeds.
//!
//! Two things keep it honest as the crate moves:
//!
//! - [`kind_tag`] matches `FieldKind` **exhaustively**, so a new variant stops
//!   this example compiling. The fix is three lines away from the arm you just
//!   added, and [`every_kind_is_shown`] fails until a real field of that kind
//!   exists in `KINDS`. A gallery missing a kind is worse than no gallery,
//!   because it reads as a complete list.
//! - Themes come from [`BUILT_IN_THEMES`], so a third palette appears
//!   everywhere for free. That is the claim `theme.rs` makes — tokens, never
//!   rules — and this is the evidence for it: if switching a theme leaks a
//!   colour the structural sheet should not own, it is visible here.
//!
//! The error states are **produced by the validator**, not typed out. A gallery
//! showing invented error text would document a form nobody is ever served.

use chrono::{DateTime, Duration, Utc};
use mecha_manifest::availability::{availability, Interval, Slot};
use mecha_manifest::{
    build_results, screen_page, survey_page, Answer, Ballot, Identity, PageMode, PollSpec,
    QuestionKind, QuestionResults, ScreenPageOptions, Show, SurveyPageOptions,
};
use mecha_manifest::{escape_text, FieldKind, FormOptions, Phase, RequestType, Theme};
use mecha_manifest::{poll_page, BookingOptions, PollAnswer, PollCandidate, PollPageOptions};
use mecha_manifest::{RequestKind, BUILT_IN_THEMES, MAX_FILE_BYTES_PER_TYPE};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The gallery's clock.
///
/// **Hardcoded, and it has to be.** `gallery/` is a committed golden file
/// diffed in CI, so a page rendered against the real clock would differ from
/// the committed one every single day and the drift check would cry wolf until
/// somebody deleted it. Booking is the first surface here that has a time at
/// all, and it is exactly the surface where that trap is easiest to fall into.
///
/// A Monday, 09:00 in `book.toml`'s `America/New_York`, chosen so the
/// starter's 24-hour minimum notice lands inside the same week and the first
/// rendered week has slots in it rather than being an honest but useless
/// empty grid.
const NOW: &str = "2026-03-02T14:00:00Z";

fn now() -> DateTime<Utc> {
    NOW.parse().expect("the gallery's clock is a literal")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out: PathBuf = std::env::args()
        .nth(1)
        .ok_or("usage: gallery <output-dir>")?
        .into();

    let entries = entries()?;
    every_kind_is_shown(&entries);
    let survey_open = survey_spec(SURVEY_OPEN_RESULTS)?;
    let survey_closed = survey_spec(SURVEY_CLOSED_RESULTS)?;
    every_question_kind_is_shown(&survey_open);

    fs::create_dir_all(out.join("source"))?;
    fs::create_dir_all(out.join("schema"))?;
    for entry in &entries {
        // The manifest text verbatim, comments and all — the documentation
        // shows "what you write" beside "what it renders", and a re-serialised
        // copy would drop exactly the comments that explain the choice.
        fs::write(
            out.join("source").join(format!("{}.toml", entry.id)),
            &entry.toml,
        )?;
        fs::write(
            out.join("schema").join(format!("{}.json", entry.id)),
            serde_json::to_string_pretty(&entry.request.json_schema())? + "\n",
        )?;
    }

    let mut pages = 0usize;
    for theme in BUILT_IN_THEMES {
        let dir = out.join(theme.name);
        fs::create_dir_all(&dir)?;
        let mut assets_written = false;
        let mut booking_assets_written = false;
        for entry in &entries {
            for variant in entry.variants() {
                if variant.is_booking() {
                    let page = render_booking(entry, &variant, theme)?;
                    fs::write(dir.join(variant.file_name(entry)), &page.html)?;
                    pages += 1;
                    if !booking_assets_written {
                        for (name, contents) in page.assets() {
                            fs::write(dir.join(name), contents)?;
                        }
                        booking_assets_written = true;
                    }
                    continue;
                }
                let options = FormOptions {
                    // Inert on purpose. The gallery is static, so nothing here
                    // posts anywhere; what a submit click does demonstrate is
                    // the HTML5 constraint layer, which refuses to navigate
                    // while a required field is empty.
                    action: "#".into(),
                    // Relative to the page's own directory, which is where the
                    // theme's stylesheet is written.
                    assets: String::new(),
                    theme,
                    ..variant.options(entry)
                };
                let page = match variant {
                    Variant::Upload => entry.request.upload_form(&options),
                    _ => entry.request.form(&options),
                };
                fs::write(dir.join(variant.file_name(entry)), &page.html)?;
                pages += 1;
                if !assets_written {
                    for (name, contents) in page.assets() {
                        fs::write(dir.join(name), contents)?;
                    }
                    assets_written = true;
                }
            }
        }
        for (name, html) in survey_pages(&survey_open, &survey_closed, theme) {
            fs::write(dir.join(name), html)?;
            pages += 1;
        }
    }

    // The contents, as data, for whatever frames these pages. The
    // documentation site builds its theme picker from this rather than from a
    // list of its own: a hardcoded list is how a third palette ships and
    // appears nowhere, which is the property `BUILT_IN` exists to give.
    fs::write(out.join("index.json"), contents_json(&entries)? + "\n")?;
    fs::write(out.join("index.html"), landing(&entries))?;
    fs::write(
        out.join("gallery.css"),
        format!("{}{LANDING_CSS}", Theme::default().css()),
    )?;

    println!("gallery → {}", out.display());
    println!(
        "  {} types × {} themes = {pages} pages, {} field kinds covered",
        entries.len(),
        BUILT_IN_THEMES.len(),
        ALL_KINDS.len()
    );
    println!("  open {}", out.join("index.html").display());
    Ok(())
}

/// One request type in the gallery, and the text it was written as.
struct Entry {
    id: String,
    /// What this type is here to show, in one line, for the landing page.
    blurb: &'static str,
    toml: String,
    request: RequestType,
    /// A deliberately bad submission, when this type demonstrates error states.
    /// Validated rather than asserted: the messages on the page are the ones a
    /// real submitter would read.
    bad_submission: Option<Map<String, Value>>,
}

/// A rendering of one type. Every variant is a state the served forms actually
/// reach — none of it is a mode invented for the gallery.
enum Variant {
    /// The public submission form.
    Plain,
    /// The same form, re-served with a rejected submission on it.
    Errors,
    /// The post-verification upload page: the one place a file input exists.
    Upload,
    /// One page of a multi-step form, which is how a step is really served.
    Step(String),
    /// The week view of a `kind = "booking"` type. `weeks` pages forward from
    /// the first week holding a slot, which is what a "next week" link does.
    Booking { weeks: i64 },
    /// A booking page re-served with the details form rejected — the state
    /// the box's POST path really produces (`http/booking.rs::page_back`):
    /// the summary names fields by label, the typed values ride back, and
    /// the picked `_slot` arrives re-checked. Rendering this page in the
    /// gallery is what surfaced both of those requirements before the
    /// server path existed, which is the gallery doing its job.
    BookingErrors,
    /// The poll page: the same weekly frame over seeded candidates, each a
    /// tri-state answer.
    Poll,
}

impl Entry {
    fn variants(&self) -> Vec<Variant> {
        // A booking type is never served as a plain form — the details fields
        // exist only beside the week view — so the gallery does not render one
        // either. Every variant here is a state the box actually reaches.
        if self.request.kind == RequestKind::Booking {
            return vec![
                Variant::Booking { weeks: 0 },
                Variant::Booking { weeks: 1 },
                Variant::BookingErrors,
                Variant::Poll,
            ];
        }

        let mut out = vec![Variant::Plain];
        if self.bad_submission.is_some() {
            out.push(Variant::Errors);
        }
        if self.request.has_file_fields() {
            out.push(Variant::Upload);
        }
        // Derived from the manifest rather than listed, so a starter that grows
        // a step grows a page.
        out.extend(
            self.request
                .steps
                .iter()
                .map(|step| Variant::Step(step.id.clone())),
        );
        out
    }
}

impl Variant {
    fn file_name(&self, entry: &Entry) -> String {
        match self {
            Variant::Plain => format!("{}.html", entry.id),
            Variant::Errors => format!("{}.errors.html", entry.id),
            Variant::Upload => format!("{}.upload.html", entry.id),
            Variant::Step(id) => format!("{}.step-{id}.html", entry.id),
            Variant::Booking { weeks: 0 } => format!("{}.html", entry.id),
            Variant::Booking { weeks } => format!("{}.week-{weeks}.html", entry.id),
            Variant::BookingErrors => format!("{}.errors.html", entry.id),
            Variant::Poll => format!("{}.poll.html", entry.id),
        }
    }

    fn label(&self) -> String {
        match self {
            Variant::Plain => "the form".into(),
            Variant::Errors | Variant::BookingErrors => "rejected, with errors".into(),
            Variant::Upload => "upload page".into(),
            Variant::Step(id) => format!("step: {id}"),
            Variant::Booking { weeks: 0 } => "the week".into(),
            Variant::Booking { weeks } => format!("{weeks} week(s) on"),
            Variant::Poll => "poll: one participant".into(),
        }
    }

    /// Whether this variant renders through the booking machinery, which has
    /// its own options type and its own three assets.
    fn is_booking(&self) -> bool {
        matches!(
            self,
            Variant::Booking { .. } | Variant::BookingErrors | Variant::Poll
        )
    }

    /// The parts of [`FormOptions`] this variant owns. The caller fills in the
    /// theme and the asset prefix, which are the same for every page.
    fn options(&self, entry: &Entry) -> FormOptions {
        match self {
            // Booking variants carry their own options type; the caller
            // branches before it gets here.
            Variant::Plain
            | Variant::Upload
            | Variant::Booking { .. }
            | Variant::BookingErrors
            | Variant::Poll => FormOptions::default(),
            Variant::Step(id) => FormOptions {
                step: Some(id.clone()),
                ..FormOptions::default()
            },
            Variant::Errors => {
                let raw = entry
                    .bad_submission
                    .clone()
                    .expect("an errors variant exists only where a bad submission does");
                // `Submit`, not `Complete`: this is the public form POST, where
                // a file value is refused outright rather than required.
                let errors = entry
                    .request
                    .validate_at(&raw, Phase::Submit)
                    .err()
                    .unwrap_or_else(|| {
                        panic!(
                            "`{}`'s bad submission validates cleanly, so the errors page \
                             would render no errors — fix the sample, not the assertion",
                            entry.id
                        )
                    });
                FormOptions {
                    // The submitter's own answers, handed back to them. Which
                    // is also the one interpolation in this crate that carries
                    // stranger-supplied text, so the gallery exercises it.
                    values: raw,
                    errors,
                    ..FormOptions::default()
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Booking
// ---------------------------------------------------------------------------

/// The slots the gallery's booking pages offer.
///
/// Run through the **real engine** against the starter's own `[availability]`,
/// rather than made up: a hand-written week would show a layout nobody's
/// policy produces, and the interesting parts of this page — a day missing
/// because its cap is met, two durations sharing a start, a gap where
/// something is already booked — are what the engine does, not what a
/// designer would draw.
fn gallery_slots(request: &RequestType) -> Result<Vec<Slot>, Box<dyn std::error::Error>> {
    let policy = request
        .availability_policy()
        .ok_or("the booking starter has no [availability]")??;

    // Two commitments in the first offerable week, so the page shows what a
    // real calendar does to it: one afternoon loses its middle, and the buffer
    // eats a little more than the meeting itself.
    let busy = |day: &str, from: &str, to: &str| -> Result<Interval, Box<dyn std::error::Error>> {
        Ok(Interval {
            start: format!("{day}T{from}:00Z").parse()?,
            end: format!("{day}T{to}:00Z").parse()?,
        })
    };
    let busy = vec![
        busy("2026-03-03", "19:00", "20:00")?, // Tue 14:00–15:00 New York
        busy("2026-03-05", "18:30", "19:30")?, // Thu 13:30–14:30 New York
    ];

    Ok(availability(&policy, &busy, &[], &[], now()))
}

/// Render one booking-shaped page.
fn render_booking(
    entry: &Entry,
    variant: &Variant,
    theme: Theme,
) -> Result<mecha_manifest::BookingPage, Box<dyn std::error::Error>> {
    let slots = gallery_slots(&entry.request)?;
    let policy = entry
        .request
        .availability_policy()
        .ok_or("not a booking manifest")??;

    if let Variant::Poll = variant {
        // A poll is seeded from what the organizer could actually offer, so
        // the candidates are drawn from the same engine output rather than
        // from a blank 7×24 grid. Three answered, one not: the read of this
        // page is the pattern across a row, and a uniform one shows nothing.
        let answers = [
            Some(PollAnswer::Yes),
            Some(PollAnswer::IfNeeded),
            None,
            Some(PollAnswer::No),
            Some(PollAnswer::Yes),
        ];
        let candidates: Vec<PollCandidate> = slots
            .iter()
            .filter(|s| s.duration_minutes == 60)
            .take(answers.len())
            .zip(answers)
            .enumerate()
            .map(|(i, (slot, mine))| PollCandidate {
                start: slot.start,
                end: slot.end,
                duration_minutes: slot.duration_minutes,
                mine,
                // Fixed rather than random: a golden file cannot roll dice.
                yes_count: [4, 2, 3, 0, 1][i],
            })
            .collect();
        return Ok(poll_page(
            &candidates,
            &PollPageOptions {
                title: "When can we all meet?".into(),
                participant: "Dana".into(),
                timezone: policy.timezone,
                action: "#".into(),
                assets: String::new(),
                theme,
                deadline_local: Some("Friday 6pm".into()),
                responded: 3,
                total: 5,
                open: true,
                notice: None,
            },
        ));
    }

    let first_week = slots
        .first()
        .map(|s| mecha_manifest::week_of(s.start.with_timezone(&policy.timezone).date_naive()));
    let (weeks, values, errors) = match variant {
        Variant::Booking { weeks } => (*weeks, Map::new(), Vec::new()),
        // The slot the stranger picked rides back with the rejection. Losing
        // it would mean re-choosing a time because a name was too long.
        Variant::BookingErrors => {
            let mut values = Map::new();
            values.insert("requester_name".into(), json!("Sam"));
            values.insert("requester_email".into(), json!("sam@example"));
            // Validate the manifest's own fields, then add the chosen slot for
            // rendering. `_slot` is the page's machinery key, not a field, and
            // handing it to the validator would manufacture an error the box
            // never produces — the server keeps the two apart for this reason.
            let errors = entry
                .request
                .validate_at(&values, Phase::Submit)
                .err()
                .unwrap_or_default();
            if let Some(slot) = slots.first() {
                values.insert(
                    "_slot".into(),
                    json!(format!(
                        "{}|{}",
                        slot.start
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        slot.duration_minutes
                    )),
                );
            }
            (0, values, errors)
        }
        _ => unreachable!("render_booking is only called for booking variants"),
    };

    let page = entry.request.booking_page(
        &slots,
        &BookingOptions {
            action: "#".into(),
            assets: String::new(),
            theme,
            now: now(),
            week: first_week.map(|w| w + Duration::days(7 * weeks)),
            values,
            errors,
            stale_notice: None,
            ..BookingOptions::default()
        },
    )?;
    Ok(page)
}

// ---------------------------------------------------------------------------
// The manifests
// ---------------------------------------------------------------------------

fn entries() -> Result<Vec<Entry>, Box<dyn std::error::Error>> {
    let mut out = vec![
        Entry {
            id: "kinds".into(),
            blurb: "One field of every kind, plus acknowledgments.",
            toml: KINDS.into(),
            request: RequestType::from_toml(KINDS)?,
            bad_submission: Some(bad_kinds_submission()),
        },
        Entry {
            id: "conditions".into(),
            blurb: "show_when, across the operators worth seeing.",
            toml: CONDITIONS.into(),
            request: RequestType::from_toml(CONDITIONS)?,
            bad_submission: None,
        },
        Entry {
            id: "stepped".into(),
            blurb: "A multi-step form, and one step that only sometimes exists.",
            toml: STEPPED.into(),
            request: RequestType::from_toml(STEPPED)?,
            bad_submission: None,
        },
    ];

    // The shipped starters, rendered by the same path. These are the ones
    // somebody will actually copy, so seeing them is worth more than seeing a
    // synthetic type — and a starter that stopped rendering would show up here.
    let types = Path::new(env!("CARGO_MANIFEST_DIR")).join("types");
    let mut starters: Vec<PathBuf> = fs::read_dir(&types)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    starters.sort();
    for path in starters {
        let toml = fs::read_to_string(&path)?;
        let request = RequestType::from_toml(&toml)?;
        out.push(Entry {
            id: request.id.clone(),
            blurb: "A shipped starter, rendered by the same path.",
            toml,
            request,
            bad_submission: None,
        });
    }
    Ok(out)
}

/// A submission wrong in as many distinct ways as the validator has answers
/// for. Deliberately not exhaustive of the error space — it is a page someone
/// reads, and thirty messages would demonstrate nothing thirteen do not.
fn bad_kinds_submission() -> Map<String, Value> {
    // Over `long_answer`'s 600-character cap, which is the error worth showing:
    // it is the one every request type has, on the field a stranger writes into.
    let long = "Every one of these answers is wrong on purpose. ".repeat(20);
    json!({
        // `short_text` is required and simply absent.
        "reference_code": "nh-301",
        "long_answer": long,
        "email_address": "not-an-address",
        "reading": "javascript:alert(1)",
        "preferred_date": "2019-06-01",
        "headcount": 5000,
        "format": "carrier-pigeon",
        "topics": ["memory", "sandboxing", "evaluation"],
        // A urlencoded string aimed at a file field is somebody probing.
        "supporting_document": "cv.pdf",
        // `accurate` and `retention` are unticked, which is how a checkbox is
        // absent — a browser submits nothing for one.
    })
    .as_object()
    .expect("a json! object is an object")
    .clone()
}

const KINDS: &str = r##"# Every field kind the manifest defines, in one type.
#
# This is the gallery's specimen, not a starter — copy `speaking.toml` if you
# are writing a real request type. What it is good for is seeing what each
# `kind` renders as, and what each one enforces.

id = "kinds"
version = 1
title = "Every field kind"
description = "One of each. This page is a rendering gallery: the browser validates natively, and submitting goes nowhere."
retain_days = 30

[verification]
field = "email_address"

[[fields]]
name = "short_text"
label = "Text"
kind = "text"
max_length = 120
required = true
help = "One line. max_length is required on every text field — these forms sit on an unauthenticated endpoint, and an uncapped field is an unbounded write."

[[fields]]
name = "reference_code"
label = "Text, with a pattern"
kind = "text"
max_length = 16
pattern = "^[A-Z]{2}-[0-9]{4}$"
help = "Two letters, a hyphen, four digits — NH-0301. The pattern rides on the input and the browser enforces it; the server enforces the cap and the type, never the regex."

[[fields]]
name = "long_answer"
label = "Long text"
kind = "long_text"
max_length = 600
help = "Many lines, same cap rule. Free text, which is what the front door quarantines: a privileged run sees the extraction of this, never the prose."

[[fields]]
name = "email_address"
label = "Email"
kind = "email"
required = true
help = "Capped at 254 by default, the practical maximum for an address. [verification] names this field, which is why it has to be required."

[[fields]]
name = "reading"
label = "URL"
kind = "url"
help = "Data to show, never a thing to fetch. Nothing in this crate resolves one, and nothing downstream should — a form field is not a reason to make an outbound request to an address a stranger chose."

[[fields]]
name = "preferred_date"
label = "Date"
kind = "date"
min = "2026-01-01"
max = "2026-12-31"
help = "YYYY-MM-DD, inclusive bounds. Literal dates rather than offsets: an offset would have to be resolved against a clock, and the browser's clock is not the server's."

[[fields]]
name = "headcount"
label = "Integer"
kind = "integer"
min = 1
max = 500
help = "Inclusive bounds, enforced at both ends."

[[fields]]
name = "format"
label = "Select"
kind = "select"
required = true
help = "One of ours. Nothing a stranger types can change which value this carries, which is what keeps the kind of a request out of their hands."
options = [
  { value = "in_person", label = "In person" },
  { value = "remote", label = "Remote" },
  { value = "hybrid", label = "Hybrid" },
]

[[fields]]
name = "topics"
label = "Multi-select"
kind = "multi_select"
max_choices = 2
help = "Several of ours, up to max_choices. Renders as checkboxes rather than a multiple <select>, which nobody has ever operated correctly on a phone."
options = [
  { value = "memory", label = "Agent memory" },
  { value = "sandboxing", label = "Sandboxing" },
  { value = "evaluation", label = "Evaluation" },
  { value = "provenance", label = "Provenance" },
]

[[fields]]
name = "first_time"
label = "Checkbox"
kind = "bool"
help = "A single boolean. Distinct from an acknowledgment, which is always required and carries something to read first."

[[fields]]
name = "supporting_document"
label = "File"
kind = "file"
max_bytes = 4194304
accept = ["pdf", "png"]
help = "On the public form this renders as a note rather than an input: uploads happen after the address is verified, so the upload page is the one place a file input exists."

[[acknowledgments]]
id = "accurate"
label = "What I have written here is accurate."

[[acknowledgments]]
id = "retention"
label = "I understand this request is kept for 30 days."
description = "Retention is declared by the request type, not negotiated per submission."
"##;

const CONDITIONS: &str = r##"# `show_when`, and the evaluator that reads it.
#
# The browser hides a field and the server enforces both halves — a hidden
# field is never required and is never accepted. With JavaScript off every
# conditional field is simply shown, which is the safe direction: a visible
# optional field is a question you can ignore, where a hidden required one is
# a form that cannot be submitted and does not say why.

id = "conditions"
version = 1
title = "Conditional fields"
description = "Change the format, tick the box, put a big number in attendance — the fields below react. The server re-evaluates every one of these rules on submit."

[verification]
field = "email_address"

[[fields]]
name = "email_address"
label = "Your email"
kind = "email"
required = true

[[fields]]
name = "format"
label = "How will it run?"
kind = "select"
required = true
options = [
  { value = "in_person", label = "In person" },
  { value = "remote", label = "Remote" },
  { value = "hybrid", label = "Hybrid" },
]

[[fields]]
name = "venue"
label = "Where"
kind = "text"
max_length = 160
required = true
show_when = { field = "format", op = "in", value = ["in_person", "hybrid"] }
help = "op = \"in\". Required only when shown: the browser did not show it, so the server cannot insist on it."

[[fields]]
name = "travel_covered"
label = "Travel is covered"
kind = "bool"
show_when = { field = "format", op = "ne", value = "remote" }
help = "op = \"ne\"."

[[fields]]
name = "travel_budget"
label = "Budget, in dollars"
kind = "integer"
min = 0
show_when = { field = "travel_covered", op = "is_true" }
help = "op = \"is_true\", not \"present\" — an unticked checkbox is present and false, so `present` would show this whatever the answer."

[[fields]]
name = "headcount"
label = "Expected attendance"
kind = "integer"
min = 1
max = 5000

[[fields]]
name = "overflow_plan"
label = "Overflow plan"
kind = "long_text"
max_length = 600
show_when = { field = "headcount", op = "gt", value = 200 }
help = "op = \"gt\". A non-numeric value on either side does not hold, rather than erroring."

[[fields]]
name = "venue_notes"
label = "Anything about the venue"
kind = "long_text"
max_length = 600
show_when = { field = "venue", op = "present" }
help = "op = \"present\" means submitted and not empty. An empty string is absent, or every optional field would satisfy its own dependants."
"##;

const STEPPED: &str = r##"# Multi-step, which is server-side: one page per step, a POST between them.
#
# That is what makes it work with JavaScript off and survive a closed tab —
# a draft is a row on the box keyed by the capability token in the URL, not
# state in a browser somebody closed.
#
# Fields are declared once, in order, and a step references them by name. The
# system this borrows from keeps `fields`, `requiredFields`, `hiddenFields` and
# `conditionalFields` as four parallel lists, which can disagree; a name in
# both `requiredFields` and `hiddenFields` is a form nobody can submit. Here a
# field owns both facts, so there is nowhere for the contradiction to live.

id = "stepped"
version = 1
title = "A form in steps"
description = "Three steps, the last of which only exists for some answers."

[verification]
field = "email_address"

[[fields]]
name = "requester_name"
label = "Your name"
kind = "text"
max_length = 120
required = true

[[fields]]
name = "email_address"
label = "Your email"
kind = "email"
required = true

[[fields]]
name = "format"
label = "How will it run?"
kind = "select"
required = true
options = [
  { value = "in_person", label = "In person" },
  { value = "remote", label = "Remote" },
  { value = "hybrid", label = "Hybrid" },
]

[[fields]]
name = "summary"
label = "What are you asking for?"
kind = "long_text"
max_length = 1200
required = true

[[fields]]
name = "travel_covered"
label = "Travel is covered"
kind = "bool"

[[fields]]
name = "travel_notes"
label = "Anything about the travel"
kind = "long_text"
max_length = 600

[[steps]]
id = "who"
title = "Who you are"
fields = ["requester_name", "email_address"]

[[steps]]
id = "what"
title = "What you're asking"
description = "Enough to answer yes or no without a second exchange."
fields = ["format", "summary"]

# A whole step, skipped. A field on a hidden step is not required, however it
# is declared, and a record carrying a field the browser never showed is a
# record that differs from what was submitted.
[[steps]]
id = "travel"
title = "Travel"
fields = ["travel_covered", "travel_notes"]
show_when = { field = "format", op = "not_in", value = ["remote"] }
"##;

// ---------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------

/// Every `FieldKind`, as the tag its TOML spells it with.
///
/// **Exhaustive on purpose.** Adding a variant to `FieldKind` stops this
/// example compiling, and the fix is to add the arm here, the tag to
/// [`ALL_KINDS`] directly below, and a real field of that kind to `KINDS`.
/// Three edits sounds like friction until you consider the alternative: a
/// gallery that is missing a kind reads as a complete list of them.
fn kind_tag(kind: &FieldKind) -> &'static str {
    match kind {
        FieldKind::Text { .. } => "text",
        FieldKind::LongText { .. } => "long_text",
        FieldKind::Email { .. } => "email",
        FieldKind::Url { .. } => "url",
        FieldKind::Date { .. } => "date",
        FieldKind::Integer { .. } => "integer",
        FieldKind::Select { .. } => "select",
        FieldKind::MultiSelect { .. } => "multi_select",
        FieldKind::Bool => "bool",
        FieldKind::File { .. } => "file",
    }
}

/// The arms of [`kind_tag`], as data. Kept adjacent to it so the pair is hard
/// to update by half.
const ALL_KINDS: &[&str] = &[
    "text",
    "long_text",
    "email",
    "url",
    "date",
    "integer",
    "select",
    "multi_select",
    "bool",
    "file",
];

fn every_kind_is_shown(entries: &[Entry]) {
    let shown: BTreeSet<&str> = entries
        .iter()
        .flat_map(|e| e.request.fields.iter())
        .map(|f| kind_tag(&f.kind))
        .collect();
    let missing: Vec<&&str> = ALL_KINDS.iter().filter(|k| !shown.contains(**k)).collect();
    assert!(
        missing.is_empty(),
        "the gallery renders no field of kind {missing:?} — add one to `KINDS`, \
         or the gallery documents a smaller manifest format than the one that ships"
    );
}

// ---------------------------------------------------------------------------
// The landing page
// ---------------------------------------------------------------------------

/// The gallery as a machine-readable index: which themes exist, which types,
/// and which renderings each one has.
///
/// Emitted for the same reason the pages are generated rather than drawn. A
/// documentation site that lists the themes itself is a second place the set
/// of themes lives, and the second place is the one that goes stale — the
/// third palette ships, appears in every generated page, and is offered by
/// nothing.
fn contents_json(entries: &[Entry]) -> Result<String, serde_json::Error> {
    let themes: Vec<Value> = BUILT_IN_THEMES
        .iter()
        .map(|t| json!({ "name": t.name, "description": t.description }))
        .collect();
    let types: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "title": entry.request.title,
                "blurb": entry.blurb,
                "source": format!("source/{}.toml", entry.id),
                "schema": format!("schema/{}.json", entry.id),
                "renderings": entry
                    .variants()
                    .iter()
                    .map(|v| json!({ "label": v.label(), "file": v.file_name(entry) }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::to_string_pretty(&json!({ "themes": themes, "types": types }))
}

/// A contents page, for opening the gallery from a file manager. The
/// documentation site builds its own frame around these pages, so this one is
/// deliberately plain: it exists so `cargo run --example gallery` is a thing
/// you can look at without a static server or a docs build.
/// The specimen survey's questions: one of every general kind, which
/// [`every_question_kind_is_shown`] insists on. `times` is deliberately not
/// here — its specimen is `book.poll.html`, the seeded grid.
const SURVEY_QUESTIONS: &str = r#"
title = "Mid-semester feedback"
deadline = "2026-03-06T22:00:00-05:00"

[[questions]]
id = "paper"
prompt = "Which paper should the discussion section take on?"
kind = "choice"

[[questions.options]]
id = "world-models"
label = "World models are enough"
detail = "Chen et al., 2026"
link = "https://example.org/world-models"

[[questions.options]]
id = "affect-probes"
label = "Affective probes in fMRI decoding"

[[questions]]
id = "order"
prompt = "Rank the project formats you'd prefer."
kind = "ranking"

[[questions.options]]
id = "replication"
label = "Replication study"

[[questions.options]]
id = "reanalysis"
label = "Reanalysis of an open dataset"

[[questions.options]]
id = "proposal"
label = "Novel study proposal"

[[questions]]
id = "pace"
prompt = "The pace of the course so far is right."
kind = "likert"
points = 5
labels = ["Strongly disagree", "Disagree", "Neutral", "Agree", "Strongly agree"]

[[questions]]
id = "confidence"
prompt = "How confident do you feel about the material right now?"
kind = "vas"
anchor_min = "Not at all confident"
anchor_max = "Completely confident"

[[questions]]
id = "keep"
prompt = "What is working that we should keep doing?"
kind = "text"
max_length = 300
"#;

/// The open page's policy: the default reveal, anonymous — the promise
/// line and the owed-results note are part of what the page demonstrates.
const SURVEY_OPEN_RESULTS: &str = "
[results]
show = \"after_vote\"
identity = \"anonymous\"
";

/// The closed page's policy: named, so the voters disclosure renders too.
const SURVEY_CLOSED_RESULTS: &str = "
[results]
show = \"after_close\"
identity = \"named\"
";

fn survey_spec(results: &str) -> Result<PollSpec, Box<dyn std::error::Error>> {
    Ok(PollSpec::from_toml(&format!(
        "{SURVEY_QUESTIONS}{results}"
    ))?)
}

/// The arms of the survey's own exhaustiveness guard: a sixth general kind
/// added to `QuestionKind` stops this example compiling here, and the check
/// below fails until the specimen shows it.
fn question_kind_tag(kind: &QuestionKind) -> &'static str {
    match kind {
        QuestionKind::Choice { .. } => "choice",
        QuestionKind::Ranking { .. } => "ranking",
        QuestionKind::Likert { .. } => "likert",
        QuestionKind::Vas { .. } => "vas",
        QuestionKind::Text { .. } => "text",
        QuestionKind::Times { .. } => "times",
    }
}

fn every_question_kind_is_shown(spec: &PollSpec) {
    let shown: BTreeSet<&str> = spec
        .questions
        .iter()
        .map(|q| question_kind_tag(&q.kind))
        .collect();
    let missing: Vec<&str> = ["choice", "ranking", "likert", "vas", "text"]
        .into_iter()
        .filter(|kind| !shown.contains(kind))
        .collect();
    assert!(
        missing.is_empty(),
        "the survey specimen is missing kinds {missing:?} — a gallery \
         missing a kind reads as a complete list"
    );
}

/// Two states the served pages actually reach: the open form before your
/// vote (results owed, not shown), and the closed poll with everything —
/// tallies, the IRV rounds, prose answers, the named-voters disclosure.
fn survey_pages(
    open_spec: &PollSpec,
    closed_spec: &PollSpec,
    theme: Theme,
) -> Vec<(&'static str, String)> {
    let mut mine = Ballot::new();
    mine.insert("pace".into(), Answer::Likert(4));
    mine.insert("order".into(), Answer::Ranking(vec!["reanalysis".into()]));

    let form = survey_page(
        open_spec,
        &mine,
        None,
        &SurveyPageOptions {
            participant: Some("Priya".into()),
            action: "#".into(),
            assets: String::new(),
            theme,
            deadline_local: Some("Fri Mar 6, 10:00 PM EST".into()),
            responded: 2,
            total: Some(6),
            open: true,
            notice: None,
            show: Show::AfterVote,
            identity: Identity::Anonymous,
            // The open form is the one worth touching: `Specimen` loads the
            // enhancement so the ranking has its grip and the VAS its
            // slider, while every path to the network stays off. Rendering
            // this page `Inert` was a gallery that quietly showed the
            // JS-off baseline and called it the form.
            mode: PageMode::Specimen,
            resolution: None,
        },
    );

    // Closed and not projected: the electorate is final, so the ranking
    // renders its full runoff rather than first preferences alone.
    let results = specimen_results(closed_spec, false, false);
    let closed = survey_page(
        closed_spec,
        &mine,
        Some(&results),
        &SurveyPageOptions {
            participant: Some("Priya".into()),
            action: "#".into(),
            assets: String::new(),
            theme,
            deadline_local: Some("Fri Mar 6, 10:00 PM EST".into()),
            responded: 5,
            total: Some(5),
            open: false,
            notice: None,
            show: Show::AfterClose,
            identity: Identity::Named,
            // Not the closed page: a closed survey is read-only and stays
            // exactly as rendered, which is what the script itself does
            // (it returns early when there is no submit button). Shipping
            // an inert script here would only imply otherwise.
            mode: PageMode::Inert,
            resolution: Some("Replication it is — projects due the last week of classes.".into()),
        },
    );

    // The wall. Same ballots, read the way a projector must read them —
    // and open rather than closed, because the only moment this page is
    // worth looking at is while the room is still answering.
    let screen = screen_page(
        open_spec,
        &specimen_results(open_spec, true, true),
        &ScreenPageOptions {
            join_url: Some("nocturne.example.edu/p/lc/pace".into()),
            responded: 5,
            open: true,
            resolution: None,
            theme,
            assets: String::new(),
            // A golden file cannot poll: `live` is what wires the 2s
            // refresh in, and the page is designed to answer identically
            // without it, one reload behind.
            live: false,
        },
    );

    vec![
        ("survey.html", form.html),
        ("survey.closed.html", closed.html),
        ("survey.screen.html", screen.html),
    ]
}

/// Five specimen ballots. The numbers under them are computed, never
/// invented — and since this file stopped keeping its own copy of the
/// display logic, they are computed by the *same function the box serves
/// from*: [`build_results`]. A gallery whose figures are assembled a second
/// way documents a page nobody is served.
///
/// `open` and `projected` pass straight through. They are separate axes:
/// `projected` is the wall's stricter cut (prose becomes a word cloud, no
/// name rides a tally), while `open` decides whether a ranking shows IRV
/// rounds or first preferences alone.
fn specimen_results(spec: &PollSpec, open: bool, projected: bool) -> Vec<QuestionResults> {
    let ballots: Vec<(&str, Ballot)> = [
        (
            "Priya",
            vec![
                ("paper", Answer::Choice(vec!["world-models".into()])),
                (
                    "order",
                    Answer::Ranking(vec!["reanalysis".into(), "replication".into()]),
                ),
                ("pace", Answer::Likert(4)),
                ("confidence", Answer::Vas(72)),
                ("keep", Answer::Text("The live coding walkthroughs.".into())),
            ],
        ),
        (
            "Tal",
            vec![
                ("paper", Answer::Choice(vec!["affect-probes".into()])),
                (
                    "order",
                    Answer::Ranking(vec!["replication".into(), "proposal".into()]),
                ),
                ("pace", Answer::Likert(2)),
                ("confidence", Answer::Vas(41)),
                (
                    "keep",
                    Answer::Text("Problem sets that mirror the coding walkthroughs.".into()),
                ),
            ],
        ),
        (
            "Noor",
            vec![
                ("paper", Answer::Choice(vec!["world-models".into()])),
                (
                    "order",
                    Answer::Ranking(vec!["reanalysis".into(), "proposal".into()]),
                ),
                ("pace", Answer::Likert(4)),
                ("confidence", Answer::Vas(88)),
            ],
        ),
        (
            "Sam",
            vec![
                ("paper", Answer::Choice(vec!["affect-probes".into()])),
                ("order", Answer::Ranking(vec!["proposal".into()])),
                ("pace", Answer::Likert(5)),
                ("confidence", Answer::Vas(64)),
            ],
        ),
        (
            "Ida",
            vec![
                ("paper", Answer::Choice(vec!["world-models".into()])),
                (
                    "order",
                    Answer::Ranking(vec!["replication".into(), "reanalysis".into()]),
                ),
                ("pace", Answer::Likert(3)),
                ("confidence", Answer::Vas(55)),
                (
                    "keep",
                    Answer::Text("Office hours right after the coding section.".into()),
                ),
            ],
        ),
    ]
    .into_iter()
    .map(|(name, answers)| {
        (
            name,
            answers
                .into_iter()
                .map(|(q, a)| (q.to_string(), a))
                .collect::<Ballot>(),
        )
    })
    .collect();

    let ballots: Vec<(String, Ballot)> = ballots
        .into_iter()
        .map(|(name, ballot)| (name.to_string(), ballot))
        .collect();

    build_results(spec, &ballots, identity_of(spec), open, projected)
}

/// The specimen's identity policy, resolved the way the box resolves it —
/// from the spec, not by hand, so a change to the default lands here too.
fn identity_of(spec: &PollSpec) -> Identity {
    spec.results.identity(spec.audience.kind)
}

fn landing(entries: &[Entry]) -> String {
    let mut body = String::new();
    body.push_str(
        "<h1>Component gallery</h1>\n<p class=\"intro\">Generated by \
         <code>cargo run --example gallery</code>. Every page below is produced by the \
         renderer that serves the real forms, from a manifest in <code>source/</code>. \
         The forms validate natively in your browser and submit nowhere.</p>\n",
    );

    for entry in entries {
        body.push_str(&format!(
            "<section>\n<h2>{}</h2>\n<p class=\"blurb\">{}</p>\n\
             <p class=\"files\"><a href=\"source/{}.toml\">manifest</a> · \
             <a href=\"schema/{}.json\">JSON Schema</a></p>\n",
            escape_text(&entry.request.title),
            escape_text(entry.blurb),
            escape_text(&entry.id),
            escape_text(&entry.id),
        ));
        for theme in BUILT_IN_THEMES {
            body.push_str(&format!(
                "<p class=\"theme\"><span class=\"name\">{}</span> ",
                escape_text(theme.name)
            ));
            let links: Vec<String> = entry
                .variants()
                .iter()
                .map(|variant| {
                    format!(
                        "<a href=\"{}/{}\">{}</a>",
                        escape_text(theme.name),
                        escape_text(&variant.file_name(entry)),
                        escape_text(&variant.label())
                    )
                })
                .collect();
            body.push_str(&links.join(" · "));
            body.push_str("</p>\n");
        }
        body.push_str("</section>\n");
    }

    body.push_str(
        "<section>\n<h2>The survey</h2>\n<p class=\"blurb\">The general poll: \
         every question kind on one page, rendered by the renderer the gate \
         serves — the open form before your vote, the closed poll with \
         tallies, the runoff rounds and the named-voters disclosure, and the \
         projector's own view of the same ballots.</p>\n",
    );
    for theme in BUILT_IN_THEMES {
        body.push_str(&format!(
            "<p class=\"theme\"><span class=\"name\">{name}</span> \
             <a href=\"{name}/survey.html\">open form</a> · \
             <a href=\"{name}/survey.closed.html\">closed, with results</a> · \
             <a href=\"{name}/survey.screen.html\">the projector</a></p>\n",
            name = escape_text(theme.name)
        ));
    }
    body.push_str("</section>\n");

    body.push_str(&format!(
        "<p class=\"foot\">{} field kinds · {} themes · file fields cap at \
         {} MB per request type.</p>\n",
        ALL_KINDS.len(),
        BUILT_IN_THEMES.len(),
        MAX_FILE_BYTES_PER_TYPE / (1024 * 1024),
    ));

    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>mecha-factory · component gallery</title>\n\
         {}\n\
         <link rel=\"stylesheet\" href=\"gallery.css\">\n\
         </head>\n<body>\n{}<main>\n{body}</main>\n</body>\n</html>\n",
        mecha_manifest::FAVICON_LINK,
        mecha_manifest::site_header(),
    )
}

/// Structure for the contents page only, reading the same tokens a theme
/// emits. It hardcodes no colour, for the same reason the form's sheet does
/// not: a second place colours live is a second place they drift.
const LANDING_CSS: &str = r#"
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--ground); color: var(--text);
  font-family: var(--font-sans); line-height: 1.55;
}
header.site { padding: 1rem 1.5rem; border-bottom: 1px solid var(--line); }
header.site svg { display: block; height: 22px; width: auto; }
main { max-width: 46rem; margin: 0 auto; padding: 2.5rem 1.5rem 4rem; }
h1 { font-size: 1.75rem; margin: 0 0 0.5rem; }
h2 { font-size: 1.0625rem; font-family: var(--font-mono); font-weight: 600; margin: 0 0 0.25rem; }
p { margin: 0 0 0.5rem; }
.intro { color: var(--muted); margin-bottom: 2.5rem; }
.blurb { color: var(--muted); font-size: 0.9375rem; }
code { font-family: var(--font-mono); font-size: 0.9em; }
section { padding: 1.25rem 0; border-top: 1px solid var(--line); }
.files, .theme { font-size: 0.875rem; }
.theme .name {
  display: inline-block; min-width: 6rem; color: var(--muted);
  font-family: var(--font-mono); font-size: 0.8125rem;
}
a { color: var(--accent); text-decoration-thickness: 1px; text-underline-offset: 2px; }
a:focus-visible { outline: 2px solid var(--ring); outline-offset: 2px; border-radius: var(--radius); }
.foot { margin-top: 2.5rem; color: var(--muted); font-size: 0.8125rem; }
"#;
