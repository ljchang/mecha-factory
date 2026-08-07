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
    /// Values and errors from a rejected submission being shown back.
    pub values: serde_json::Map<String, serde_json::Value>,
    pub errors: Vec<crate::ValidationError>,
    /// A line about freshness, when the server judges the cache old enough
    /// to say so. Rendered verbatim (escaped), so wording stays server-side.
    pub stale_notice: Option<String>,
}

impl Default for BookingOptions {
    fn default() -> Self {
        BookingOptions {
            action: String::new(),
            assets: String::new(),
            theme: crate::theme::NOCTURNE,
            now: DateTime::<Utc>::UNIX_EPOCH,
            week: None,
            values: serde_json::Map::new(),
            errors: Vec::new(),
            stale_notice: None,
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
    pub fn assets(&self) -> [(&'static str, &str); 3] {
        [
            ("booking.css", &self.style),
            ("form.js", FORM_JS),
            ("booking.js", BOOKING_JS),
        ]
    }
}

/// The page's assets for a theme, independent of any render — what a server
/// answers asset requests from, where [`BookingPage::assets`] serves whoever
/// is writing a rendered bundle to disk.
pub fn booking_assets(theme: &crate::Theme) -> [(&'static str, String); 3] {
    [
        (
            "booking.css",
            format!(
                "{}{}{}",
                theme.css(),
                crate::form::FORM_STRUCTURE,
                BOOKING_STRUCTURE
            ),
        ),
        ("form.js", FORM_JS.to_string()),
        ("booking.js", BOOKING_JS.to_string()),
    ]
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
            body.push_str(
                "<div class=\"errors\" role=\"alert\"><p>Some answers need attention:</p><ul>\n",
            );
            for error in &options.errors {
                body.push_str(&format!(
                    "<li>{}: {}</li>\n",
                    escape_text(&error.field),
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
                "<a rel=\"nofollow\" href=\"?week={}\">&larr; earlier</a>\n",
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
                "<a rel=\"nofollow\" href=\"?week={week_end}\">later &rarr;</a>\n"
            ));
        } else {
            body.push_str("<span></span>\n");
        }
        body.push_str("</nav>\n");

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
                    "<section class=\"day\"><h2>{}</h2>\n",
                    day.format("%a %b %-d")
                ));
                for slot in of_day {
                    let start_utc = slot.start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    let local = slot.start.with_timezone(&tz);
                    let label = local.format("%-I:%M %p").to_string().to_lowercase();
                    body.push_str(&format!(
                        "<label class=\"slot\"><input type=\"radio\" name=\"_slot\" \
                         value=\"{start_utc}|{minutes}\" required>\
                         <span class=\"when\" data-utc=\"{start_utc}\">{label}</span>\
                         <span class=\"mins\" data-minutes=\"{minutes}\">{minutes} min</span>\
                         </label>\n",
                        minutes = slot.duration_minutes,
                    ));
                }
                body.push_str("</section>\n");
                day += Duration::days(1);
            }
            body.push_str("</div>\n");
        }

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
        body.push_str(&format!(
            "<script type=\"application/json\" id=\"booking-config\">{}</script>\n",
            serde_json::json!({ "timezone": tz.to_string() })
                .to_string()
                .replace("</", "<\\/")
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
            style: format!(
                "{}{}{}",
                options.theme.css(),
                crate::form::FORM_STRUCTURE,
                BOOKING_STRUCTURE
            ),
        })
    }
}

fn local_day(slot: &Slot, tz: Tz) -> NaiveDate {
    slot.start.with_timezone(&tz).date_naive()
}

/// The week grid, on the same tokens as everything else. Mobile does not
/// shrink the columns: the grid scrolls sideways with snap stops, which is
/// the day-pager pattern in one CSS rule.
const BOOKING_STRUCTURE: &str = r#"
.booking .weeknav { display:flex; justify-content:space-between; align-items:baseline; margin:1.5rem 0 0.5rem; }
.booking .weeknav .range { font-weight:600; }
.booking .zone { color:var(--muted); font-size:0.9rem; }
.booking .stale { color:var(--signal); font-size:0.9rem; }
.booking .week { display:grid; grid-auto-flow:column; grid-auto-columns:minmax(7.5rem,1fr);
  gap:0.75rem; overflow-x:auto; scroll-snap-type:x proximity; padding-bottom:0.5rem; }
.booking .day { scroll-snap-align:start; }
.booking .day h2 { font-size:0.95rem; margin:0 0 0.5rem; color:var(--muted); font-weight:600; }
.booking .slot { display:flex; gap:0.5rem; align-items:baseline; justify-content:space-between;
  border:1px solid var(--line); border-radius:var(--radius); padding:0.45rem 0.7rem;
  margin-bottom:0.5rem; cursor:pointer; background:var(--surface); }
