//! A meeting poll's life after the release: the record as a state machine,
//! and the sweep that advances it.
//!
//! MEETING-POLL-UX-DESIGN.md's shape is *ask once, review once, then nothing
//! until it is booked*. Everything after the owner's release is deterministic:
//! invitations go out, the silent are nudged once, the poll closes on its own
//! terms, and either a clean winner is booked or the ranking is put in front
//! of the owner with reasons. No model anywhere — this is
//! SCHEDULING-DESIGN.md §2.2's rule ("the pipeline is a command, not a
//! prompt") applied to the half of the poll that used to be handed to the
//! agent as `polls status --json`.
//!
//! ### The record is the seam
//!
//! `~/.mecha/factory/polls/<id>.json` already held what the box never learns
//! — names against addresses and URLs. It now carries a `lifecycle` block, and
//! three verbs on one timer are each a consumer of it, exactly as
//! `~/.mecha/requests/*.json` is the seam between `factory-publish drain` and
//! `mecha-mail bookings`:
//!
//! - `factory-publish polls sweep` (this module) talks to the box and to
//!   nothing else: it observes the tally, decides when the poll has closed
//!   and what the verdict is, and writes that down. It sends no mail and
//!   creates no event, because this crate holds no calendar credential.
//! - `mecha-mail polls` reads what is due — invitations still unsent, a
//!   nudge, a booking — and does it from the owner's account, writing back
//!   `invites`, `nudged_at` and `booked`.
//! - `mecha polls sweep` stages the owner's pick as an outbox draft when the
//!   verdict is `pick`, and writes `resolution` when that draft is decided.
//!
//! Each writes only its own fields, and every field is optional on load, so a
//! record written by a newer binary never fails an older reader — a closed
//! enum in an append-only store is a wire format.
//!
//! ### The freebusy cache
//!
//! The tool used to take a path to `mecha-mail freebusy --json` output, which
//! meant the model running that pipeline and writing a file — and, because
//! `poll_meeting_create` is outbox-routed and executes at *release*, a file
//! that was an hour old by the time anyone approved the poll. The
//! `mecha-slots` timer already pipes fresh busy time through `slots push`
//! every two minutes; `slots push` now writes what it saw, policy included,
//! and a poll created without an explicit `freebusy` reads that. The
//! freshness refusal is unchanged; it is now a statement about the timer.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::availability::Interval;
use crate::remote::Remote;

/// The subject line the invitations go out under. `{title}` is substituted
/// per poll; the rest is the owner's to edit on the card.
pub const DEFAULT_SUBJECT: &str = "When can you meet? — {title}";

/// The invitation body. The owner's `{message}` renders first; the block
/// after it explains the link, the deadline and what happens next, in words
/// the recipient can act on without knowing what mecha is.
pub const DEFAULT_INVITATION: &str = "\
{message}

I'm finding a time for \"{title}\" ({duration} minutes). Could you mark which of these times work for you?

    {url}

Please answer by {deadline_local}. Tap a time to cycle through yes, if needed, and no. The link is yours alone — it is how the page knows the answers are yours — so please don't forward it. Once everyone has answered, I'll send a calendar invitation for the time that works.";

/// The one nudge, to whoever is silent a day before the deadline.
pub const DEFAULT_NUDGE: &str = "\
A quick reminder — the poll for \"{title}\" closes {deadline_local}, and I don't have your answer yet:

    {url}

It takes about ten seconds. Thank you!";

/// The slot a verdict or a pick lands on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Booking {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub duration_minutes: u32,
}

/// One row of the ranking as the owner reads it: the slot, the counts, and
/// the reason in a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedRow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub duration_minutes: u32,
    pub yes: usize,
    pub if_needed: usize,
    pub no: usize,
    pub feasible: bool,
    pub unanimous: bool,
    /// "everyone can", "Tal if needed", "Priya hasn't answered".
    pub reason: String,
}

/// The event `mecha-mail polls` created for a `book`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Booked {
    pub event_id: String,
    pub account: String,
    pub at: DateTime<Utc>,
}

/// The record's lifecycle block. Every field defaults, so a record written
/// before a field existed — or by a newer binary — loads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lifecycle {
    /// The mailbox the invitations and the event come from. `None` is the
    /// sweep's `--account`.
    pub account: Option<String>,
    /// The owner's sentence, rendered above the invitation block.
    pub message: Option<String>,
    pub subject: String,
    pub invitation: String,
    pub nudge: String,
    pub timezone: String,
    pub deadline: Option<DateTime<Utc>>,
    /// `unanimous` | `feasible` | `manual`, copied from the policy at create.
    /// Anything unrecognised reads as `manual` — the mode that books nothing.
    pub auto_book: String,
    pub nudge_hours_before: u32,
    pub nudge_min_lead_hours: u32,
    /// Name → when their invitation was sent. `None` is still owed.
    pub invites: BTreeMap<String, Option<DateTime<Utc>>>,
    /// Names owed a nudge right now; `mecha-mail polls` sends and clears it.
    pub nudge_due: Vec<String>,
    pub nudged_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    /// `book` | `pick` | `no_time` | `closed` (by hand, on the box).
    pub verdict: Option<String>,
    pub book: Option<Booking>,
    /// The top of the ranking with reasons, when the verdict is `pick`.
    pub ranked: Vec<RankedRow>,
    /// Who never answered, as of close.
    pub silent: Vec<String>,
    /// The outbox id of the owner's pick draft, when there is one.
    pub pick_item: Option<String>,
    pub booked: Option<Booked>,
    /// `mecha-mail polls` found the clean winner colliding with the owner's
    /// live calendar and made no event. Its field; this side reads it.
    pub conflict: Option<String>,
    /// The sentence the poll page shows once closed.
    pub resolution: Option<String>,
    pub box_closed_at: Option<DateTime<Utc>>,
}

