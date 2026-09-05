//! The availability engine: what "bookable" means, as a pure function.
//!
//! `availability(policy, busy, holds, bookings, now)` → the slots a stranger
//! may claim. Everything here is deterministic arithmetic over inputs the
//! caller gathered — free/busy intervals from `mecha-mail freebusy`, holds
//! from open polls, bookings from the ledger — so the whole of "when can I
//! be booked" is unit-testable with no credential, no clock and no network
//! anywhere. The slot-refresh pipeline runs this on a timer and pushes the
//! result to the box as data; the box never computes availability, only
//! subtracts from what was pushed.
//!
//! Time zones: windows are stated in the user's IANA zone and slots are
//! emitted as UTC instants. A window boundary that falls inside the
//! spring-forward gap takes the first instant that exists (late, not lost);
//! one inside the repeated autumn hour takes the first occurrence — the same
//! both-directions rule mecha's cron already earned.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// One occupied span, UTC. The JSON shape (`{"start": "…Z", "end": "…Z"}`)
/// is the contract with `mecha-mail freebusy --json`, deliberately matched
/// as data rather than as a shared crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// A slot the engine offers: an instant, an end, and the duration that made
/// it. Slots of different durations may share a start; slots of one duration
/// overlap on the increment grid — both are ordinary for a booking page,
/// which resolves them at claim time, not at offer time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub duration_minutes: u32,
}

/// A weekly recurring bookable window, in the policy's zone.
#[derive(Debug, Clone)]
pub struct WeeklyWindow {
    pub day: Weekday,
    pub start: NaiveTime,
    pub end: NaiveTime,
}

/// A date that departs from the weekly pattern. Empty `windows` closes the
/// day; a non-empty list replaces the weekly windows entirely for that date.
#[derive(Debug, Clone)]
pub struct DateOverride {
    pub date: NaiveDate,
    pub windows: Vec<(NaiveTime, NaiveTime)>,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub timezone: Tz,
    /// Offered meeting lengths, minutes.
    pub durations: Vec<u32>,
    /// Clear air demanded on each side of anything already occupied.
    pub buffer_minutes: u32,
    /// A stranger cannot book closer to now than this.
    pub min_notice_hours: u32,
    /// Nor further ahead than this.
    pub horizon_days: u32,
    /// Meetings per local day before the day stops being offered at all.
    pub per_day_cap: Option<u32>,
    /// The start-time grid (Calendly's "increment"), independent of duration.
    pub increment_minutes: u32,
    pub windows: Vec<WeeklyWindow>,
    pub overrides: Vec<DateOverride>,
    /// How a group poll seeded from this policy runs its lifecycle.
    pub poll: PollPolicy,
}

/// What books a group poll by itself, once answers are in.
///
/// The 2026-08-07 ruling was *auto with guardrails*: a booking happens with
/// nobody in the loop only when every participant answered and exactly one
/// slot is a plain yes for all. That stays the default. The other two are
/// the owner's to choose, and neither books over a silent participant — a
/// meeting someone never agreed to is the failure the poll exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoBook {
    /// Everyone answered and one slot is unanimous plain-yes.
    #[default]
    Unanimous,
    /// Everyone answered and the best-ranked slot is feasible — an if-needed
    /// is accepted, a tie takes the earliest.
    Feasible,
    /// Never: every close is the owner's pick.
    Manual,
}

impl AutoBook {
    pub fn as_str(&self) -> &'static str {
        match self {
            AutoBook::Unanimous => "unanimous",
            AutoBook::Feasible => "feasible",
            AutoBook::Manual => "manual",
        }
    }
}

/// The numbers a poll's lifecycle runs on, all owner-set, all defaulted.
///
/// Copied into the poll's record at creation, so a policy edit never changes
/// a poll already in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollPolicy {
    pub auto_book: AutoBook,
    /// Answers close this many days after the invitations go out …
    pub deadline_days: u32,
    /// … at this hour, in the policy's zone.
    pub deadline_hour: u32,
    /// One nudge to whoever is silent, this long before the deadline. Zero
    /// disables it.
    pub nudge_hours_before: u32,
    /// No nudge when the deadline was closer than this at send — a reminder
    /// eleven hours after an invitation is nagging.
    pub nudge_min_lead_hours: u32,
}