.booking .slot:hover { border-color:var(--accent); }
.booking .slot input { position:absolute; opacity:0; pointer-events:none; }
.booking .slot:has(input:checked) { border-color:var(--accent); outline:2px solid var(--ring); }
.booking .slot .mins { color:var(--muted); font-size:0.85rem; white-space:nowrap; }
.booking .empty { color:var(--muted); margin:2rem 0; }
.booking .durations { display:inline-flex; gap:0; border:1px solid var(--line);
  border-radius:var(--radius); overflow:hidden; margin:0 0 0.75rem; }
.booking .durations button { border:0; background:var(--surface); color:var(--text);
  padding:0.35rem 0.9rem; cursor:pointer; }
.booking .durations button[aria-pressed="true"] { background:var(--accent); color:var(--on-accent); }
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
  try {
    var zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    if (zone && zone !== config.timezone) {
      var whens = document.querySelectorAll(".when[data-utc]");
      var fmt = new Intl.DateTimeFormat(undefined,
        { hour: "numeric", minute: "2-digit", timeZone: zone });
      whens.forEach(function (el) {
        el.textContent = fmt.format(new Date(el.getAttribute("data-utc"))).toLowerCase();
      });
      var nowFmt = new Intl.DateTimeFormat(undefined,
        { hour: "numeric", minute: "2-digit", timeZone: zone });
      var note = document.getElementById("zone-note");
      if (note) {
        note.textContent = "Times are shown in your time zone (" + zone +
          " — currently " + nowFmt.format(new Date()).toLowerCase() + ").";
      }
    }
  } catch (e) { /* the server-rendered labels stand */ }

  // 2. The duration filter, built only when there is something to filter.
  var minutes = [];
  document.querySelectorAll(".slot .mins[data-minutes]").forEach(function (el) {
    var m = el.getAttribute("data-minutes");
    if (minutes.indexOf(m) < 0) minutes.push(m);
  });
  var week = document.querySelector(".week");
  if (week && minutes.length > 1) {
    minutes.sort(function (a, b) { return a - b; });
    var bar = document.createElement("div");
    bar.className = "durations";
    bar.setAttribute("role", "group");
    bar.setAttribute("aria-label", "Meeting length");
    var apply = function (wanted) {
      document.querySelectorAll(".slot").forEach(function (slot) {
        var m = slot.querySelector(".mins").getAttribute("data-minutes");
        var show = wanted === null || m === wanted;
        slot.hidden = !show;
        if (!show) {
          var input = slot.querySelector("input");
          if (input.checked) input.checked = false;
        }
      });
      bar.querySelectorAll("button").forEach(function (b) {
        b.setAttribute("aria-pressed", String(b.dataset.minutes === (wanted === null ? "all" : wanted)));
      });
    };
    var mk = function (label, value) {
      var b = document.createElement("button");
      b.type = "button";
      b.textContent = label;
      b.dataset.minutes = value === null ? "all" : value;
      b.addEventListener("click", function () { apply(value); });
      bar.appendChild(b);
      return b;
    };
    minutes.forEach(function (m) { mk(m + " min", m); });
    mk("all", null).setAttribute("aria-pressed", "true");
    week.parentNode.insertBefore(bar, week);
  }
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
        assert!(html.contains(r#"value="2026-08-11T17:00:00Z|60""#));
        // 17:00Z is 1pm Eastern in August; the column is the local day.
        assert!(html.contains("1:00 pm"), "host-zone rendering");
        assert!(html.contains("Tue Aug 11"));
        assert!(html.contains("Thu Aug 13"));
        assert!(html.contains("Times are shown in America/New_York"));
        assert!(html.contains(r#"name="requester_email""#), "details fields ride along");
        assert!(html.contains("Book this time"));
        assert!(!html.contains("<script>"), "no inline script under the gate CSP");
        // Both scripts external, one stylesheet.
        assert!(html.contains("booking.js") && html.contains("form.js"));
        let names: Vec<&str> = page.assets().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["booking.css", "form.js", "booking.js"]);
    }

    #[test]
    fn weeks_page_forward_never_backward_and_filter_to_the_shown_week() {
        let slots = [
            slot("2026-08-11T17:00:00Z", 30),
            slot("2026-08-25T17:00:00Z", 30),
        ];
        let now = "2026-08-10T12:00:00Z";
        let first = booking().booking_page(&slots, &options(now)).unwrap();
        assert!(first.html.contains("later &rarr;"), "a later slot means a next link");
        assert!(!first.html.contains("earlier"), "the first week has no past");
        assert!(!first.html.contains("Aug 25"), "next week's slot is not shown");

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
        with_notice.stale_notice = Some("Refreshed 2026-08-09; recent changes may not show.".into());
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

    #[test]
    fn mondays_anchor_weeks() {
        assert_eq!(week_of("2026-08-13".parse().unwrap()), "2026-08-10".parse().unwrap());
        assert_eq!(week_of("2026-08-10".parse().unwrap()), "2026-08-10".parse().unwrap());
        assert_eq!(week_of("2026-08-16".parse().unwrap()), "2026-08-10".parse().unwrap());
    }
}