impl Lifecycle {
    /// `manual` for anything this binary does not recognise: an unknown mode
    /// must never book.
    pub fn mode(&self) -> mecha_manifest::AutoBook {
        use mecha_manifest::AutoBook;
        match self.auto_book.as_str() {
            "unanimous" => AutoBook::Unanimous,
            "feasible" => AutoBook::Feasible,
            _ => AutoBook::Manual,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed_at.is_some()
    }

    /// Whether the booking page must still keep this poll's candidates off
    /// sale. Not "still asking" but "still committed": a closed poll whose
    /// event does not exist yet — the sweep decided, `mecha-mail polls` has
    /// not run, or the owner has not picked — would otherwise publish the
    /// winning slot for the minutes or days in between. A hand close and
    /// `no_time` release at once.
    pub fn holds_slots(&self) -> bool {
        !self.is_closed()
            || (self.booked.is_none()
                && matches!(self.verdict.as_deref(), Some("book") | Some("pick")))
    }

    /// The local rendering of the deadline, for the templates.
    pub fn deadline_local(&self) -> String {
        match (self.deadline, self.timezone.parse::<chrono_tz::Tz>()) {
            (Some(at), Ok(tz)) => at
                .with_timezone(&tz)
                .format("%A %-d %B at %-I:%M %p %Z")
                .to_string(),
            (Some(at), Err(_)) => at.format("%Y-%m-%d %H:%M UTC").to_string(),
            (None, _) => "when everyone has answered".to_string(),
        }
    }

    /// The one-line state the monitor and the briefing show.
    pub fn summary(&self) -> String {
        let sent = self.invites.values().filter(|v| v.is_some()).count();
        if self.verdict.is_none() && self.invites.is_empty() {
            // No invitations on record is not "all sent".
            return "—".to_string();
        }
        if let Some(verdict) = &self.verdict {
            return match verdict.as_str() {
                "book" if self.booked.is_some() => "booked".to_string(),
                "book" if self.conflict.is_some() => "booking blocked — collision".to_string(),
                "book" => "booking".to_string(),
                "pick" if self.booked.is_some() => "booked (your pick)".to_string(),
                "pick" => "needs a pick".to_string(),
                "no_time" => "no time found".to_string(),
                other => other.to_string(),
            };
        }
        if sent < self.invites.len() {
            format!("invites {sent}/{}", self.invites.len())
        } else {
            "invites sent".to_string()
        }
    }
}

/// What the sweep saw on the box, reduced to what `advance` needs. Built
/// from [`crate::polls::Status::Times`]; a struct of its own so the state
/// machine is testable from a literal.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// `open` or `closed`, the box's word.
    pub state: String,
    pub resolution: Option<String>,
    pub responded: usize,
    pub total: usize,
    pub silent: Vec<String>,
    pub ranked: Vec<mecha_manifest::RankedCandidate>,
    /// Name → candidate key → answer, for the reasons.
    pub answers: BTreeMap<String, BTreeMap<String, mecha_manifest::PollAnswer>>,
}

/// What a tick decided, for the person reading the journal — and the one
/// thing it needs the caller to do on the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advanced {
    pub lines: Vec<String>,
    /// Close the poll on the box with this sentence.
    pub close_box_with: Option<String>,
}