impl Default for PollPolicy {
    fn default() -> Self {
        PollPolicy {
            auto_book: AutoBook::Unanimous,
            deadline_days: 3,
            deadline_hour: 17,
            nudge_hours_before: 24,
            nudge_min_lead_hours: 36,
        }
    }
}

/// The `[availability]` section exactly as TOML states it, before meaning is
/// checked. Public within the crate so a `booking` manifest embeds it as a
/// section and inherits this parse instead of re-deciding it. Strict on
/// purpose (`deny_unknown_fields`): a typo'd key in an availability policy
/// silently changes when a stranger can book the user's week.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPolicy {
    pub timezone: String,
    pub durations: Vec<u32>,
    #[serde(default)]
    pub buffer_minutes: u32,
    #[serde(default = "default_notice")]
    pub min_notice_hours: u32,
    #[serde(default = "default_horizon")]
    pub horizon_days: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_day_cap: Option<u32>,
    #[serde(default = "default_increment")]
    pub increment_minutes: u32,
    pub windows: Vec<RawWindow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<RawOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll: Option<RawPollPolicy>,
}

/// The `[poll]` table as written: every key optional, so an owner states
/// only what they are changing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPollPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_book: Option<AutoBook>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_hour: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudge_hours_before: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudge_min_lead_hours: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawWindow {
    pub day: String,
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOverride {
    pub date: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<[String; 2]>,
}

fn default_notice() -> u32 {
    24
}
fn default_horizon() -> u32 {
    60
}
fn default_increment() -> u32 {
    30
}

impl Policy {
    /// Parse the `[availability]` vocabulary from a standalone TOML document.
    pub fn from_toml(text: &str) -> crate::Result<Policy> {
        Self::from_raw(&toml::from_str(text)?)
    }

    /// Check a deserialized section and give it meaning. Every refusal names
    /// its field — this is the validation both a `type check` at home and
    /// the slot pipeline run, and "refused with the reason" is the contract.
    pub fn from_raw(raw: &RawPolicy) -> crate::Result<Policy> {
        use crate::ManifestError;
        fn ensure(ok: bool, message: impl FnOnce() -> String) -> crate::Result<()> {
            if ok {
                Ok(())
            } else {
                Err(ManifestError::invalid(message()))
            }
        }

        let timezone: Tz = raw.timezone.parse().map_err(|_| {
            ManifestError::invalid(format!("`{}` is not an IANA timezone", raw.timezone))
        })?;
        ensure(!raw.durations.is_empty(), || {
            "durations must name at least one".into()
        })?;
        ensure(raw.durations.iter().all(|d| (5..=480).contains(d)), || {
            "each duration must be 5–480 minutes".into()
        })?;
        ensure(!raw.windows.is_empty(), || {
            "windows must name at least one".into()
        })?;
        ensure(raw.horizon_days >= 1, || {
            "horizon_days must be at least 1".into()
        })?;
        ensure(raw.increment_minutes >= 5, || {
            "increment_minutes must be at least 5".into()
        })?;
        let defaults = PollPolicy::default();
        let raw_poll = raw.poll.clone().unwrap_or_default();
        let poll = PollPolicy {
            auto_book: raw_poll.auto_book.unwrap_or(defaults.auto_book),
            deadline_days: raw_poll.deadline_days.unwrap_or(defaults.deadline_days),
            deadline_hour: raw_poll.deadline_hour.unwrap_or(defaults.deadline_hour),
            nudge_hours_before: raw_poll
                .nudge_hours_before
                .unwrap_or(defaults.nudge_hours_before),
            nudge_min_lead_hours: raw_poll
                .nudge_min_lead_hours
                .unwrap_or(defaults.nudge_min_lead_hours),
        };
        ensure(poll.deadline_days >= 1, || {
            "poll.deadline_days must be at least 1".into()
        })?;
        ensure(poll.deadline_hour <= 23, || {
            "poll.deadline_hour must be 0–23".into()
        })?;

        let time = |raw: &str| -> crate::Result<NaiveTime> {
            NaiveTime::parse_from_str(raw, "%H:%M")
                .map_err(|_| ManifestError::invalid(format!("`{raw}` is not an HH:MM time")))
        };
        let pair = |start: &str, end: &str| -> crate::Result<(NaiveTime, NaiveTime)> {
            let (start, end) = (time(start)?, time(end)?);
            ensure(start < end, || {
                format!("window `{start}`–`{end}` ends before it starts")
            })?;
            Ok((start, end))
        };

        let mut windows = Vec::new();
        for w in &raw.windows {
            let day: Weekday = w
                .day
                .parse()
                .map_err(|_| ManifestError::invalid(format!("`{}` is not a weekday", w.day)))?;
            let (start, end) = pair(&w.start, &w.end)?;
            windows.push(WeeklyWindow { day, start, end });
        }
        let mut overrides = Vec::new();
        for o in &raw.overrides {
            let date: NaiveDate = o.date.parse().map_err(|_| {
                ManifestError::invalid(format!("`{}` is not a YYYY-MM-DD date", o.date))
            })?;
            let mut day_windows = Vec::new();
            for [start, end] in &o.windows {
                day_windows.push(pair(start, end)?);
            }
            overrides.push(DateOverride {
                date,
                windows: day_windows,
            });
        }

        Ok(Policy {
            timezone,
            durations: raw.durations.clone(),
            buffer_minutes: raw.buffer_minutes,
            min_notice_hours: raw.min_notice_hours,
            horizon_days: raw.horizon_days,
            per_day_cap: raw.per_day_cap,
            increment_minutes: raw.increment_minutes,
            windows,
            overrides,
            poll,
        })
    }
}

