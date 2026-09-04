//! The booking page: a week of columns, rendered from slots the box was
//! pushed. The contract crate renders it for the same reason it renders the
//! form — the document is the contract's business, the URL scheme is the
//! server's — and everything works with JavaScript off: slots are radio
//! inputs, week paging is links, times are server-rendered in the host's
//! zone and labelled as such. `booking.js` is enhancement only: it re-renders
//! times in the visitor's zone, adds the duration filter, and nothing else a
//! submission depends on.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;

use crate::availability::Slot;
use crate::form::{render_acknowledgment, FORM_JS};
use crate::request::RequestType;
use crate::{escape_text, FormOptions};

/// What the server supplies to render one view of the page.
pub struct BookingOptions {
    /// Where the form POSTs.
    pub action: String,
    /// URL prefix (ending in `/`) where the page's assets live.
    pub assets: String,
    pub theme: crate::Theme,
    /// "Now", supplied by the caller — this crate reads no clock.
    pub now: DateTime<Utc>,
    /// The Monday to show. `None` shows the first week holding a slot.
    pub week: Option<NaiveDate>,
    /// The meeting length to show, from `?mins=`. `None` — or a length no
    /// future slot offers — falls back to the shortest on offer, so each
    /// start time renders exactly once and the page never shows "3:30 pm"
    /// twice. Switching lengths is a link, like week paging: it works with
    /// JavaScript off, and the server is the one filtering.
    pub duration: Option<u32>,
    /// Values and errors from a rejected submission being shown back. May
    /// carry the page's own `_slot` key, which re-checks the picked time —
    /// a visitor who mistyped an email must not lose the slot they chose.
    pub values: serde_json::Map<String, serde_json::Value>,
    pub errors: Vec<crate::ValidationError>,
    /// A line about freshness, when the server judges the cache old enough
    /// to say so. Rendered verbatim (escaped), so wording stays server-side.
    pub stale_notice: Option<String>,
    /// Where `booking.js` may poll for live availability, relative to the
    /// page (usually `slots.json` under the assets prefix). `None` renders a
    /// static page — the gallery's golden files must not fetch anything.
    pub live_slots_url: Option<String>,
}

impl Default for BookingOptions {
    fn default() -> Self {
        BookingOptions {
            action: String::new(),
            assets: String::new(),
            theme: crate::theme::NOCTURNE,
            now: DateTime::<Utc>::UNIX_EPOCH,
            week: None,
            duration: None,
            values: serde_json::Map::new(),
            errors: Vec::new(),
            stale_notice: None,
            live_slots_url: None,
        }
    }
}

/// A rendered booking page and its assets. Three files rather than two:
/// `form.js` still drives the detail fields' conditions, `booking.js` is the
/// page's own enhancement, and one stylesheet carries theme, form structure
/// and the week grid so nothing can half-apply.
pub struct BookingPage {
    pub html: String,
    pub style: String,
}

impl BookingPage {
    pub fn assets(&self) -> [(&'static str, &str); 6] {
        [
            ("booking.css", &self.style),
            ("form.js", FORM_JS),
            ("booking.js", BOOKING_JS),
            ("poll.js", POLL_JS),
            ("survey.js", crate::poll_render::SURVEY_JS),
            ("screen.js", crate::poll_render::SCREEN_JS),
        ]
    }
}

/// Everything `booking.css` is, for one theme.
///
/// **One builder, because there used to be several and they disagreed.** Every
/// page in this family — booking, poll, survey, projector — links a stylesheet
/// called `booking.css`, so whichever page writes it decides what all the
/// others get. The booking builders omitted `SURVEY_STRUCTURE`; the survey
/// builders included it; the gallery wrote the booking one first and marked the
/// job done. The result was a documentation gallery whose survey page had *no*
/// survey CSS at all: list markers where `list-style:none` was meant to be, and
/// rank buttons falling back to the full-size accent pill that `button` is. The
/// live server was fine, because it happened to build its assets down the other
/// path — which is the part that makes this the dangerous kind of bug, since
/// the surface people learn the component from was the broken one.
///
/// A shared asset name needs a single definition. `assert_the_stylesheet_has_one_definition`
/// holds the two paths together.
pub fn page_style(theme: &crate::Theme) -> String {
    format!(
        "{}{}{}{}",
        theme.css(),
        crate::form::FORM_STRUCTURE,
        BOOKING_STRUCTURE,
        crate::poll_render::SURVEY_STRUCTURE
    )
}

/// The page's assets for a theme, independent of any render — what a server
/// answers asset requests from, where [`BookingPage::assets`] serves whoever
/// is writing a rendered bundle to disk.
pub fn booking_assets(theme: &crate::Theme) -> [(&'static str, String); 6] {
    [
        ("booking.css", page_style(theme)),
        ("form.js", FORM_JS.to_string()),
        ("booking.js", BOOKING_JS.to_string()),
        ("poll.js", POLL_JS.to_string()),
        ("survey.js", crate::poll_render::SURVEY_JS.to_string()),
        ("screen.js", crate::poll_render::SCREEN_JS.to_string()),
    ]
}

/// One candidate in a poll page, with everything the cell shows.
pub struct PollCandidate {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub duration_minutes: u32,
    /// This participant's current answer, if they have one.
    pub mine: Option<PollAnswer>,
    /// How many others have said yes so far — the heat, server-rendered.
    pub yes_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollAnswer {
    Yes,
    IfNeeded,
    No,
}

impl PollAnswer {
    pub fn as_str(&self) -> &'static str {
        match self {
            PollAnswer::Yes => "yes",
            PollAnswer::IfNeeded => "if_needed",
            PollAnswer::No => "no",
        }
    }
    pub fn parse(raw: &str) -> Option<PollAnswer> {
        match raw {
            "yes" => Some(PollAnswer::Yes),
            "if_needed" => Some(PollAnswer::IfNeeded),
            "no" => Some(PollAnswer::No),
            _ => None,
        }
    }
}

/// What the server supplies to render one participant's view.
pub struct PollPageOptions {
    pub title: String,
    pub participant: String,
    pub timezone: Tz,
    pub action: String,
    pub assets: String,
    pub theme: crate::Theme,
    pub deadline_local: Option<String>,
    pub responded: usize,
    pub total: usize,
    /// Closed polls render read-only: the answers stand, the form is gone.
    pub open: bool,
    /// A one-line notice ("Saved — you can change this until…").
    pub notice: Option<String>,
}

/// The participant's page: the booking page's weekly frame over the seeded
/// candidates, each a tri-state answer. Works with JavaScript off — the
/// three states are radios; `booking.js`'s zone re-render applies to the
/// same `.when[data-utc]` labels; tap-to-cycle arrives with the polish
/// pass. Never a blank 7×24 grid: what is rendered is only what the
/// organizer can actually do, which is the entire point of the seeding.
pub fn poll_page(candidates: &[PollCandidate], options: &PollPageOptions) -> BookingPage {
    let tz = options.timezone;
    let mut body = String::new();
    body.push_str(&format!("<h1>{}</h1>\n", escape_text(&options.title)));
    body.push_str(&format!(
        "<p class=\"intro\">Hi {} — mark the times you could do. \
         {} of {} have answered so far.</p>\n",
        escape_text(&options.participant),
        options.responded,
        options.total
    ));
    if let Some(notice) = &options.notice {
        body.push_str(&format!(
            "<p class=\"stale\" role=\"note\">{}</p>\n",
            escape_text(notice)
        ));
    }
    if let Some(deadline) = &options.deadline_local {
        body.push_str(&format!(
            "<p class=\"zone\">Answers close {}.</p>\n",
            escape_text(deadline)
        ));
    }
    body.push_str(&format!(
        "<p class=\"zone\" id=\"zone-note\">Times are shown in {}.</p>\n",
        escape_text(&tz.to_string())
    ));

    if !options.open {
        body.push_str(
            "<p class=\"empty\">This poll is closed; answers can no longer change.</p>\n",
        );
    }
    body.push_str(&format!(
        "<form method=\"post\" action=\"{}\">\n",
        escape_text(&options.action)
    ));
    body.push_str("<div class=\"week poll\" role=\"group\" aria-label=\"Candidate times\">\n");

    let mut sorted: Vec<&PollCandidate> = candidates.iter().collect();
    sorted.sort_by_key(|c| (c.start, c.duration_minutes));
    let mut day = None;
    for candidate in &sorted {
        let local = candidate.start.with_timezone(&tz);
        let this_day = local.date_naive();
        if day != Some(this_day) {
            if day.is_some() {
                body.push_str("</section>\n");
            }
            body.push_str(&format!(
                "<section class=\"day\"><h2><span class=\"dow\">{}</span> \
                 <span class=\"date\">{}</span></h2>\n",
                this_day.format("%a"),
                this_day.format("%b %-d")
            ));
            day = Some(this_day);
        }
        let key = format!(
            "a_{}|{}",
            candidate
                .start
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            candidate.duration_minutes
        );
        let label = local.format("%-I:%M %p").to_string().to_lowercase();
        // The heat is discrete steps, never a continuous ramp: six shades is
        // what stays tellable-apart, and the count rides in text beside the
        // colour so the information survives colour-blindness and screen
        // readers. A class rather than an inline style, because the gate's
        // CSP (`style-src 'self'`) forbids style attributes.
        let heat_bucket = if options.total == 0 || candidate.yes_count == 0 {
            0
        } else {
            (candidate.yes_count * 5).div_ceil(options.total).min(5)
        };
        let heat = if candidate.yes_count > 0 {
            format!(
                " <span class=\"count\">{} of {} yes</span>",
                candidate.yes_count, options.total
            )
        } else {
            String::new()
        };
        body.push_str(&format!(
            "<fieldset class=\"slot answer heat-{heat_bucket}\"><legend><span class=\"when\" \
             data-utc=\"{utc}\">{label}</span> <span class=\"len\">· {mins} min</span>{heat}</legend>\n",
            utc = candidate
                .start
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            mins = candidate.duration_minutes,
        ));
        for answer in [PollAnswer::Yes, PollAnswer::IfNeeded, PollAnswer::No] {
            // Unanswered defaults to "no": when2meet's rule — you paint the
            // times you CAN do, and silence is unavailability.
            let checked = match candidate.mine {
                Some(mine) => mine == answer,
                None => answer == PollAnswer::No,
            };
            let word = match answer {
                PollAnswer::Yes => "yes",
                PollAnswer::IfNeeded => "if needed",
                PollAnswer::No => "no",
            };
            body.push_str(&format!(
                "<label class=\"tri\"><input type=\"radio\" name=\"{key}\" \
                 value=\"{value}\"{checked}{disabled}> {word}</label>\n",
                value = answer.as_str(),
                checked = if checked { " checked" } else { "" },
                disabled = if options.open { "" } else { " disabled" },
            ));
        }
        body.push_str("</fieldset>\n");
    }
    if day.is_some() {
        body.push_str("</section>\n");
    }
    body.push_str("</div>\n");
    if options.open {
        // Where poll.js reports autosaves. Hidden until it speaks; with
        // JavaScript off the button below is the whole story.
        body.push_str(
            "<p class=\"savestate\" id=\"savestate\" aria-live=\"polite\" hidden></p>\n\
             <button type=\"submit\">Save my answers</button>\n",
        );
    }
    body.push_str("</form>\n");
    body.push_str(&format!(
        "<script src=\"{0}booking.js\" defer></script>\n\
         <script src=\"{0}poll.js\" defer></script>\n",
        escape_text(&options.assets)
    ));

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n\
         {}\n\
         <link rel=\"stylesheet\" href=\"{}booking.css\">\n\
         </head>\n<body>\n{}<main class=\"booking\">\n{}</main>\n</body>\n</html>\n",
        escape_text(&options.title),
        crate::brand::FAVICON_LINK,
        escape_text(&options.assets),
        crate::form::site_header(),
        body
    );
    BookingPage {
        html,
        style: page_style(&options.theme),
    }
}

/// One candidate's standing once answers are in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedCandidate {
    pub start: DateTime<Utc>,
    pub duration_minutes: u32,
    pub yes: usize,
    pub if_needed: usize,
    pub no: usize,
    /// Everyone who answered could attend (yes or if-needed).
    pub feasible: bool,
    /// Every participant answered plain yes — nobody pays anything.
    pub unanimous: bool,
}