/// The candidate key `rank_poll` and the box agree on.
pub fn candidate_key(start: DateTime<Utc>, duration: u32) -> String {
    format!(
        "{}|{duration}",
        start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

/// Advance one record by one tick. Pure: the clock is an argument, the box
/// is [`Observed`], and the caller does the two things with side effects
/// (write the record, close on the box).
pub fn advance(life: &mut Lifecycle, seen: &Observed, now: DateTime<Utc>) -> Advanced {
    let mut lines = Vec::new();
    let mut close_box_with = None;

    if !life.is_closed() {
        // The box writes exactly `open` or `closed`. Anything else is a reply
        // this client could not read — never a close, and never terminal:
        // the record is left alone and the next tick asks again.
        if seen.state != "open" && seen.state != "closed" {
            lines.push(format!(
                "the box reports state `{}`, which this client does not understand; record untouched",
                seen.state
            ));
            return Advanced {
                lines,
                close_box_with,
            };
        }
        // Closed by hand on the box (the TUI's close key, the CLI): the
        // sweep records it and stops. Whatever resolution was written there
        // stands.
        if seen.state == "closed" {
            life.closed_at = Some(now);
            life.verdict = Some("closed".to_string());
            life.resolution = seen.resolution.clone();
            life.box_closed_at = Some(now);
            // A nudge queued before the owner closed by hand would mail
            // "the poll closes Thursday" about a poll that is closed.
            life.nudge_due.clear();
            lines.push("closed on the box by hand; nothing more to do".to_string());
            return Advanced {
                lines,
                close_box_with,
            };
        }

        let all_sent = !life.invites.is_empty() && life.invites.values().all(|v| v.is_some());
        let first_sent = life.invites.values().flatten().min().copied();

        // The nudge: once, to the silent, a day before the deadline — and
        // not at all when the deadline was already close when the
        // invitations went out.
        if let (Some(deadline), Some(first)) = (life.deadline, first_sent) {
            let nudge = chrono::Duration::hours(i64::from(life.nudge_hours_before));
            let lead = chrono::Duration::hours(i64::from(life.nudge_min_lead_hours));
            if life.nudge_hours_before > 0
                && all_sent
                && life.nudged_at.is_none()
                && life.nudge_due.is_empty()
                && !seen.silent.is_empty()
                && now >= deadline - nudge
                && now < deadline
                && deadline - first >= lead
            {
                life.nudge_due = seen.silent.clone();
                lines.push(format!("nudge due to {}", seen.silent.join(", ")));
            }
        }

        let everyone = seen.total > 0 && seen.responded >= seen.total;
        let past_deadline = life.deadline.is_some_and(|d| now >= d);
        if all_sent && (everyone || past_deadline) {
            life.closed_at = Some(now);
            life.silent = seen.silent.clone();
            life.nudge_due.clear();
            match mecha_manifest::auto_book(&seen.ranked, seen.responded, seen.total, life.mode()) {
                Some(winner) => {
                    life.verdict = Some("book".to_string());
                    life.book = Some(Booking {
                        start: winner.start,
                        end: winner.start
                            + chrono::Duration::minutes(i64::from(winner.duration_minutes)),
                        duration_minutes: winner.duration_minutes,
                    });
                    lines.push(format!(
                        "closed: everyone answered, {} is a clean winner — booking",
                        winner
                            .start
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    ));
                }
                None => {
                    life.verdict = Some("pick".to_string());
                    life.ranked = seen
                        .ranked
                        .iter()
                        .take(3)
                        .map(|c| RankedRow {
                            start: c.start,
                            end: c.start + chrono::Duration::minutes(i64::from(c.duration_minutes)),
                            duration_minutes: c.duration_minutes,
                            yes: c.yes,
                            if_needed: c.if_needed,
                            no: c.no,
                            feasible: c.feasible,
                            unanimous: c.unanimous,
                            reason: reason(c, &seen.answers, &seen.silent),
                        })
                        .collect();
                    lines.push(format!(
                        "closed: {} — the owner picks",
                        if everyone {
                            "no clean winner".to_string()
                        } else {
                            format!("deadline passed with {} silent", seen.silent.len())
                        }
                    ));
                }
            }
        }
    }

    // The mail half found the clean winner colliding with the owner's live
    // calendar: no event was made, and the decision comes back here — the
    // owner picks, over the full ranking, with the collision on its row.
    if life.is_closed()
        && life.verdict.as_deref() == Some("book")
        && life.booked.is_none()
        && life.conflict.is_some()
    {
        let reason = life.conflict.clone().unwrap_or_default();
        let collided = life.book.as_ref().map(|b| b.start);
        life.verdict = Some("pick".to_string());
        life.book = None;
        life.ranked = seen
            .ranked
            .iter()
            .take(3)
            .map(|c| {
                let mut why = self::reason(c, &seen.answers, &seen.silent);
                if collided == Some(c.start) {
                    why = format!("{why} — but {reason}");
                }
                RankedRow {
                    start: c.start,
                    end: c.start + chrono::Duration::minutes(i64::from(c.duration_minutes)),
                    duration_minutes: c.duration_minutes,
                    yes: c.yes,
                    if_needed: c.if_needed,
                    no: c.no,
                    feasible: c.feasible,
                    unanimous: c.unanimous,
                    reason: why,
                }
            })
            .collect();
        lines.push(format!(
            "the winning slot collided with the calendar ({reason}) — the owner picks"
        ));
    }

    // Once the outcome exists, the poll page gets its sentence.
    if life.is_closed() && life.box_closed_at.is_none() {
        if let Some(booked) = &life.booked {
            let sentence = life.resolution.clone().unwrap_or_else(|| {
                let when = life
                    .book
                    .as_ref()
                    .map(|b| local_range(b, &life.timezone))
                    .unwrap_or_else(|| booked.at.to_rfc3339());
                format!("Booked: {when}")
            });
            life.resolution = Some(sentence.clone());
            close_box_with = Some(sentence);
        } else if let Some(sentence) = &life.resolution {
            close_box_with = Some(sentence.clone());
        }
        if let Some(sentence) = &close_box_with {
            lines.push(format!("closing on the box: {sentence}"));
        }
    }

    Advanced {
        lines,
        close_box_with,
    }
}

/// The box answered `closed: false` to our close: someone closed it by hand
/// between the tally and the write, and the box keeps the resolution
/// written then. Record that — the box's sentence, not ours — rather than
/// a completed write that never happened.
pub fn box_refused_close(life: &mut Lifecycle, now: DateTime<Utc>) -> String {
    life.box_closed_at = Some(now);
    life.resolution = None;
    life.verdict = Some("closed".to_string());
    "already closed on the box by hand; its resolution stands, not ours".to_string()
}

/// "Tue 9 Sep, 2:00–3:00 PM EDT", in the poll's zone.
pub fn local_range(booking: &Booking, timezone: &str) -> String {
    match timezone.parse::<chrono_tz::Tz>() {
        Ok(tz) => {
            let start = booking.start.with_timezone(&tz);
            let end = booking.end.with_timezone(&tz);
            format!(
                "{}, {}–{}",
                start.format("%a %-d %b"),
                start.format("%-I:%M %p"),
                end.format("%-I:%M %p %Z")
            )
        }
        Err(_) => format!(
            "{}–{} UTC",
            booking.start.format("%a %-d %b %H:%M"),
            booking.end.format("%H:%M")
        ),
    }
}

/// The sentence beside a ranked row: who pays, who is missing.
fn reason(
    c: &mecha_manifest::RankedCandidate,
    answers: &BTreeMap<String, BTreeMap<String, mecha_manifest::PollAnswer>>,
    silent: &[String],
) -> String {
    use mecha_manifest::PollAnswer;
    if c.unanimous {
        return "everyone can".to_string();
    }
    let key = candidate_key(c.start, c.duration_minutes);
    let mut parts = Vec::new();
    let mut if_needed = Vec::new();
    let mut cannot = Vec::new();
    for (name, theirs) in answers {
        match theirs.get(&key) {
            Some(PollAnswer::IfNeeded) => if_needed.push(name.as_str()),
            Some(PollAnswer::No) => cannot.push(name.as_str()),
            _ => {}
        }
    }
    if !if_needed.is_empty() {
        parts.push(format!("{} if needed", if_needed.join(", ")));
    }
    if !cannot.is_empty() {
        parts.push(format!("{} can't", cannot.join(", ")));
    }
    if !silent.is_empty() {
        parts.push(format!(
            "{} {} answered",
            silent.join(", "),
            if silent.len() == 1 {
                "hasn't"
            } else {
                "haven't"
            }
        ));
    }
    if parts.is_empty() {
        "everyone who answered can".to_string()
    } else {
        parts.join("; ")
    }
}

// ---------------------------------------------------------------------------
// The record on disk.

fn record_dir() -> Result<PathBuf> {
    let dir = Remote::dir()?.join("polls");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A record with its lifecycle, loaded. Records from before the lifecycle
/// existed (or general polls, which have none) come back `None`.
pub struct Record {
    pub path: PathBuf,
    pub instrument: String,
    pub poll_id: String,
    pub title: String,
    pub value: Value,
    pub lifecycle: Lifecycle,
}

/// Every record that carries a lifecycle, by file name. Unreadable files are
/// a finding — reported, never skipped as if absent.
pub fn records() -> Result<(Vec<Record>, Vec<String>)> {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    let dir = record_dir()?;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    for path in paths {
        match load(&path) {
            Ok(Some(record)) => found.push(record),
            Ok(None) => {}
            Err(e) => problems.push(format!("{}: {e:#}", path.display())),
        }
    }
    Ok((found, problems))
}

/// One record by poll id, if it has a lifecycle.
pub fn record(poll_id: &str) -> Result<Option<Record>> {
    let path = record_dir()?.join(format!("{poll_id}.json"));
    if !path.exists() {
        return Ok(None);
    }
    load(&path)
}

fn load(path: &std::path::Path) -> Result<Option<Record>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: Value = serde_json::from_str(&text).context("not JSON")?;
    let Some(life) = value.get("lifecycle").filter(|v| v.is_object()) else {
        return Ok(None);
    };
    let lifecycle: Lifecycle = serde_json::from_value(life.clone()).context("lifecycle")?;
    let field = |k: &str| value[k].as_str().unwrap_or_default().to_string();
    Ok(Some(Record {
        path: path.to_path_buf(),
        instrument: field("instrument"),
        poll_id: field("poll_id"),
        title: field("title"),
        value,
        lifecycle,
    }))
}

/// The lifecycle fields this half owns. `save` writes these and nothing
/// else, into the file as it is *now* — `mecha-mail polls` runs on the same
/// timer and owns `invites`, `nudged_at` and `booked`, and a snapshot taken
/// before a round of box calls would write its answers back over theirs:
/// a lost `invites` entry is a second invitation, a lost `booked` a second
/// calendar event.
pub const OWNED: &[&str] = &[
    "nudge_due",
    "closed_at",
    "verdict",
    "book",
    "ranked",
    "silent",
    "resolution",
    "box_closed_at",
];

/// Write this half's fields back into the record, re-read first so another
/// verb's writes since the load survive; temp-sibling-and-rename so a
/// reader never sees half a file.
pub fn save(record: &mut Record) -> Result<()> {
    let mine = serde_json::to_value(&record.lifecycle)?;
    let mut current = match std::fs::read_to_string(&record.path) {
        Ok(text) => serde_json::from_str::<Value>(&text).with_context(|| {
            format!(
                "{} changed under the sweep and is not JSON",
                record.path.display()
            )
        })?,
        // Gone since the load: write what we have rather than lose the tick.
        Err(_) => record.value.clone(),
    };
    if !current["lifecycle"].is_object() {
        current["lifecycle"] = json!({});
    }
    for key in OWNED {
        current["lifecycle"][*key] = mine[*key].clone();
    }
    record.value = current;
    let tmp = record.path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&record.value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &record.path)
        .with_context(|| format!("renaming into {}", record.path.display()))?;
    Ok(())
}

/// The candidate slots of every poll still open, as holds for the booking
/// engine — SCHEDULING-DESIGN.md §5.5: the public page must not sell a slot
/// a poll is still asking about.
///
/// An unreadable record, or a candidate that does not parse, is an error
/// rather than a hold quietly not taken: the caller is `slots push`, and a
/// push made on the strength of a file it could not read publishes exactly
/// the times an open poll is still asking about.
pub fn open_holds() -> Result<Vec<Interval>> {
    let (records, problems) = records()?;
    anyhow::ensure!(
        problems.is_empty(),
        "refusing to compute holds over poll records that could not be read:\n{}",
        problems.join("\n")
    );
    let mut holds = Vec::new();
    for record in records.iter().filter(|r| r.lifecycle.holds_slots()) {
        let Some(candidates) = record.value["candidates"].as_array() else {
            continue;
        };
        for (i, c) in candidates.iter().enumerate() {
            let at = |k: &str| -> Result<DateTime<Utc>> {
                c[k].as_str().and_then(|s| s.parse().ok()).with_context(|| {
                    format!(
                        "{}: candidate {i} has no readable `{k}`",
                        record.path.display()
                    )
                })
            };
            holds.push(Interval {
                start: at("start")?,
                end: at("end")?,
            });
        }
    }
    Ok(holds)
}

// ---------------------------------------------------------------------------
// The freebusy cache.

/// What `slots push` last saw: the policy it computed with and the busy
/// time it was handed, together — so a poll seeded from this uses exactly
/// what the booking page promised.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFreebusy {
    pub instrument: String,
    pub policy_path: String,
    pub policy: String,
    pub cached_at: DateTime<Utc>,
    pub freebusy: Value,
}