/// The engine. Holds and bookings both subtract exactly as busy time does —
/// buffered, because a hold is a prospective meeting — and bookings
/// additionally count against the per-day cap. Output is sorted by start
/// then duration, deduplicated across overlapping windows.
pub fn availability(
    policy: &Policy,
    busy: &[Interval],
    holds: &[Interval],
    bookings: &[Interval],
    now: DateTime<Utc>,
) -> Vec<Slot> {
    let earliest = now + Duration::hours(i64::from(policy.min_notice_hours));
    let latest = now + Duration::days(i64::from(policy.horizon_days));
    let buffer = Duration::minutes(i64::from(policy.buffer_minutes));
    let blocked = merge(
        busy.iter()
            .chain(holds)
            .chain(bookings)
            .map(|iv| Interval {
                start: iv.start - buffer,
                end: iv.end + buffer,
            })
            .collect(),
    );

    let tz = policy.timezone;
    let mut slots = Vec::new();
    let mut date = earliest.with_timezone(&tz).date_naive();
    let last_day = latest.with_timezone(&tz).date_naive();
    while date <= last_day {
        if day_is_capped(policy, bookings, date) {
            date += Duration::days(1);
            continue;
        }
        for (window_start, window_end) in windows_for(policy, date) {
            let (Some(w_start), Some(w_end)) = (
                local_instant(tz, date, window_start),
                local_instant(tz, date, window_end),
            ) else {
                continue;
            };
            if w_end <= w_start {
                continue;
            }
            let mut start = w_start;
            while start < w_end {
                for &duration in &policy.durations {
                    let end = start + Duration::minutes(i64::from(duration));
                    let fits = end <= w_end && start >= earliest && end <= latest;
                    if fits && !overlaps(&blocked, start, end) {
                        slots.push(Slot {
                            start,
                            end,
                            duration_minutes: duration,
                        });
                    }
                }
                start += Duration::minutes(i64::from(policy.increment_minutes.max(1)));
            }
        }
        date += Duration::days(1);
    }

    slots.sort_by_key(|s| (s.start, s.duration_minutes));
    slots.dedup();
    slots
}

/// The day's windows: an override replaces the weekly pattern wholesale.
fn windows_for(policy: &Policy, date: NaiveDate) -> Vec<(NaiveTime, NaiveTime)> {
    if let Some(o) = policy.overrides.iter().find(|o| o.date == date) {
        return o.windows.clone();
    }
    policy
        .windows
        .iter()
        .filter(|w| w.day == date.weekday())
        .map(|w| (w.start, w.end))
        .collect()
}