/// Rank a closed (or closing) poll. Pure, so the guardrail is testable:
/// feasibility first, then the most yeses, then the fewest if-neededs (the
/// cost — CalBench's point is that "found a time" and "found a time nobody
/// pays for" are different numbers), then the earliest start as the tie
/// among equals. A participant who never answered counts as no everywhere,
/// which only understates — the guardrail below demands full attendance
/// anyway.
pub fn rank_poll(
    candidates: &[(DateTime<Utc>, u32)],
    answers: &[std::collections::BTreeMap<String, PollAnswer>],
    total_participants: usize,
) -> Vec<RankedCandidate> {
    let mut ranked: Vec<RankedCandidate> = candidates
        .iter()
        .map(|(start, duration)| {
            let key = format!(
                "{}|{duration}",
                start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            );
            let mut yes = 0;
            let mut if_needed = 0;
            for participant in answers {
                match participant.get(&key) {
                    Some(PollAnswer::Yes) => yes += 1,
                    Some(PollAnswer::IfNeeded) => if_needed += 1,
                    _ => {}
                }
            }
            let no = total_participants - yes - if_needed;
            RankedCandidate {
                start: *start,
                duration_minutes: *duration,
                yes,
                if_needed,
                no,
                feasible: yes + if_needed == total_participants,
                unanimous: yes == total_participants,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.feasible
            .cmp(&a.feasible)
            .then(b.yes.cmp(&a.yes))
            .then(a.if_needed.cmp(&b.if_needed))
            .then(a.start.cmp(&b.start))
    });
    ranked
}

/// The auto-book guardrail, exactly as decided: a booking happens by itself
/// only when every participant answered and exactly one candidate is
/// unanimous plain-yes. A tie between two unanimous slots, an if-needed in
/// the best option, or a silent participant all mean judgment — rank and
/// stage, never guess.
pub fn clean_winner(
    ranked: &[RankedCandidate],
    responded: usize,
    total_participants: usize,
) -> Option<&RankedCandidate> {
    if responded != total_participants {
        return None;
    }
    let mut unanimous = ranked.iter().filter(|c| c.unanimous);
    match (unanimous.next(), unanimous.next()) {
        (Some(winner), None) => Some(winner),
        _ => None,
    }
}

/// What the sweep books by itself at close, under the owner's `[poll]`
/// setting. `None` is the owner's pick — the ranking is staged with reasons
/// and a person releases it.
///
/// Every mode refuses a silent participant: the table in
/// MEETING-POLL-UX-DESIGN.md §5 has no row that books over someone who never
/// answered.
pub fn auto_book(
    ranked: &[RankedCandidate],
    responded: usize,
    total_participants: usize,
    mode: crate::availability::AutoBook,
) -> Option<&RankedCandidate> {
    use crate::availability::AutoBook;
    if responded != total_participants {
        return None;
    }
    match mode {
        AutoBook::Manual => None,
        AutoBook::Unanimous => clean_winner(ranked, responded, total_participants),
        // The ranking already puts unanimous before merely feasible, more
        // yeses before fewer, and the earliest among equals — so the best
        // feasible slot is the first one, or there is none.
        AutoBook::Feasible => ranked.first().filter(|c| c.feasible),
    }
}

/// The Monday of the week holding `date`.
pub fn week_of(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

impl RequestType {
    /// Render the weekly booking view. `slots` is whatever the cache holds —
    /// already computed at home, already narrowed by the server; this
    /// function only lays it out. Fails only on a manifest that is not a
    /// booking (the caller routed wrong, and rendering an empty week would
    /// hide that).
    pub fn booking_page(
        &self,
        slots: &[Slot],
        options: &BookingOptions,
    ) -> crate::Result<BookingPage> {
        let policy = self
            .availability_policy()
            .ok_or_else(|| crate::ManifestError::invalid("not a booking manifest"))??;
        let tz = policy.timezone;

        // Only the future is offerable, whatever the cache still holds.
        let mut future: Vec<&Slot> = slots.iter().filter(|s| s.start > options.now).collect();
        future.sort_by_key(|s| (s.start, s.duration_minutes));

        // The lengths on offer, and the one being shown. Everything below —
        // the grid, the week paging, the empty-week message — is about one
        // meeting length at a time, so a start time never renders twice.
        let durations: Vec<u32> = {
            let mut seen = Vec::new();
            for slot in &future {
                if !seen.contains(&slot.duration_minutes) {
                    seen.push(slot.duration_minutes);
                }
            }
            seen.sort_unstable();
            seen
        };
        let chosen = options
            .duration
            .filter(|d| durations.contains(d))
            .or_else(|| durations.first().copied());
        let future: Vec<&Slot> = future
            .into_iter()
            .filter(|s| Some(s.duration_minutes) == chosen)
            .collect();
        // `&mins=` rides every navigation link once there is a choice to
        // keep; with one length there is nothing to preserve.
        let mins_param = match chosen {
            Some(chosen) if durations.len() > 1 => format!("&mins={chosen}"),
            _ => String::new(),
        };

        let today = options.now.with_timezone(&tz).date_naive();
        let this_week = week_of(today);
        let first_slot_week = future.first().map(|s| local_day(s, tz)).map(week_of);
        let week = options
            .week
            .map(week_of)
            .or(first_slot_week)
            .unwrap_or(this_week)
            // Never page into the past, wherever the link came from.
            .max(this_week);
        let week_end = week + Duration::days(7);

        let in_week: Vec<&Slot> = future
            .iter()
            .copied()
            .filter(|s| {
                let day = local_day(s, tz);
                day >= week && day < week_end
            })
            .collect();
        let has_next = future.iter().any(|s| local_day(s, tz) >= week_end);
        let has_prev = week > this_week;

        // The slot a rejected submission had picked, so it survives the trip.
        let picked_slot = options.values.get("_slot").and_then(|v| v.as_str());

        let mut body = String::new();
        body.push_str(&format!("<h1>{}</h1>\n", escape_text(&self.title)));
        if let Some(description) = &self.description {
            body.push_str(&format!(
                "<p class=\"intro\">{}</p>\n",
                escape_text(description)
            ));
        }
        if let Some(notice) = &options.stale_notice {
            body.push_str(&format!(
                "<p class=\"stale\" role=\"note\">{}</p>\n",
                escape_text(notice)
            ));
        }
        if !options.errors.is_empty() {
            // The same label lookup as the plain form's summary: a visitor is
            // told "Your email", never `requester_email`.
            body.push_str(
                "<div class=\"errors\" role=\"alert\"><p>Some answers need attention:</p><ul>\n",
            );
            for error in &options.errors {
                let label = self
                    .field(&error.field)
                    .map(|f| f.label.as_str())
                    .or_else(|| {
                        self.acknowledgments
                            .iter()
                            .find(|a| a.id == error.field)
                            .map(|a| a.label.as_str())
                    })
                    .unwrap_or(error.field.as_str());
                body.push_str(&format!(
                    "<li><a href=\"#{}\">{}</a>: {}</li>\n",
                    escape_text(&error.field),
                    escape_text(label),
                    escape_text(&error.message)
                ));
            }
            body.push_str("</ul></div>\n");
        }

        // The week navigation: links, so paging works with JavaScript off.
        // A GET with a query string, never a POST — nothing is held yet.
        body.push_str("<nav class=\"weeknav\" aria-label=\"Week\">\n");
        if has_prev {
            body.push_str(&format!(
                "<a rel=\"nofollow\" href=\"?week={}{mins_param}\"><span aria-hidden=\"true\">&larr;</span> earlier</a>\n",
                week - Duration::days(7)
            ));
        } else {
            body.push_str("<span></span>\n");
        }
        body.push_str(&format!(
            "<span class=\"range\">{} – {}</span>\n",
            week.format("%b %-d"),
            (week_end - Duration::days(1)).format("%b %-d")
        ));
        if has_next {
            body.push_str(&format!(
                "<a rel=\"nofollow\" href=\"?week={week_end}{mins_param}\">later <span aria-hidden=\"true\">&rarr;</span></a>\n"
            ));
        } else {
            body.push_str("<span></span>\n");
        }
        body.push_str("</nav>\n");

        // The length switch: server-side links, the same shape as week
        // paging, so it books identically with JavaScript off. Rendered only
        // when there is a real choice.
        if durations.len() > 1 {
            body.push_str("<nav class=\"durations\" aria-label=\"Meeting length\">\n");
            for duration in &durations {
                let current = Some(*duration) == chosen;
                body.push_str(&format!(
                    "<a rel=\"nofollow\" href=\"?week={week}&mins={duration}\"{}>{duration} min</a>\n",
                    if current { " aria-current=\"true\"" } else { "" },
                ));
            }
            body.push_str("</nav>\n");
        }

        body.push_str(&format!(
            "<form method=\"post\" action=\"{}\">\n",
            escape_text(&options.action)
        ));

        // The zone every server-rendered time is stated in. booking.js
        // rewrites the times and this line together or not at all.
        body.push_str(&format!(
            "<p class=\"zone\" id=\"zone-note\">Times are shown in {}.</p>\n",
            escape_text(&tz.to_string())
        ));

        // Where booking.js speaks when live availability moves under the
        // page — "that time was just taken". Empty and hidden until it does.
        body.push_str("<p class=\"notice\" id=\"live-note\" aria-live=\"assertive\" hidden></p>\n");

        if in_week.is_empty() {
            let hint = if has_next {
                " Try a later week."
            } else if future.is_empty() {
                " Nothing is currently open — check back soon."
            } else {
                ""
            };
            body.push_str(&format!(
                "<p class=\"empty\">No times this week.{hint}</p>\n"
            ));
        } else {
            body.push_str("<div class=\"week\" role=\"group\" aria-label=\"Available times\">\n");
            let mut day = week;
            while day < week_end {
                let of_day: Vec<&&Slot> =
                    in_week.iter().filter(|s| local_day(s, tz) == day).collect();
                // A column exists only when it offers something: an empty
                // Saturday every week is noise, not information.
                if of_day.is_empty() {
                    day += Duration::days(1);
                    continue;
                }
                body.push_str(&format!(
                    "<section class=\"day\"><h2><span class=\"dow\">{}</span> \
                     <span class=\"date\">{}</span></h2>\n",
                    day.format("%a"),
                    day.format("%b %-d")
                ));
                for slot in of_day {
                    let start_utc = slot
                        .start
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    let local = slot.start.with_timezone(&tz);
                    let label = local.format("%-I:%M %p").to_string().to_lowercase();
                    let value = format!("{start_utc}|{}", slot.duration_minutes);
                    body.push_str(&format!(
                        "<label class=\"slot\"><input type=\"radio\" name=\"_slot\" \
                         value=\"{value}\"{checked} required>\
                         <span class=\"when\" data-utc=\"{start_utc}\">{label}</span>\
                         </label>\n",
                        checked = if picked_slot == Some(value.as_str()) {
                            " checked"
                        } else {
                            ""
                        },
                    ));
                }
                body.push_str("</section>\n");
                day += Duration::days(1);
            }
            body.push_str("</div>\n");
        }

        // From here down the page is about the picked time. With CSS `:has`
        // support, the details stay out of the way until a slot is chosen —
        // the page's one reveal — and the chip above them echoes the choice
        // (written by booking.js; its absence loses nothing). Without `:has`,
        // everything is simply visible, which is the JS-off page.
        body.push_str(
            "<p class=\"pick-hint\">Pick a time to continue.</p>\n\
             <div class=\"picked\" id=\"picked\" aria-live=\"polite\"></div>\n",
        );

        // The details form, by the same machinery as every other type.
        let form_options = FormOptions {
            action: options.action.clone(),
            values: options.values.clone(),
            errors: options.errors.clone(),
            assets: options.assets.clone(),
            theme: options.theme,
            ..FormOptions::default()
        };
        body.push_str("<div class=\"details\">\n");
        for field in &self.fields {
            body.push_str(&self.render_field(field, &form_options));
        }
        for ack in &self.acknowledgments {
            body.push_str(&render_acknowledgment(ack, &form_options));
        }
        body.push_str("</div>\n<button type=\"submit\">Book this time</button>\n</form>\n");

        // Conditions for the detail fields, exactly as the form emits them.
        body.push_str(&format!(
            "<script type=\"application/json\" id=\"conditions\">{}</script>\n",
            serde_json::to_string(&self.condition_map())
                .unwrap_or_else(|_| "{}".into())
                .replace("</", "<\\/")
        ));
        let mut config = serde_json::json!({
            "timezone": tz.to_string(),
            "week_start": week.to_string(),
            "week_end": week_end.to_string(),
        });
        if let Some(chosen) = chosen {
            config["duration"] = chosen.into();
        }
        if let Some(url) = &options.live_slots_url {
            config["slots_url"] = url.as_str().into();
        }
        body.push_str(&format!(
            "<script type=\"application/json\" id=\"booking-config\">{}</script>\n",
            config.to_string().replace("</", "<\\/")
        ));
        body.push_str(&format!(
            "<script src=\"{0}form.js\" defer></script>\n<script src=\"{0}booking.js\" defer></script>\n",
            escape_text(&options.assets)
        ));

        let html = format!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n\
             <meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
             <title>{}</title>\n\
             {}\n\
             <link rel=\"stylesheet\" href=\"{}booking.css\">\n\
             </head>\n<body>\n{}<main class=\"booking\">\n{}</main>\n</body>\n</html>\n",
            escape_text(&self.title),
            crate::brand::FAVICON_LINK,
            escape_text(&options.assets),
            crate::form::site_header(),
            body
        );

        Ok(BookingPage {
            html,
            style: page_style(&options.theme),
        })
    }
}

fn local_day(slot: &Slot, tz: Tz) -> NaiveDate {
    slot.start.with_timezone(&tz).date_naive()
}

/// The week grid, on the same tokens as everything else. Mobile does not
/// shrink the columns: the grid scrolls sideways with snap stops, which is
/// the day-pager pattern in one CSS rule.
///
/// The page's one reveal lives here too: with `:has` support, the details
/// form stays out of the way until a time is picked, and picking one is what
/// discloses it. Browsers without `:has` (and readers with CSS off) get the
/// whole page at once, which is exactly the JS-off contract — the reveal is
/// an enhancement with no script behind it at all.
pub(crate) const BOOKING_STRUCTURE: &str = r#"
/* --- the week frame ----------------------------------------------------- */

.booking .weeknav { display:grid; grid-template-columns:1fr auto 1fr; align-items:center;
  gap:0.75rem; margin:2rem 0 1rem; }
.booking .weeknav a { font-family:var(--font-mono); font-size:0.8125rem; text-decoration:none;
  color:var(--muted); border:1px solid var(--line); border-radius:999px; padding:0.3rem 0.9rem;
  justify-self:start; white-space:nowrap; transition:color .12s ease, border-color .12s ease; }
.booking .weeknav a:hover { color:var(--text); border-color:var(--muted); }
.booking .weeknav > :last-child { justify-self:end; }
.booking .weeknav .range { font-family:var(--font-mono); font-size:0.8125rem; font-weight:500;
  letter-spacing:0.06em; text-transform:uppercase; }

.booking .durations { display:inline-flex; gap:0.25rem; border:1px solid var(--line);
  border-radius:999px; padding:0.25rem; margin:0 0 1.25rem; }
.booking .durations a { font-family:var(--font-mono); font-size:0.8125rem; text-decoration:none;
  color:var(--muted); padding:0.25rem 0.9rem; border-radius:999px; transition:color .12s ease; }
.booking .durations a:hover { color:var(--text); }
.booking .durations a[aria-current="true"] { background:var(--accent); color:var(--on-accent); }

.booking .zone { color:var(--muted); font-size:0.875rem; margin:0 0 1.25rem; }
.booking .stale { color:var(--signal); font-size:0.9rem; }
.booking .notice { color:var(--signal); border-left:2px solid var(--signal);
  padding:0.25rem 0 0.25rem 1rem; font-size:0.9375rem; }
.booking .notice a { color:var(--signal); }
.booking .empty { color:var(--muted); margin:2rem 0; }

.booking .week { display:grid; grid-auto-flow:column; grid-auto-columns:minmax(7.25rem,1fr);
  gap:1rem; overflow-x:auto; scroll-snap-type:x proximity; padding-bottom:0.75rem; }
.booking .day { scroll-snap-align:start; min-width:0; }
.booking .day h2 { display:flex; flex-direction:column; gap:0.125rem; margin:0 0 0.75rem;
  padding:0 0 0.5rem; border-bottom:1px solid var(--line); }
.booking .day .dow { font-family:var(--font-mono); font-size:0.75rem; font-weight:500;
  text-transform:uppercase; letter-spacing:0.08em; }
.booking .day .date { font-size:0.8125rem; font-weight:400; color:var(--muted); }

/* --- slots: the time is the button -------------------------------------- */

.booking .slot { display:block; text-align:center; font-family:var(--font-mono);
  font-size:0.875rem; font-variant-numeric:tabular-nums; border:1px solid var(--line);
  border-radius:var(--radius); padding:0.55rem 0.5rem; margin-bottom:0.5rem; cursor:pointer;
  background:var(--surface); transition:border-color .12s ease, background .12s ease,
  color .12s ease, opacity .3s ease, transform .3s ease; }
.booking .slot:not(.answer):hover { border-color:var(--accent); color:var(--accent); }
.booking .slot input { position:absolute; opacity:0; pointer-events:none; }
/* The fill is the *booking* pick alone. A poll cell is also a `.slot` and
   always holds a checked tri-state radio, so keying this on `input:checked`
   would flood every cell — the selector names the booking radio on purpose. */
.booking .slot:has(input[name="_slot"]:checked) { background:var(--accent);
  border-color:var(--accent); color:var(--on-accent); }
.booking .slot:has(input:focus-visible) { border-color:var(--ring);
  box-shadow:0 0 0 3px color-mix(in srgb, var(--ring) 35%, transparent); }
/* A slot someone else just took leaves quietly; its day follows when empty. */
.booking .slot.gone { opacity:0; transform:scale(0.96); pointer-events:none; }

/* --- the reveal: pick a time, get the form ------------------------------- */

.booking .pick-hint { color:var(--muted); font-size:0.9375rem; margin:1.5rem 0 0; }
.booking .picked { display:none; }
.booking .picked:not(:empty) { display:block; font-family:var(--font-mono); font-size:0.875rem;
  border:1px solid var(--accent); border-left-width:3px; border-radius:var(--radius);
  padding:0.6rem 0.9rem; margin:1.5rem 0 1.5rem; }
.booking .details { margin-top:1.5rem; }
@supports selector(:has(*)) {
  .booking form:not(:has(input[name="_slot"]:checked)) .details,
  .booking form:not(:has(input[name="_slot"]:checked)) button[type="submit"] { display:none; }
  .booking form:has(input[name="_slot"]:checked) .pick-hint { display:none; }
  .booking form:not(:has(input[name="_slot"])) .pick-hint { display:none; }
}

/* --- the poll grid ------------------------------------------------------- */

.booking .slot.answer { display:block; text-align:left; font-family:var(--font-sans);
  cursor:default; padding:0.55rem 0.75rem; }
.booking .slot.answer legend { font-family:var(--font-mono); font-size:0.8125rem;
  font-weight:500; padding:0; float:left; width:100%; text-transform:none;
  letter-spacing:0.01em; color:var(--text); }
.booking .slot.answer .len { color:var(--muted); font-weight:400; }
.booking .count { color:var(--muted); font-size:0.75rem; white-space:nowrap;
  float:right; font-weight:400; }
.booking .slot.answer .tri { display:inline-flex; gap:0.3rem; align-items:center; clear:both;
  margin:0.35rem 0.9rem 0 0; cursor:pointer; color:var(--muted); font-size:0.9rem; }
.booking .slot.answer .tri input { position:static; opacity:1; pointer-events:auto; }
.booking .slot.answer:has(input[value="yes"]:checked) { border-color:var(--accent);
  outline:2px solid var(--ring); }
.booking .slot.answer:has(input[value="if_needed"]:checked) { border-color:var(--signal); }

/* Heat: how many others said yes, in six tellable steps. Colour never
   carries it alone — the count is in the cell's text and accessible name. */
.booking .heat-1 { background:color-mix(in srgb, var(--accent) 8%, var(--surface)); }
.booking .heat-2 { background:color-mix(in srgb, var(--accent) 16%, var(--surface)); }
.booking .heat-3 { background:color-mix(in srgb, var(--accent) 24%, var(--surface)); }
.booking .heat-4 { background:color-mix(in srgb, var(--accent) 33%, var(--surface)); }
.booking .heat-5 { background:color-mix(in srgb, var(--accent) 42%, var(--surface)); }

/* poll.js upgrades the grid to tap-to-cycle painting: the radios leave the
   page (they stay in the form), the whole cell becomes the control, and the
   state chip says what your answer is. */
.booking .week.poll.paint .slot.answer { cursor:pointer; touch-action:none;
  user-select:none; -webkit-user-select:none; }
.booking .paint .slot.answer .tri { display:none; }
.booking .slot.answer .state { display:none; }
.booking .paint .slot.answer .state { display:inline-block; font-family:var(--font-mono);
  font-size:0.75rem; margin-top:0.35rem; padding:0.1rem 0.5rem; border-radius:999px;
  border:1px solid var(--line); color:var(--muted); }
.booking .paint .slot.answer[data-state="yes"] .state { background:var(--accent);
  border-color:var(--accent); color:var(--on-accent); }
.booking .paint .slot.answer[data-state="if_needed"] .state { border-color:var(--signal);
  color:var(--signal); }
.booking .paint .slot.answer:focus-visible { outline:none; border-color:var(--ring);
  box-shadow:0 0 0 3px color-mix(in srgb, var(--ring) 35%, transparent); }
.booking .savestate { font-family:var(--font-mono); font-size:0.8125rem; color:var(--muted); }
.booking form.autosaves button[type="submit"] { display:none; }
"#;

/// Enhancement only — nothing a submission depends on happens here.
const BOOKING_JS: &str = r#"// Generated by mecha-manifest. Enhancement only:
// the page books identically with this file blocked.
(function () {
  "use strict";
  var config = {};
  try { config = JSON.parse(document.getElementById("booking-config").textContent); } catch (e) {}

  // 1. Times in the visitor's zone — every stamp and the zone line together,
  // or nothing: a page half-converted is worse than a labelled host zone.
  var zone = config.timezone;
  try {
    var detected = Intl.DateTimeFormat().resolvedOptions().timeZone;
    if (detected && detected !== config.timezone) {
      zone = detected;
      var fmt = new Intl.DateTimeFormat(undefined,
        { hour: "numeric", minute: "2-digit", timeZone: zone });
      document.querySelectorAll(".when[data-utc]").forEach(function (el) {
        el.textContent = fmt.format(new Date(el.getAttribute("data-utc"))).toLowerCase();
      });
      var note = document.getElementById("zone-note");
      if (note) {
        note.textContent = "Times are shown in your time zone (" + zone +
          " — currently " + fmt.format(new Date()).toLowerCase() + ").";
      }
    }
  } catch (e) { zone = config.timezone; /* the server-rendered labels stand */ }

  // 2. The picked time, echoed in full above the details — the reveal is
  // CSS; this chip is the only part a script writes, and losing it loses a
  // restatement, never the booking.
  var picked = document.getElementById("picked");
  var describe = function (input) {
    var raw = input.value.split("|");
    var start = new Date(raw[0]);
    var minutes = parseInt(raw[1], 10) || 0;
    var end = new Date(start.getTime() + minutes * 60000);
    try {
      var day = new Intl.DateTimeFormat(undefined,
        { weekday: "long", month: "short", day: "numeric", timeZone: zone });
      var time = new Intl.DateTimeFormat(undefined,
        { hour: "numeric", minute: "2-digit", timeZone: zone });
      return day.format(start) + " · " + time.format(start).toLowerCase() +
        " – " + time.format(end).toLowerCase() + " (" + minutes + " min)";
    } catch (e) { return ""; }
  };
  var updatePicked = function () {
    if (!picked) return;
    var checked = document.querySelector('input[name="_slot"]:checked');
    picked.textContent = checked ? describe(checked) : "";
  };
  document.addEventListener("change", function (event) {
    if (event.target && event.target.name === "_slot") updatePicked();
  });
  updatePicked(); // a rejected submission arrives with its slot re-checked

  // 3. Live availability. The page polls the same truth the POST checks, so
  // a slot someone else just held leaves the page instead of waiting to fail
  // at submit; if it was *your* pick, the page says so out loud. New slots
  // (a freed cancellation, a fresh push) reload a pristine page — anything
  // typed makes the reload an offer instead, never a theft.
  var note = document.getElementById("live-note");
  if (config.slots_url && note && window.fetch) {
    var dirty = false;
    document.addEventListener("input", function (event) {
      if (event.target && event.target.closest && event.target.closest(".details")) dirty = true;
    });
    var hostDay = null;
    try {
      hostDay = new Intl.DateTimeFormat("en-CA",
        { timeZone: config.timezone, year: "numeric", month: "2-digit", day: "2-digit" });
    } catch (e) {}
    var inShownWeek = function (iso) {
      if (!hostDay || !config.week_start) return false;
      var day = hostDay.format(new Date(iso));
      return day >= config.week_start && day < config.week_end;
    };
    var tell = function (message, withReload) {
      note.textContent = message;
      if (withReload) {
        var link = document.createElement("a");
        link.href = window.location.href;
        link.textContent = "Show them";
        note.appendChild(document.createTextNode(" — "));
        note.appendChild(link);
      }
      note.hidden = false;
    };
    var sweep = function (offered) {
      var open = {};
      offered.forEach(function (s) { open[s.start + "|" + s.duration_minutes] = true; });
      var shown = {};
      var pickTaken = false;
      document.querySelectorAll('.week input[name="_slot"]').forEach(function (input) {
        var slot = input.closest(".slot");
        if (!slot || slot.classList.contains("gone")) return;
        if (open[input.value]) { shown[input.value] = true; return; }
        if (input.checked) { input.checked = false; pickTaken = true; }
        slot.classList.add("gone");
        window.setTimeout(function () {
          var day = slot.closest(".day");
          slot.remove();
          if (day && !day.querySelector(".slot")) day.remove();
        }, 350);
      });
      if (pickTaken) {
        updatePicked();
        tell("That time was just taken — these are still open.");
      }
      var fresh = offered.some(function (s) {
        return s.duration_minutes === config.duration &&
          !shown[s.start + "|" + s.duration_minutes] &&
          inShownWeek(s.start) && new Date(s.start) > new Date();
      });
      if (fresh) {
        var untouched = !dirty && !document.querySelector('input[name="_slot"]:checked');
        if (untouched) { window.location.reload(); }
        else if (note.hidden) { tell("More times just opened up.", true); }
      }
    };
    var refresh = function () {
      fetch(config.slots_url, { headers: { Accept: "application/json" } })
        .then(function (res) { return res.ok ? res.json() : null; })
        .then(function (data) { if (data && data.slots) sweep(data.slots); })
        .catch(function () { /* offline is not an error the page can fix */ });
    };
    window.setInterval(refresh, 30000);
    document.addEventListener("visibilitychange", function () {
      if (document.visibilityState === "visible") refresh();
    });
  }
})();
"#;

/// The poll grid's enhancement: tap to cycle, drag to paint, autosave.
/// Everything here rides on the radios staying in the form — the script
/// changes how they are set, never what is submitted.
const POLL_JS: &str = r#"// Generated by mecha-manifest. Enhancement only:
// the poll answers identically with this file blocked.
(function () {
  "use strict";
  var grid = document.querySelector(".week.poll");
  var form = grid && grid.closest("form");
  if (!grid || !form || !window.fetch) return;
  var cells = Array.prototype.slice.call(grid.querySelectorAll(".slot.answer"));
  // A closed poll is read-only and stays exactly as rendered.
  if (!cells.length || cells[0].querySelector("input[disabled]")) return;

  var ORDER = ["yes", "if_needed", "no"];
  var WORDS = { yes: "yes", if_needed: "if needed", no: "no" };

  var stateOf = function (cell) {
    var checked = cell.querySelector("input:checked");
    return checked ? checked.value : "no";
  };
  var paint = function (cell, state) {
    var input = cell.querySelector('input[value="' + state + '"]');
    if (input) input.checked = true;
    cell.setAttribute("data-state", state);
    cell.querySelector(".state").textContent = WORDS[state];
    var when = cell.querySelector(".when");
    var len = cell.querySelector(".len");
    cell.setAttribute("aria-label",
      (when ? when.textContent : "") + (len ? " " + len.textContent : "") +
      " — your answer: " + WORDS[state] + ". Press to change.");
  };
  var apply = function (cell, state) {
    if (stateOf(cell) === state && cell.getAttribute("data-state") === state) return;
    paint(cell, state);
    queueSave();
  };

  grid.classList.add("paint");
  form.classList.add("autosaves");
  cells.forEach(function (cell) {
    var chip = document.createElement("span");
    chip.className = "state";
    cell.appendChild(chip);
    cell.setAttribute("role", "button");
    cell.setAttribute("tabindex", "0");
    paint(cell, stateOf(cell));
  });

  // Tap cycles one cell; a drag paints every cell it crosses with the state
  // the first tap produced — the anchor decides the stroke, so a mixed drag
  // never flickers (when2meet's own mechanic).
  var stroke = null;
  var cellAt = function (event) {
    var el = document.elementFromPoint(event.clientX, event.clientY);
    return el && el.closest ? el.closest(".slot.answer") : null;
  };
  grid.addEventListener("pointerdown", function (event) {
    var cell = cellAt(event);
    if (!cell) return;
    event.preventDefault();
    stroke = ORDER[(ORDER.indexOf(stateOf(cell)) + 1) % ORDER.length];
    apply(cell, stroke);
    try { grid.setPointerCapture(event.pointerId); } catch (e) {}
  });
  grid.addEventListener("pointermove", function (event) {
    if (stroke === null) return;
    var cell = cellAt(event);
    if (cell) apply(cell, stroke);
  });
  ["pointerup", "pointercancel"].forEach(function (name) {
    grid.addEventListener(name, function () { stroke = null; });
  });
  grid.addEventListener("keydown", function (event) {
    if (event.key !== " " && event.key !== "Enter") return;
    var cell = event.target.closest && event.target.closest(".slot.answer");
    if (!cell) return;
    event.preventDefault();
    apply(cell, ORDER[(ORDER.indexOf(stateOf(cell)) + 1) % ORDER.length]);
  });

  // Autosave: every commit POSTs, debounced. A failed save says so and
  // hands back the button — the JS-off path is also the degraded path.
  var status = document.getElementById("savestate");
  var timer = null;
  var inflight = false;
  var again = false;
  var say = function (text) {
    if (status) { status.hidden = false; status.textContent = text; }
  };
  var fallback = function () {
    say("Couldn’t save — use the button below.");
    form.classList.remove("autosaves");
  };
  var save = function () {
    if (inflight) { again = true; return; }
    inflight = true;
    var body = new URLSearchParams(new FormData(form));
    fetch(window.location.pathname, {
      method: "POST", body: body, headers: { Accept: "application/json" }
    })
      .then(function (res) {
        if (res.status === 204) say("Saved — you can change your answers until the poll closes.");
        else fallback();
      })
      .catch(fallback)
      .then(function () {
        inflight = false;
        if (again) { again = false; save(); }
      });
  };
  var queueSave = function () {
    if (timer) window.clearTimeout(timer);
    say("Saving…");
    timer = window.setTimeout(save, 700);
  };
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn booking() -> RequestType {
        RequestType::from_toml(
            r#"
            id = "book"
            version = 1
            kind = "booking"
            title = "Book a meeting"

            [[fields]]
            name = "requester_name"
            label = "Your name"
            kind = "text"
            max_length = 120
            required = true

            [[fields]]
            name = "requester_email"
            label = "Your email"
            kind = "email"
            required = true

            [verification]
            field = "requester_email"

            [availability]
            timezone = "America/New_York"
            durations = [30, 60]
            [[availability.windows]]
            day = "tue"
            start = "13:00"
            end = "17:00"
        "#,
        )
        .unwrap()
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn slot(start: &str, minutes: u32) -> Slot {
        Slot {
            start: t(start),
            end: t(start) + Duration::minutes(i64::from(minutes)),
            duration_minutes: minutes,
        }
    }

    fn options(now: &str) -> BookingOptions {
        BookingOptions {
            action: "/s/alice/book".into(),
            assets: "/s/alice/book/".into(),
            now: t(now),
            ..BookingOptions::default()
        }
    }

    /// The JS-off contract in one test: radios carry UTC instants, times are
    /// rendered in the host zone and the zone is named, the details fields
    /// and submit are on the same page, and no inline script exists.
    #[test]
    fn the_page_books_with_javascript_off() {
        let slots = [
            slot("2026-08-11T17:00:00Z", 30),
            slot("2026-08-11T17:00:00Z", 60),
            slot("2026-08-13T18:30:00Z", 30),
        ];
        let page = booking()
            .booking_page(&slots, &options("2026-08-10T12:00:00Z"))
            .unwrap();
        let html = &page.html;
        assert!(html.contains(r#"name="_slot" value="2026-08-11T17:00:00Z|30""#));
        // 17:00Z is 1pm Eastern in August; the column is the local day.
        assert!(html.contains("1:00 pm"), "host-zone rendering");
        assert!(html.contains(r#"<span class="dow">Tue</span>"#));
        assert!(html.contains(r#"<span class="date">Aug 11</span>"#));
        assert!(html.contains(r#"<span class="date">Aug 13</span>"#));
        assert!(html.contains("Times are shown in America/New_York"));
        assert!(
            html.contains(r#"name="requester_email""#),
            "details fields ride along"
        );
        assert!(html.contains("Book this time"));
        assert!(
            !html.contains("<script>"),
            "no inline script under the gate CSP"
        );
        // Both scripts external, one stylesheet.
        assert!(html.contains("booking.js") && html.contains("form.js"));
        let names: Vec<&str> = page.assets().iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "booking.css",
                "form.js",
                "booking.js",
                "poll.js",
                "survey.js",
                "screen.js"
            ]
        );
    }

    /// One start time renders once: the meeting length is a server-side
    /// choice made through links, exactly like week paging, so the page
    /// dedupes with JavaScript off and the links preserve each other's state.
    #[test]
    fn each_length_is_its_own_view_and_the_links_work_without_js() {
        let slots = [
            slot("2026-08-11T17:00:00Z", 30),
            slot("2026-08-11T17:00:00Z", 60),
            slot("2026-08-13T18:30:00Z", 30),
        ];
        // Default: the shortest length, so nothing renders twice.
        let page = booking()
            .booking_page(&slots, &options("2026-08-10T12:00:00Z"))
            .unwrap();
        assert!(page.html.contains("2026-08-11T17:00:00Z|30"));
        assert!(
            !page.html.contains(r#"value="2026-08-11T17:00:00Z|60""#),
            "the 60-minute slot lives on the 60-minute view"
        );
        // The switch is links with the current one marked.
        assert!(page.html.contains(r#"aria-current="true">30 min</a>"#));
        assert!(page.html.contains("&mins=60"));

        // ?mins=60 shows the 60-minute slots and only those.
        let mut hour = options("2026-08-10T12:00:00Z");
        hour.duration = Some(60);
        let page = booking().booking_page(&slots, &hour).unwrap();
        assert!(page.html.contains(r#"value="2026-08-11T17:00:00Z|60""#));
        assert!(!page.html.contains(r#"value="2026-08-11T17:00:00Z|30""#));
        assert!(page.html.contains(r#"aria-current="true">60 min</a>"#));

        // A length nobody offers falls back rather than rendering nothing.
        let mut bogus = options("2026-08-10T12:00:00Z");
        bogus.duration = Some(45);
        let page = booking().booking_page(&slots, &bogus).unwrap();
        assert!(page.html.contains(r#"value="2026-08-11T17:00:00Z|30""#));

        // One length on offer: no switch at all, and no &mins= anywhere.
        let single = [slot("2026-08-11T17:00:00Z", 30)];
        let page = booking()
            .booking_page(&single, &options("2026-08-10T12:00:00Z"))
            .unwrap();
        assert!(!page.html.contains("class=\"durations\""));
        assert!(!page.html.contains("&mins="));
    }

    /// A rejected submission keeps what the visitor already did: the picked
    /// slot arrives re-checked, the typed values ride the details fields,
    /// and the error summary names fields by label, never by name.
    #[test]
    fn a_rejected_submission_keeps_the_slot_and_names_fields_by_label() {
        let slots = [slot("2026-08-11T17:00:00Z", 30)];
        let mut options = options("2026-08-10T12:00:00Z");
        options
            .values
            .insert("_slot".into(), "2026-08-11T17:00:00Z|30".into());
        options.values.insert("requester_name".into(), "Ada".into());
        options.errors = vec![crate::ValidationError {
            field: "requester_email".into(),
            message: "is not a valid email address".into(),
        }];
        let page = booking().booking_page(&slots, &options).unwrap();
        assert!(
            page.html
                .contains(r#"value="2026-08-11T17:00:00Z|30" checked"#),
            "the picked slot survives the round trip"
        );
        assert!(page.html.contains(r#"value="Ada""#), "typed values survive");
        assert!(
            page.html.contains(">Your email</a>"),
            "the summary says the label"
        );
        assert!(!page.html.contains(">requester_email</a>"));
    }

    #[test]
    fn weeks_page_forward_never_backward_and_filter_to_the_shown_week() {
        let slots = [
            slot("2026-08-11T17:00:00Z", 30),
            slot("2026-08-25T17:00:00Z", 30),
        ];
        let now = "2026-08-10T12:00:00Z";
        let first = booking().booking_page(&slots, &options(now)).unwrap();
        assert!(
            first.html.contains("?week=2026-08-17\">later"),
            "a later slot means a next link"
        );
        assert!(
            !first.html.contains("earlier"),
            "the first week has no past"
        );
        assert!(
            !first.html.contains("Aug 25"),
            "next week's slot is not shown"
        );

        let mut later = options(now);
        later.week = Some("2026-08-25".parse().unwrap());
        let page = booking().booking_page(&slots, &later).unwrap();
        assert!(page.html.contains("2026-08-25T17:00:00Z|30"));
        assert!(page.html.contains("earlier"));

        // ?week= pointing into the past clamps to the present week.
        let mut past = options(now);
        past.week = Some("2020-01-06".parse().unwrap());
        let page = booking().booking_page(&slots, &past).unwrap();
        assert!(page.html.contains("2026-08-11T17:00:00Z|30"));
    }

    #[test]
    fn stale_slots_and_empty_weeks_tell_the_truth() {
        // A cache still holding yesterday's slot must not offer it.
        let slots = [slot("2026-08-11T17:00:00Z", 30)];
        let page = booking()
            .booking_page(&slots, &options("2026-08-12T12:00:00Z"))
            .unwrap();
        assert!(!page.html.contains("_slot"), "the past is not offerable");
        assert!(page.html.contains("check back soon"));

        let mut with_notice = options("2026-08-10T12:00:00Z");
        with_notice.stale_notice =
            Some("Refreshed 2026-08-09; recent changes may not show.".into());
        let page = booking().booking_page(&slots, &with_notice).unwrap();
        assert!(page.html.contains("recent changes may not show"));
    }

    #[test]
    fn a_plain_request_cannot_render_a_booking_page() {
        let plain = RequestType::from_toml(
            r#"
            id = "meeting"
            version = 1
            title = "Meeting"
            [[fields]]
            name = "email"
            label = "Email"
            kind = "email"
            required = true
            [verification]
            field = "email"
        "#,
        )
        .unwrap();
        assert!(plain.booking_page(&[], &BookingOptions::default()).is_err());
    }

    /// The poll cell carries its heat as a discrete class and its count as
    /// text, so the information never rides on colour alone — and the page
    /// ships the autosave status element and both scripts.
    #[test]
    fn poll_cells_carry_discrete_heat_and_counts_in_text() {
        let candidates = [
            PollCandidate {
                start: t("2030-02-05T18:00:00Z"),
                end: t("2030-02-05T19:00:00Z"),
                duration_minutes: 60,
                mine: Some(PollAnswer::Yes),
                yes_count: 3,
            },
            PollCandidate {
                start: t("2030-02-06T15:00:00Z"),
                end: t("2030-02-06T16:00:00Z"),
                duration_minutes: 60,
                mine: None,
                yes_count: 0,
            },
        ];
        let options = PollPageOptions {
            title: "Lab meeting".into(),
            participant: "Tal".into(),
            timezone: chrono_tz::America::New_York,
            action: String::new(),
            assets: "/p/a/".into(),
            theme: crate::theme::NOCTURNE,
            deadline_local: None,
            responded: 3,
            total: 5,
            open: true,
            notice: None,
        };
        let page = poll_page(&candidates, &options);
        // 3 of 5 yes → ceil(3·5/5) = 3 of the 5 heat steps.
        assert!(page.html.contains("heat-3"));
        assert!(page.html.contains("3 of 5 yes"));
        // Nobody yet: the cold cell says nothing rather than "0 of 5".
        assert!(page.html.contains("heat-0"));
        assert!(!page.html.contains("0 of 5 yes"));
        assert!(page.html.contains(r#"id="savestate""#));
        assert!(page.html.contains("poll.js") && page.html.contains("booking.js"));
        assert!(
            !page.html.contains("<script>"),
            "no inline script under the gate CSP"
        );
    }

    #[test]
    fn mondays_anchor_weeks() {
        assert_eq!(
            week_of("2026-08-13".parse().unwrap()),
            "2026-08-10".parse().unwrap()
        );
        assert_eq!(
            week_of("2026-08-10".parse().unwrap()),
            "2026-08-10".parse().unwrap()
        );
        assert_eq!(
            week_of("2026-08-16".parse().unwrap()),
            "2026-08-10".parse().unwrap()
        );
    }

    /// MEETING-POLL-UX-DESIGN.md §5's table, row by row: what each mode
    /// books, and that no mode books over a silent participant.
    #[test]
    fn auto_book_follows_the_table_and_never_books_over_silence() {
        use crate::availability::AutoBook::{Feasible, Manual, Unanimous};
        use std::collections::BTreeMap;
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let candidates = vec![
            (t("2030-02-05T18:00:00Z"), 60u32),
            (t("2030-02-06T15:00:00Z"), 60u32),
        ];
        let answer = |wed: &str, thu: &str| -> BTreeMap<String, PollAnswer> {
            [
                (
                    "2030-02-05T18:00:00Z|60".to_string(),
                    PollAnswer::parse(wed).unwrap(),
                ),
                (
                    "2030-02-06T15:00:00Z|60".to_string(),
                    PollAnswer::parse(thu).unwrap(),
                ),
            ]
            .into()
        };
        let starts =
            |ranked: &[RankedCandidate], mode| auto_book(ranked, 2, 2, mode).map(|c| c.start);

        // One unanimous slot: unanimous and feasible book it, manual never.
        let ranked = rank_poll(&candidates, &[answer("yes", "no"), answer("yes", "yes")], 2);
        assert_eq!(starts(&ranked, Unanimous), Some(t("2030-02-05T18:00:00Z")));
        assert_eq!(starts(&ranked, Feasible), Some(t("2030-02-05T18:00:00Z")));
        assert_eq!(starts(&ranked, Manual), None);

        // Two unanimous: a pick, unless feasible — which takes the earliest.
        let ranked = rank_poll(
            &candidates,
            &[answer("yes", "yes"), answer("yes", "yes")],
            2,
        );
        assert_eq!(starts(&ranked, Unanimous), None);
        assert_eq!(starts(&ranked, Feasible), Some(t("2030-02-05T18:00:00Z")));

        // The best needs an if-needed: feasible accepts the cost, unanimous
        // hands it to the owner.
        let ranked = rank_poll(
            &candidates,
            &[answer("yes", "no"), answer("if_needed", "no")],
            2,
        );
        assert_eq!(starts(&ranked, Unanimous), None);
        assert_eq!(starts(&ranked, Feasible), Some(t("2030-02-05T18:00:00Z")));

        // Nothing feasible: nobody books.
        let ranked = rank_poll(&candidates, &[answer("yes", "no"), answer("no", "yes")], 2);
        assert_eq!(starts(&ranked, Feasible), None);

        // A silent participant: no mode books, however clean the answered look.
        let ranked = rank_poll(&candidates, &[answer("yes", "yes")], 2);
        for mode in [Unanimous, Feasible, Manual] {
            assert!(
                auto_book(&ranked, 1, 2, mode).is_none(),
                "{mode:?} booked over silence"
            );
        }
    }

    /// The guardrail, against every murky shape it must refuse: a tie of
    /// unanimous slots, an if-needed in the best, a silent participant —
    /// and the one clean shape it must accept.
    #[test]
    fn the_clean_winner_is_unique_unanimous_and_fully_attended() {
        use std::collections::BTreeMap;
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let candidates = vec![
            (t("2030-02-05T18:00:00Z"), 60u32),
            (t("2030-02-06T15:00:00Z"), 60u32),
        ];
        let answer = |wed: &str, thu: &str| -> BTreeMap<String, PollAnswer> {
            [
                (
                    "2030-02-05T18:00:00Z|60".to_string(),
                    PollAnswer::parse(wed).unwrap(),
                ),
                (
                    "2030-02-06T15:00:00Z|60".to_string(),
                    PollAnswer::parse(thu).unwrap(),
                ),
            ]
            .into()
        };

        // Clean: both answered, exactly one unanimous slot.
        let answers = vec![answer("yes", "no"), answer("yes", "yes")];
        let ranked = rank_poll(&candidates, &answers, 2);
        assert_eq!(ranked[0].start, t("2030-02-05T18:00:00Z"));
        assert!(ranked[0].unanimous);
        let winner = clean_winner(&ranked, 2, 2).expect("clean");
        assert_eq!(winner.start, t("2030-02-05T18:00:00Z"));

        // A tie of unanimous slots: judgment, not a guess.
        let answers = vec![answer("yes", "yes"), answer("yes", "yes")];
        let ranked = rank_poll(&candidates, &answers, 2);
        assert!(
            clean_winner(&ranked, 2, 2).is_none(),
            "two unanimous = stage"
        );

        // The best needs an if-needed: someone pays, so someone decides.
        let answers = vec![answer("yes", "no"), answer("if_needed", "no")];
        let ranked = rank_poll(&candidates, &answers, 2);
        assert!(ranked[0].feasible && !ranked[0].unanimous);
        assert!(clean_winner(&ranked, 2, 2).is_none());

        // A silent participant blocks auto even if the answered are unanimous.
        let answers = vec![answer("yes", "no")];
        let ranked = rank_poll(&candidates, &answers, 2);
        assert!(clean_winner(&ranked, 1, 2).is_none(), "silence blocks auto");

        // Feasible-with-cost still outranks infeasible, and fewer
        // if-neededs outrank more at equal yeses.
        let answers = vec![answer("yes", "if_needed"), answer("if_needed", "if_needed")];
        let ranked = rank_poll(&candidates, &answers, 2);
        assert!(ranked[0].feasible);
        assert_eq!(ranked[0].if_needed, 1, "the cheaper feasible slot leads");
    }

    /// Every page in this family links one `booking.css`, so every path that
    /// produces it has to produce the same bytes. Two of them did not: the
    /// booking builders left `SURVEY_STRUCTURE` out, and because the gallery
    /// writes the first `booking.css` it meets and marks the job done, the
    /// documentation gallery's survey page shipped with no survey CSS at all —
    /// visible rank buttons wearing the full-size accent pill, and list markers
    /// where `list-style:none` was meant to be. The live server built its
    /// assets down the other path and looked fine, which is exactly why nobody
    /// noticed.
    ///
    /// Asserting on the *presence* of a rule rather than only on equality,
    /// because two paths agreeing on the wrong thing is the other way to fail.
    #[test]
    fn assert_the_stylesheet_has_one_definition() {
        for theme in crate::theme::BUILT_IN {
            let shared = page_style(&theme);

            let served = booking_assets(&theme)
                .into_iter()
                .find(|(name, _)| *name == "booking.css")
                .map(|(_, css)| css)
                .expect("booking.css is among the assets");
            assert_eq!(served, shared, "what a server answers with has drifted");

            // A really rendered page, not a second call to the same helper:
            // the drift was a page builder inlining its own `format!`, and
            // only a real page catches that.
            let candidates = vec![PollCandidate {
                start: "2026-03-03T14:00:00Z".parse().unwrap(),
                end: "2026-03-03T14:30:00Z".parse().unwrap(),
                duration_minutes: 30,
                mine: None,
                yes_count: 0,
            }];
            let rendered = poll_page(
                &candidates,
                &PollPageOptions {
                    title: "Lab meeting".into(),
                    participant: "Tal".into(),
                    timezone: chrono_tz::America::New_York,
                    action: String::new(),
                    assets: "/p/a/".into(),
                    theme,
                    deadline_local: None,
                    responded: 0,
                    total: 1,
                    open: true,
                    notice: None,
                },
            );
            assert_eq!(
                rendered.style, shared,
                "a rendered poll page builds a different booking.css"
            );

            // The rules the gallery was missing, named so a future reshuffle
            // of the concatenation cannot quietly drop them again.
            for needle in [".survey .rank-list", ".survey .cloud-w", ".vas-range"] {
                assert!(
                    shared.contains(needle),
                    "`{needle}` is missing from booking.css"
                );
            }
        }
    }
}