fn cache_dir() -> Result<PathBuf> {
    let dir = Remote::dir()?.join("freebusy");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Record the pipeline's input for `<instrument>`.
pub fn remember_freebusy(
    instrument: &str,
    policy_path: &str,
    policy: &str,
    freebusy_json: &str,
) -> Result<PathBuf> {
    let freebusy: Value = serde_json::from_str(freebusy_json).context("freebusy is not JSON")?;
    let doc = CachedFreebusy {
        instrument: instrument.to_string(),
        policy_path: policy_path.to_string(),
        policy: policy.to_string(),
        cached_at: Utc::now(),
        freebusy,
    };
    let path = cache_dir()?.join(format!("{instrument}.json"));
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// The pipeline's last input for `<instrument>`, if it has ever run.
pub fn cached_freebusy(instrument: &str) -> Result<Option<CachedFreebusy>> {
    let path = cache_dir()?.join(format!("{instrument}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(
        serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?,
    ))
}

/// Every instrument the pipeline has cached for — the default when a create
/// names none and there is exactly one.
pub fn cached_instruments() -> Result<Vec<String>> {
    let mut ids: Vec<String> = std::fs::read_dir(cache_dir()?)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(str::to_string)
        })
        .collect();
    ids.sort();
    Ok(ids)
}