/// A local wall time as a UTC instant. Inside the spring-forward gap the
/// first instant that exists is taken (an hour later on the wall); inside
/// the repeated autumn hour, the first occurrence.
fn local_instant(tz: Tz, date: NaiveDate, time: NaiveTime) -> Option<DateTime<Utc>> {
    let naive = date.and_time(time);
    tz.from_local_datetime(&naive)
        .earliest()
        .or_else(|| {
            tz.from_local_datetime(&(naive + Duration::hours(1)))
                .earliest()
        })
        .map(|t| t.with_timezone(&Utc))
}

fn day_is_capped(policy: &Policy, bookings: &[Interval], date: NaiveDate) -> bool {
    let Some(cap) = policy.per_day_cap else {
        return false;
    };
    let count = bookings
        .iter()
        .filter(|b| b.start.with_timezone(&policy.timezone).date_naive() == date)
        .count();
    count as u32 >= cap
}

fn overlaps(blocked: &[Interval], start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    blocked.iter().any(|b| b.start < end && start < b.end)
}

/// Sort and coalesce; degenerate intervals are dropped.
fn merge(intervals: Vec<Interval>) -> Vec<Interval> {
    let mut intervals: Vec<Interval> = intervals.into_iter().filter(|i| i.start < i.end).collect();
    intervals.sort_by_key(|i| (i.start, i.end));
    let mut merged: Vec<Interval> = Vec::with_capacity(intervals.len());
    for next in intervals {
        match merged.last_mut() {
            Some(last) if next.start <= last.end => last.end = last.end.max(next.end),
            _ => merged.push(next),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn hm(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").unwrap()
    }

    fn iv(start: &str, end: &str) -> Interval {
        Interval {
            start: t(start),
            end: t(end),
        }
    }

    /// Tuesday+Thursday 13:00–17:00 Eastern, 30/60 min, 30-min grid.
    fn policy() -> Policy {
        Policy {
            timezone: "America/New_York".parse().unwrap(),
            durations: vec![30, 60],
            buffer_minutes: 0,
            min_notice_hours: 24,
            horizon_days: 14,
            per_day_cap: None,
            increment_minutes: 30,
            windows: vec![
                WeeklyWindow {
                    day: Weekday::Tue,
                    start: hm("13:00"),
                    end: hm("17:00"),
                },
                WeeklyWindow {
                    day: Weekday::Thu,
                    start: hm("13:00"),
                    end: hm("17:00"),
                },
            ],
            poll: PollPolicy::default(),
            overrides: vec![],
        }
    }

    // 2026-08-03 is a Monday; the 4th and 6th are Tue/Thu.
    const NOW: &str = "2026-08-03T12:00:00Z";

    #[test]
    fn slots_fill_windows_on_the_grid_in_utc() {
        let slots = availability(&policy(), &[], &[], &[], t(NOW));
        assert!(!slots.is_empty());
        // 13:00 Eastern in August is 17:00Z.
        let first = slots.first().unwrap();
        assert_eq!(first.start, t("2026-08-04T17:00:00Z"));
        assert_eq!(first.duration_minutes, 30);
        // Both durations offered at the same start.
        assert_eq!(slots[1].start, first.start);
        assert_eq!(slots[1].duration_minutes, 60);
        // A 60-minute slot never overflows the window; the last grid step
        // (16:30 local) offers only the 30.
        let window_end = t("2026-08-04T21:00:00Z");
        assert!(slots
            .iter()
            .all(|s| s.end <= window_end || s.start >= t("2026-08-06T00:00:00Z")));
        assert!(slots
            .iter()
            .any(|s| s.start == t("2026-08-04T20:30:00Z") && s.duration_minutes == 30));
        assert!(!slots
            .iter()
            .any(|s| s.start == t("2026-08-04T20:30:00Z") && s.duration_minutes == 60));
    }

    #[test]
    fn min_notice_and_horizon_bound_both_ends() {
        let mut p = policy();
        p.min_notice_hours = 48; // now+48h = Wed 12:00Z: Tuesday is gone.
        let slots = availability(&p, &[], &[], &[], t(NOW));
        assert!(slots.iter().all(|s| s.start >= t("2026-08-05T12:00:00Z")));
        assert!(
            slots
                .iter()
                .any(|s| s.start.date_naive().to_string() == "2026-08-06"),
            "Thursday survives the notice window"
        );
        // Horizon: nothing ends after now + 14d.
        assert!(slots.iter().all(|s| s.end <= t("2026-08-17T12:00:00Z")));
    }

    #[test]
    fn busy_blocks_and_buffers_widen_the_block() {
        let busy = [iv("2026-08-04T18:00:00Z", "2026-08-04T19:00:00Z")]; // 14:00–15:00 local
        let no_buffer = availability(&policy(), &busy, &[], &[], t(NOW));
        // 13:30+60 overlaps busy and is gone; 13:30+30 touches and survives.
        assert!(!no_buffer
            .iter()
            .any(|s| s.start == t("2026-08-04T17:30:00Z") && s.duration_minutes == 60));
        assert!(no_buffer
            .iter()
            .any(|s| s.start == t("2026-08-04T17:30:00Z") && s.duration_minutes == 30));
        assert!(no_buffer
            .iter()
            .any(|s| s.start == t("2026-08-04T19:00:00Z")));

        let mut p = policy();
        p.buffer_minutes = 15;
        let buffered = availability(&p, &busy, &[], &[], t(NOW));
        // With 15 min of clear air demanded, back-to-back is gone too.
        assert!(!buffered
            .iter()
            .any(|s| s.start == t("2026-08-04T17:30:00Z") && s.duration_minutes == 30));
        assert!(!buffered
            .iter()
            .any(|s| s.start == t("2026-08-04T19:00:00Z")));
        assert!(buffered
            .iter()
            .any(|s| s.start == t("2026-08-04T19:30:00Z")));
    }

    #[test]
    fn holds_subtract_exactly_like_busy() {
        let holds = [iv("2026-08-04T17:00:00Z", "2026-08-04T21:00:00Z")];
        let slots = availability(&policy(), &[], &holds, &[], t(NOW));
        assert!(
            slots.iter().all(|s| s.start >= t("2026-08-05T00:00:00Z")),
            "a fully-held Tuesday offers nothing"
        );
    }

    #[test]
    fn the_per_day_cap_closes_a_day_that_reached_it() {
        let mut p = policy();
        p.per_day_cap = Some(2);
        let bookings = [
            iv("2026-08-04T17:00:00Z", "2026-08-04T17:30:00Z"),
            iv("2026-08-04T19:00:00Z", "2026-08-04T19:30:00Z"),
        ];
        let slots = availability(&p, &[], &[], &bookings, t(NOW));
        assert!(
            !slots
                .iter()
                .any(|s| s.start.date_naive().to_string() == "2026-08-04"),
            "two bookings meet the cap; Tuesday closes"
        );
        assert!(
            slots
                .iter()
                .any(|s| s.start.date_naive().to_string() == "2026-08-06"),
            "Thursday is untouched"
        );
        // Below the cap, the day still offers its remaining air.
        p.per_day_cap = Some(3);
        let slots = availability(&p, &[], &[], &bookings, t(NOW));
        assert!(slots.iter().any(|s| s.start == t("2026-08-04T18:00:00Z")));
    }

    #[test]
    fn overrides_close_or_reshape_a_date() {
        let mut p = policy();
        p.overrides = vec![
            DateOverride {
                date: "2026-08-04".parse().unwrap(),
                windows: vec![],
            },
            DateOverride {
                date: "2026-08-05".parse().unwrap(), // a Wednesday, normally closed
                windows: vec![(hm("09:00"), hm("10:00"))],
            },
        ];
        let slots = availability(&p, &[], &[], &[], t(NOW));
        assert!(!slots
            .iter()
            .any(|s| s.start.date_naive().to_string() == "2026-08-04"));
        assert!(slots.iter().any(|s| s.start == t("2026-08-05T13:00:00Z")));
    }

    /// 2026-03-08: US spring-forward; 02:00–03:00 does not exist. A window
    /// boundary in the gap takes the first instant that exists — late, not
    /// lost, and never a panic.
    #[test]
    fn a_window_boundary_in_the_dst_gap_shifts_rather_than_vanishing() {
        let mut p = policy();
        p.min_notice_hours = 0;
        p.horizon_days = 2;
        p.windows = vec![WeeklyWindow {
            day: Weekday::Sun,
            start: hm("01:30"),
            end: hm("02:30"), // inside the gap
        }];
        let slots = availability(&p, &[], &[], &[], t("2026-03-07T12:00:00Z"));
        // 01:30 EST = 06:30Z exists; the 02:30 end shifts to 03:30 EDT =
        // 07:30Z, so the window is one real hour and offers the 30s.
        assert!(slots.iter().any(|s| s.start == t("2026-03-08T06:30:00Z")));
        assert!(slots.iter().all(|s| s.end <= t("2026-03-08T07:30:00Z")));
    }

    /// 2026-11-01: the 01:00–02:00 hour happens twice. First occurrence
    /// wins, and no slot instant is offered twice.
    #[test]
    fn the_repeated_autumn_hour_fires_once() {
        let mut p = policy();
        p.min_notice_hours = 0;
        p.horizon_days = 2;
        p.windows = vec![WeeklyWindow {
            day: Weekday::Sun,
            start: hm("01:00"),
            end: hm("02:00"),
        }];
        let slots = availability(&p, &[], &[], &[], t("2026-10-31T12:00:00Z"));
        // 01:00 EDT = 05:00Z (first occurrence; the second would be 06:00Z).
        assert!(slots.iter().any(|s| s.start == t("2026-11-01T05:00:00Z")));
        let mut starts: Vec<(DateTime<Utc>, u32)> = slots
            .iter()
            .map(|s| (s.start, s.duration_minutes))
            .collect();
        let before = starts.len();
        starts.dedup();
        assert_eq!(before, starts.len(), "no slot may be offered twice");
    }

    #[test]
    fn overlapping_windows_do_not_duplicate_slots() {
        let mut p = policy();
        p.windows.push(WeeklyWindow {
            day: Weekday::Tue,
            start: hm("14:00"),
            end: hm("18:00"),
        });
        let slots = availability(&p, &[], &[], &[], t(NOW));
        let mut keyed: Vec<(DateTime<Utc>, u32)> = slots
            .iter()
            .map(|s| (s.start, s.duration_minutes))
            .collect();
        let before = keyed.len();
        keyed.dedup();
        assert_eq!(before, keyed.len());
    }

    const POLICY_TOML: &str = r#"
        timezone = "America/New_York"
        durations = [30, 60]
        buffer_minutes = 10
        min_notice_hours = 24
        horizon_days = 60
        per_day_cap = 3

        [[windows]]
        day = "tue"
        start = "13:00"
        end = "17:00"

        [[overrides]]
        date = "2026-11-26"

        [[overrides]]
        date = "2026-08-20"
        windows = [["09:00", "10:30"]]
    "#;

    #[test]
    fn a_policy_parses_from_the_manifest_vocabulary() {
        let p = Policy::from_toml(POLICY_TOML).unwrap();
        assert_eq!(p.durations, vec![30, 60]);
        assert_eq!(p.increment_minutes, 30, "the default grid");
        assert_eq!(p.windows.len(), 1);
        assert_eq!(p.windows[0].day, Weekday::Tue);
        assert!(
            p.overrides[0].windows.is_empty(),
            "no windows closes the day"
        );
        assert_eq!(p.overrides[1].windows[0].0, hm("09:00"));
        assert_eq!(p.per_day_cap, Some(3));
    }

    /// A typo'd policy silently changes when a stranger can book a week, so
    /// every malformed shape is a refusal, never a default.
    #[test]
    fn malformed_policies_are_refused_with_the_field_named() {
        fn doc(header: &str, window: &str) -> String {
            format!("{header}\n[[windows]]\n{window}\n")
        }
        let good_window = "day = \"tue\"\nstart = \"13:00\"\nend = \"17:00\"";
        let cases: &[(String, &str)] = &[
            (
                doc("timezone = \"Eastern\"\ndurations = [30]", good_window),
                "not an IANA timezone",
            ),
            (
                // The typo the strictness exists for.
                doc(
                    "timezone = \"UTC\"\ndurations = [30]\nbufer_minutes = 10",
                    good_window,
                ),
                "bufer_minutes",
            ),
            (
                doc(
                    "timezone = \"UTC\"\ndurations = [30]",
                    "day = \"tue\"\nstart = \"17:00\"\nend = \"13:00\"",
                ),
                "ends before",
            ),
            (
                doc(
                    "timezone = \"UTC\"\ndurations = [30]",
                    "day = \"tue\"\nstart = \"1pm\"\nend = \"17:00\"",
                ),
                "HH:MM",
            ),
            (
                doc(
                    "timezone = \"UTC\"\ndurations = [30]",
                    "day = \"someday\"\nstart = \"13:00\"\nend = \"17:00\"",
                ),
                "weekday",
            ),
            (
                doc("timezone = \"UTC\"\ndurations = []", good_window),
                "at least one",
            ),
        ];
        for (toml, expect) in cases {
            let err = Policy::from_toml(toml).unwrap_err();
            assert!(
                format!("{err:#}").contains(expect),
                "should fail mentioning `{expect}`, got: {err:#}\n{toml}"
            );
        }
    }

    /// `[poll]` is optional, every key in it is optional, and a key it does
    /// not know is an error — the same arrangement as the rest of the policy,
    /// because `auto_bok = "manual"` silently meaning "unanimous" is a
    /// meeting booked over someone's objection.
    #[test]
    fn the_poll_table_defaults_key_by_key_and_refuses_typos() {
        let base = "timezone = \"UTC\"\ndurations = [30]\n[[windows]]\nday = \"mon\"\nstart = \"09:00\"\nend = \"10:00\"\n";
        let absent = Policy::from_toml(base).unwrap();
        assert_eq!(absent.poll, PollPolicy::default());
        assert_eq!(absent.poll.auto_book, AutoBook::Unanimous);

        let partial = Policy::from_toml(&format!(
            "{base}[poll]\nauto_book = \"manual\"\nnudge_hours_before = 0\n"
        ))
        .unwrap();
        assert_eq!(partial.poll.auto_book, AutoBook::Manual);
        assert_eq!(partial.poll.nudge_hours_before, 0);
        assert_eq!(
            partial.poll.deadline_days, 3,
            "untouched keys keep the default"
        );
        assert_eq!(partial.poll.deadline_hour, 17);

        for (bad, expect) in [
            ("[poll]\nauto_bok = \"manual\"\n", "auto_bok"),
            ("[poll]\nauto_book = \"always\"\n", "always"),
            ("[poll]\ndeadline_hour = 24\n", "0–23"),
            ("[poll]\ndeadline_days = 0\n", "at least 1"),
        ] {
            let err = Policy::from_toml(&format!("{base}{bad}")).unwrap_err();
            assert!(
                format!("{err:#}").contains(expect),
                "should fail mentioning `{expect}`, got: {err:#}"
            );
        }
    }

    /// The JSON contract with `mecha-mail freebusy --json`: its `busy` rows
    /// parse as `Interval`s, and a `Slot` serialises with UTC stamps.
    #[test]
    fn the_data_contract_round_trips() {
        let parsed: Vec<Interval> = serde_json::from_str(
            r#"[{"start": "2026-08-10T13:00:00Z", "end": "2026-08-10T14:00:00Z"}]"#,
        )
        .unwrap();
        assert_eq!(parsed[0].end - parsed[0].start, Duration::hours(1));
        let slot = Slot {
            start: t("2026-08-11T13:00:00Z"),
            end: t("2026-08-11T13:30:00Z"),
            duration_minutes: 30,
        };
        let json = serde_json::to_value(slot).unwrap();
        assert_eq!(json["start"], "2026-08-11T13:00:00Z");
        assert_eq!(json["duration_minutes"], 30);
    }
}