/// The lifecycle a fresh record starts with.
#[allow(clippy::too_many_arguments)]
pub fn fresh(
    policy: &crate::availability::Policy,
    names: &[String],
    deadline: DateTime<Utc>,
    account: Option<&str>,
    message: Option<&str>,
    subject: &str,
    invitation: &str,
) -> Lifecycle {
    Lifecycle {
        account: account.map(str::to_string),
        message: message.map(str::to_string),
        subject: subject.to_string(),
        invitation: invitation.to_string(),
        nudge: DEFAULT_NUDGE.to_string(),
        timezone: policy.timezone.to_string(),
        deadline: Some(deadline),
        auto_book: policy.poll.auto_book.as_str().to_string(),
        nudge_hours_before: policy.poll.nudge_hours_before,
        nudge_min_lead_hours: policy.poll.nudge_min_lead_hours,
        invites: names.iter().map(|n| (n.clone(), None)).collect(),
        ..Lifecycle::default()
    }
}

/// The lifecycle as a tool answer reads it — `summary` first, the fields after.
pub fn describe(life: &Lifecycle) -> Value {
    json!({
        "summary": life.summary(),
        "deadline": life.deadline,
        "deadline_local": life.deadline_local(),
        "auto_book": life.auto_book,
        "invites": life.invites,
        "nudged_at": life.nudged_at,
        "closed_at": life.closed_at,
        "verdict": life.verdict,
        "book": life.book,
        "ranked": life.ranked,
        "silent": life.silent,
        "booked": life.booked,
        "resolution": life.resolution,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mecha_manifest::{rank_poll, PollAnswer};

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    const WED: &str = "2030-02-05T18:00:00Z";
    const THU: &str = "2030-02-06T15:00:00Z";

    fn fixture(mode: &str) -> Lifecycle {
        let mut life = Lifecycle {
            timezone: "America/New_York".into(),
            deadline: Some(t("2030-02-01T22:00:00Z")),
            auto_book: mode.into(),
            nudge_hours_before: 24,
            nudge_min_lead_hours: 36,
            ..Lifecycle::default()
        };
        life.invites
            .insert("Priya".into(), Some(t("2030-01-29T14:00:00Z")));
        life.invites
            .insert("Tal".into(), Some(t("2030-01-29T14:00:00Z")));
        life
    }

    /// Two people over two slots, answers by name.
    fn seen(answers: &[(&str, Option<(&str, &str)>)]) -> Observed {
        let candidates = vec![(t(WED), 60u32), (t(THU), 60u32)];
        let mut by_name = BTreeMap::new();
        let mut silent = Vec::new();
        for (name, theirs) in answers {
            match theirs {
                Some((wed, thu)) => {
                    let mut map = BTreeMap::new();
                    map.insert(candidate_key(t(WED), 60), PollAnswer::parse(wed).unwrap());
                    map.insert(candidate_key(t(THU), 60), PollAnswer::parse(thu).unwrap());
                    by_name.insert(name.to_string(), map);
                }
                None => silent.push(name.to_string()),
            }
        }
        let rows: Vec<_> = by_name.values().cloned().collect();
        Observed {
            state: "open".into(),
            resolution: None,
            responded: by_name.len(),
            total: answers.len(),
            silent,
            ranked: rank_poll(&candidates, &rows, answers.len()),
            answers: by_name,
        }
    }

    /// Everyone answered and one slot is unanimous: the poll closes early —
    /// the deadline is days away — and the verdict is a booking.
    #[test]
    fn a_clean_winner_closes_early_and_books() {
        let mut life = fixture("unanimous");
        let now = t("2030-01-30T10:00:00Z");
        let out = advance(
            &mut life,
            &seen(&[
                ("Priya", Some(("yes", "no"))),
                ("Tal", Some(("yes", "yes"))),
            ]),
            now,
        );
        assert_eq!(life.closed_at, Some(now));
        assert_eq!(life.verdict.as_deref(), Some("book"));
        assert_eq!(life.book.as_ref().unwrap().start, t(WED));
        assert_eq!(life.book.as_ref().unwrap().end, t("2030-02-05T19:00:00Z"));
        assert!(
            out.close_box_with.is_none(),
            "the box closes once the event exists"
        );
        assert!(out.lines[0].contains("clean winner"), "{:?}", out.lines);
    }

    /// The same answers under `manual`: the owner picks, with reasons.
    #[test]
    fn manual_never_books_and_the_ranking_carries_reasons() {
        let mut life = fixture("manual");
        advance(
            &mut life,
            &seen(&[
                ("Priya", Some(("yes", "no"))),
                ("Tal", Some(("if_needed", "yes"))),
            ]),
            t("2030-01-30T10:00:00Z"),
        );
        assert_eq!(life.verdict.as_deref(), Some("pick"));
        assert!(life.book.is_none());
        assert_eq!(life.ranked[0].start, t(WED));
        assert_eq!(life.ranked[0].reason, "Tal if needed");
        assert_eq!(life.ranked[1].reason, "Priya can't");
    }

    /// Before the deadline, a silent participant holds the poll open; after
    /// it, the poll closes as a pick and names them.
    #[test]
    fn silence_holds_until_the_deadline_then_becomes_a_pick() {
        let mut life = fixture("feasible");
        let observed = seen(&[("Priya", Some(("yes", "yes"))), ("Tal", None)]);
        let out = advance(&mut life, &observed, t("2030-01-30T10:00:00Z"));
        assert!(!life.is_closed(), "{:?}", out.lines);

        advance(&mut life, &observed, t("2030-02-01T22:00:00Z"));
        assert!(life.is_closed());
        assert_eq!(
            life.verdict.as_deref(),
            Some("pick"),
            "no mode books over silence"
        );
        assert_eq!(life.silent, vec!["Tal".to_string()]);
        assert!(
            life.ranked[0].reason.contains("Tal hasn't answered"),
            "{}",
            life.ranked[0].reason
        );
    }

    /// One nudge, to the silent only, inside the window before the deadline
    /// — and never when the invitation itself was recent.
    #[test]
    fn the_nudge_fires_once_in_its_window_and_never_after_a_short_lead() {
        let mut life = fixture("unanimous");
        let observed = seen(&[("Priya", Some(("yes", "yes"))), ("Tal", None)]);

        // Two days out: nothing.
        advance(&mut life, &observed, t("2030-01-30T22:00:00Z"));
        assert!(life.nudge_due.is_empty());

        // Inside 24h: Tal is owed one.
        advance(&mut life, &observed, t("2030-02-01T00:00:00Z"));
        assert_eq!(life.nudge_due, vec!["Tal".to_string()]);

        // The mail half sends it; the next tick does not re-queue.
        life.nudge_due.clear();
        life.nudged_at = Some(t("2030-02-01T00:02:00Z"));
        advance(&mut life, &observed, t("2030-02-01T06:00:00Z"));
        assert!(life.nudge_due.is_empty());

        // A poll invited 20h before its deadline never nudges: eleven hours
        // after an invitation is nagging.
        let mut rushed = fixture("unanimous");
        for sent in rushed.invites.values_mut() {
            *sent = Some(t("2030-02-01T02:00:00Z"));
        }
        advance(&mut rushed, &observed, t("2030-02-01T12:00:00Z"));
        assert!(rushed.nudge_due.is_empty());

        // Zero disables it outright.
        let mut quiet = fixture("unanimous");
        quiet.nudge_hours_before = 0;
        advance(&mut quiet, &observed, t("2030-02-01T00:00:00Z"));
        assert!(quiet.nudge_due.is_empty());
    }

    /// Nothing closes while an invitation is still owed — a poll nobody has
    /// been asked about cannot have a deadline pass on them.
    #[test]
    fn an_unsent_invitation_holds_the_poll_open() {
        let mut life = fixture("unanimous");
        life.invites.insert("Tal".into(), None);
        advance(
            &mut life,
            &seen(&[("Priya", Some(("yes", "yes"))), ("Tal", None)]),
            t("2030-02-02T10:00:00Z"),
        );
        assert!(!life.is_closed());
    }

    /// The box gets its sentence only once the event exists (or the owner
    /// has written one), and the sentence is in the poll's own zone.
    #[test]
    fn the_box_closes_with_the_outcome_sentence() {
        let mut life = fixture("unanimous");
        let observed = seen(&[
            ("Priya", Some(("yes", "no"))),
            ("Tal", Some(("yes", "yes"))),
        ]);
        advance(&mut life, &observed, t("2030-01-30T10:00:00Z"));
        // Booking pending: the box stays open.
        let out = advance(&mut life, &observed, t("2030-01-30T10:02:00Z"));
        assert!(out.close_box_with.is_none());

        life.booked = Some(Booked {
            event_id: "ev1".into(),
            account: "work".into(),
            at: t("2030-01-30T10:03:00Z"),
        });
        let out = advance(&mut life, &observed, t("2030-01-30T10:04:00Z"));
        assert_eq!(
            out.close_box_with.as_deref(),
            Some("Booked: Tue 5 Feb, 1:00 PM–2:00 PM EST")
        );
        assert_eq!(life.resolution, out.close_box_with);

        // A rejected pick: mecha wrote the sentence, the sweep passes it on.
        let mut rejected = fixture("manual");
        advance(&mut rejected, &observed, t("2030-01-30T10:00:00Z"));
        rejected.verdict = Some("no_time".into());
        rejected.resolution = Some("No time found".into());
        let out = advance(&mut rejected, &observed, t("2030-01-30T11:00:00Z"));
        assert_eq!(out.close_box_with.as_deref(), Some("No time found"));
    }

    /// A reply this client could not read is neither a close nor terminal:
    /// the record is untouched and the next tick asks again.
    #[test]
    fn an_unreadable_box_state_is_a_finding_not_a_close() {
        let mut life = fixture("unanimous");
        let before = life.clone();
        let mut observed = seen(&[
            ("Priya", Some(("yes", "yes"))),
            ("Tal", Some(("yes", "yes"))),
        ]);
        observed.state = "?".into();
        let out = advance(&mut life, &observed, t("2030-01-30T10:00:00Z"));
        assert_eq!(life, before, "nothing recorded over an unknown state");
        assert!(
            out.lines[0].contains("does not understand"),
            "{:?}",
            out.lines
        );
        assert!(out.close_box_with.is_none());
    }

    /// The hold outlives the close: a decided poll whose event does not
    /// exist yet keeps its candidates off the booking page.
    #[test]
    fn holds_last_until_the_booking_exists() {
        let mut life = fixture("unanimous");
        assert!(life.holds_slots(), "open");
        life.closed_at = Some(t("2030-01-30T10:00:00Z"));
        life.verdict = Some("book".into());
        assert!(life.holds_slots(), "decided, not yet booked");
        life.verdict = Some("pick".into());
        assert!(life.holds_slots(), "waiting on the owner");
        life.booked = Some(Booked {
            event_id: "ev".into(),
            account: "a".into(),
            at: t("2030-01-30T10:03:00Z"),
        });
        assert!(!life.holds_slots(), "the event exists");
        let mut gone = fixture("unanimous");
        gone.closed_at = Some(t("2030-01-30T10:00:00Z"));
        gone.verdict = Some("no_time".into());
        assert!(!gone.holds_slots(), "no time: nothing to protect");
        gone.verdict = Some("closed".into());
        assert!(!gone.holds_slots(), "closed by hand: nothing to protect");
    }

    /// The mail half could not book the clean winner: the verdict becomes
    /// the owner's pick over the full ranking, the collision on its row,
    /// and the slot is not offered as a booking again.
    #[test]
    fn a_conflict_from_the_mail_half_becomes_a_pick_over_the_full_ranking() {
        let mut life = fixture("unanimous");
        let observed = seen(&[
            ("Priya", Some(("yes", "yes"))),
            ("Tal", Some(("yes", "no"))),
        ]);
        advance(&mut life, &observed, t("2030-01-30T10:00:00Z"));
        assert_eq!(life.verdict.as_deref(), Some("book"));
        assert_eq!(life.book.as_ref().unwrap().start, t(WED));

        life.conflict = Some("your calendar now has something at that time".into());
        let out = advance(&mut life, &observed, t("2030-01-30T10:04:00Z"));
        assert_eq!(life.verdict.as_deref(), Some("pick"));
        assert!(life.book.is_none());
        assert_eq!(life.ranked.len(), 2, "the whole ranking, not the one slot");
        assert_eq!(life.ranked[0].start, t(WED));
        assert!(
            life.ranked[0]
                .reason
                .ends_with("— but your calendar now has something at that time"),
            "{}",
            life.ranked[0].reason
        );
        assert_eq!(life.ranked[1].reason, "Tal can't");
        assert!(out.lines[0].contains("collided"), "{:?}", out.lines);
        // And it is not re-flipped on the next tick.
        let before = life.clone();
        advance(&mut life, &observed, t("2030-01-30T10:06:00Z"));
        assert_eq!(life, before);
    }

    /// The box refusing our close (someone closed it by hand in between)
    /// is recorded as the box's outcome, never as ours having been written.
    #[test]
    fn a_refused_close_records_the_boxs_outcome_not_ours() {
        let mut life = fixture("unanimous");
        let observed = seen(&[
            ("Priya", Some(("yes", "no"))),
            ("Tal", Some(("yes", "yes"))),
        ]);
        advance(&mut life, &observed, t("2030-01-30T10:00:00Z"));
        life.booked = Some(Booked {
            event_id: "ev1".into(),
            account: "work".into(),
            at: t("2030-01-30T10:03:00Z"),
        });
        let out = advance(&mut life, &observed, t("2030-01-30T10:04:00Z"));
        assert!(out.close_box_with.is_some());
        let line = box_refused_close(&mut life, t("2030-01-30T10:04:01Z"));
        assert!(line.contains("its resolution stands"));
        assert!(life.box_closed_at.is_some(), "retired: the box is settled");
        assert_eq!(life.resolution, None, "not a sentence the box never wrote");
        assert_eq!(life.verdict.as_deref(), Some("closed"));
    }

    /// A close by hand on the box is final: recorded, resolution kept, no
    /// verdict computed over it — and a queued nudge dies with it.
    #[test]
    fn a_hand_close_on_the_box_is_recorded_and_left_alone() {
        let mut life = fixture("feasible");
        life.nudge_due = vec!["Tal".into()];
        let mut observed = seen(&[
            ("Priya", Some(("yes", "yes"))),
            ("Tal", Some(("yes", "yes"))),
        ]);
        observed.state = "closed".into();
        observed.resolution = Some("Moved to Slack".into());
        let out = advance(&mut life, &observed, t("2030-01-30T10:00:00Z"));
        assert_eq!(life.verdict.as_deref(), Some("closed"));
        assert_eq!(life.resolution.as_deref(), Some("Moved to Slack"));
        assert!(life.book.is_none());
        assert!(life.nudge_due.is_empty(), "no reminder about a closed poll");
        assert!(out.close_box_with.is_none());
    }

    /// The wire format: a lifecycle from a newer binary loads, and an
    /// unknown mode books nothing.
    #[test]
    fn unknown_fields_load_and_an_unknown_mode_is_manual() {
        let loaded: Lifecycle = serde_json::from_value(json!({
            "auto_book": "always",
            "verdict": "something_new",
            "a_field_from_the_future": 1
        }))
        .unwrap();
        assert_eq!(loaded.mode(), mecha_manifest::AutoBook::Manual);
        assert_eq!(loaded.verdict.as_deref(), Some("something_new"));
        assert_eq!(loaded.summary(), "something_new");
    }

    /// The cache round-trips with the policy it was computed under, and the
    /// holds are exactly the candidates of the polls still open.
    #[test]
    fn the_cache_and_the_holds_live_in_the_factory_dir() {
        let _guard = crate::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("MECHA_HOME", home.path());

        assert!(cached_instruments().unwrap().is_empty());
        remember_freebusy(
            "book",
            "/x/policy.toml",
            "timezone = \"UTC\"",
            r#"{"busy": []}"#,
        )
        .unwrap();
        let cached = cached_freebusy("book").unwrap().expect("cached");
        assert_eq!(cached.policy, "timezone = \"UTC\"");
        assert_eq!(cached.policy_path, "/x/policy.toml");
        assert_eq!(cached.freebusy["busy"], json!([]));
        assert_eq!(cached_instruments().unwrap(), vec!["book".to_string()]);
        assert!(cached_freebusy("other").unwrap().is_none());

        let dir = home.path().join("factory").join("polls");
        std::fs::create_dir_all(&dir).unwrap();
        let record = |id: &str, closed: bool| {
            json!({
                "instrument": "book", "poll_id": id, "title": id,
                "candidates": [{"start": WED, "end": "2030-02-05T19:00:00Z", "duration_minutes": 60}],
                "lifecycle": { "closed_at": closed.then_some(WED) },
            })
        };
        std::fs::write(dir.join("open.json"), record("open", false).to_string()).unwrap();
        std::fs::write(dir.join("done.json"), record("done", true).to_string()).unwrap();
        // A general poll's record has no lifecycle and is not a hold.
        std::fs::write(
            dir.join("survey.json"),
            json!({"poll_id": "survey"}).to_string(),
        )
        .unwrap();

        let holds = open_holds().unwrap();
        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0].start, t(WED));

        // One unreadable record refuses the whole computation: a push over
        // it would publish the times that record is holding.
        std::fs::write(dir.join("broken.json"), "{not json").unwrap();
        let err = open_holds().unwrap_err().to_string();
        assert!(err.contains("broken.json"), "{err}");
        let (records, problems) = records().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(problems.len(), 1, "an unreadable record is a finding");
        std::fs::remove_file(dir.join("broken.json")).unwrap();

        // So does a candidate that does not parse.
        let mut torn = record("torn", false);
        torn["candidates"][0]["end"] = json!("yesterday-ish");
        std::fs::write(dir.join("torn.json"), torn.to_string()).unwrap();
        let err = open_holds().unwrap_err().to_string();
        assert!(
            err.contains("candidate 0") && err.contains("`end`"),
            "{err}"
        );
        std::fs::remove_file(dir.join("torn.json")).unwrap();

        // A closed poll still waiting on its event holds its slot.
        let mut decided = record("decided", true);
        decided["lifecycle"]["verdict"] = json!("book");
        std::fs::write(dir.join("decided.json"), decided.to_string()).unwrap();
        assert_eq!(open_holds().unwrap().len(), 2, "open + decided-not-booked");

        // `save` merges only this half's fields into the file as it is now:
        // the mail half's write since the load survives.
        let (loaded, _) = super::records().unwrap();
        let mut mine = loaded.into_iter().find(|r| r.poll_id == "open").unwrap();
        let mut theirs: Value =
            serde_json::from_str(&std::fs::read_to_string(&mine.path).unwrap()).unwrap();
        theirs["lifecycle"]["invites"] = json!({"Priya": "2030-01-29T14:00:00Z"});
        theirs["lifecycle"]["booked"] = json!({"event_id": "ev9", "account": "a", "at": WED});
        std::fs::write(&mine.path, theirs.to_string()).unwrap();
        mine.lifecycle.verdict = Some("pick".into());
        mine.lifecycle.closed_at = Some(t(WED));
        save(&mut mine).unwrap();
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&mine.path).unwrap()).unwrap();
        assert_eq!(
            after["lifecycle"]["invites"]["Priya"],
            "2030-01-29T14:00:00Z"
        );
        assert_eq!(
            after["lifecycle"]["booked"]["event_id"], "ev9",
            "theirs kept"
        );
        assert_eq!(after["lifecycle"]["verdict"], "pick", "mine written");
        assert_eq!(after["lifecycle"]["closed_at"], WED);

        std::env::remove_var("MECHA_HOME");
    }

    #[test]
    fn the_summary_reads_as_one_line() {
        assert_eq!(
            Lifecycle::default().summary(),
            "—",
            "no invitations on record is not done"
        );
        let mut life = fixture("unanimous");
        assert_eq!(life.summary(), "invites sent");
        life.invites.insert("Tal".into(), None);
        assert_eq!(life.summary(), "invites 1/2");
        life.verdict = Some("pick".into());
        assert_eq!(life.summary(), "needs a pick");
        life.booked = Some(Booked {
            event_id: "e".into(),
            account: "a".into(),
            at: t("2030-01-30T10:03:00Z"),
        });
        assert_eq!(life.summary(), "booked (your pick)");
    }
}
