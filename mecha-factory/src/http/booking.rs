//! The booking page: `/s/{handle}/{id}` on the gate.
//!
//! The GET half only, so far — the page, its assets, its week paging. What
//! it serves is the slot cache home pushed, and nothing else: this handler
//! computes no availability and never could, because the policy's busy data
//! never reaches the box. The claim (the POST) arrives with the hold
//! machinery; until then the page renders and the form's target answers 405,
//! which is honest about exactly what exists.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use mecha_manifest::{BookingOptions, RequestKind, RequestType};

use super::{v1, Failure, Shared};
use crate::config::Origin;
use crate::db::UserRow;

/// A cache whose stamp is older than this states its age on the page. The
/// timer refreshes every fifteen minutes, so a day-old cache means the
/// pipeline has been down for a day — the page keeps working (stale but
/// honest, youcanbookme's own degradation), and says so.
const STALE_AFTER_HOURS: i64 = 24;

#[derive(serde::Deserialize)]
pub struct WeekQuery {
    week: Option<String>,
}

/// Whose booking page, and which — or nothing, in a way that says nothing,
/// exactly as the form's resolve answers. A plain request type is "nothing"
/// here the same way a booking type is "nothing" at `/f/`.
fn resolve(app: &Shared, handle: &str, id: &str) -> Option<(UserRow, RequestType)> {
    let user = match app.db.user_by_handle(handle) {
        Ok(Some(user)) if user.active() => user,
        _ => return None,
    };
    let stored = app.db.type_get(&user.id, id).ok()??;
    let parsed = RequestType::from_toml(&stored.manifest).ok()?;
    parsed.servable().ok()?;
    if parsed.kind != RequestKind::Booking {
        return None;
    }
    Some((user, parsed))
}

fn nothing_here() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Not found</title></head><body><main><h1>Not found</h1>\
         <p>There is no booking page here.</p></main></body></html>",
    )
        .into_response()
}

/// `GET /s/{handle}/{id}` — the weekly view.
pub async fn page(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, id)): Path<(String, String)>,
    Query(query): Query<WeekQuery>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((user, parsed)) = resolve(&app, &handle, &id) else {
        return nothing_here();
    };

    let now = chrono::Utc::now();
    let now_stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // The box subtracts, never adds: live holds and confirmed bookings come
    // off whatever home pushed, judged against the clock at query time so an
    // abandoned hold frees its slot with no sweeper anywhere.
    let blocking = app
        .db
        .bookings_blocking(&user.id, &id, &now_stamp)
        .unwrap_or_default();
    let blocked = |start: &chrono::DateTime<chrono::Utc>,
                   end: &chrono::DateTime<chrono::Utc>|
     -> bool {
        blocking.iter().any(|(b_start, b_end)| {
            let parse = |raw: &str| {
                chrono::DateTime::parse_from_rfc3339(raw)
                    .map(|t| t.with_timezone(&chrono::Utc))
            };
            match (parse(b_start), parse(b_end)) {
                (Ok(bs), Ok(be)) => bs < *end && *start < be,
                // An unparseable row blocks nothing it can name — but it
                // must not unblock anything either; treat it as covering
                // everything, because a corrupt ledger row is a bug to
                // surface, not free time.
                _ => true,
            }
        })
    };
    let cache = app.db.slots_get(&user.id, &id).ok().flatten();
    let (slots, stale_notice) = match &cache {
        Some(row) => {
            let mut slots: Vec<mecha_manifest::availability::Slot> =
                serde_json::from_str(&row.slots).unwrap_or_default();
            slots.retain(|s| !blocked(&s.start, &s.end));
            let stale = chrono::DateTime::parse_from_rfc3339(&row.generated_at)
                .map(|g| now - g.with_timezone(&chrono::Utc)
                    > chrono::Duration::hours(STALE_AFTER_HOURS))
                // An unparseable stamp is stale, not fresh.
                .unwrap_or(true);
            let notice = stale.then(|| {
                format!(
                    "Availability was last refreshed {} — recent changes may not show.",
                    row.generated_at
                )
            });
            (slots, notice)
        }
        // No cache yet: the page exists and says nothing is open, which is
        // what a just-created instrument honestly offers.
        None => (Vec::new(), None),
    };

    let options = BookingOptions {
        action: format!("/s/{handle}/{id}"),
        assets: format!("/s/{handle}/{id}/"),
        theme: mecha_manifest::Theme::by_name(&app.config.theme),
        now,
        week: query.week.as_deref().and_then(|w| w.parse().ok()),
        stale_notice,
        ..BookingOptions::default()
    };
    match parsed.booking_page(&slots, &options) {
        Ok(page) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            page.html,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(%id, error = %e, "rendering a booking page");
            Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `GET /s/{handle}/{id}/{name}` — the page's stylesheet and scripts.
///
/// Answered without resolving the type: the assets are a function of the
/// server's theme alone, and refusing them for an unknown handle would tell
/// a prober which handles exist from an asset URL.
pub async fn asset(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((_handle, _id, name)): Path<(String, String, String)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let theme = mecha_manifest::Theme::by_name(&app.config.theme);
    for (asset_name, body) in mecha_manifest::booking_assets(&theme) {
        if asset_name == name {
            return (
                StatusCode::OK,
                [(
                    header::CONTENT_TYPE,
                    mecha_manifest::content_type(asset_name),
                )],
                body,
            )
                .into_response();
        }
    }
    nothing_here()
}

/// `POST /s/{handle}/{id}` — pick a slot, take the hold, send the link.
///
/// The order is the design: the hold is taken *before* the queue row is
/// written, because the hold is the contended thing — losing the race is
/// ordinary (the visitor gets the refreshed week back with a notice, never
/// an error page) and a queue row without its hold would be a booking that
/// cannot happen. The slot must be one the cache offers right now: a POST
/// naming any other instant is a prober, not a race.
pub async fn submit(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, id)): Path<(String, String)>,
    body: String,
) -> Response {
    use super::intake;

    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((user, parsed)) = resolve(&app, &handle, &id) else {
        return nothing_here();
    };
    let Ok(verification) = parsed.servable() else {
        return nothing_here();
    };

    let raw = intake::form_values(&body);
    let now = chrono::Utc::now();
    let now_stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // The slot, before the fields: parsed strictly, then proved against the
    // cache. `_slot` is `start|minutes` exactly as the page emitted it.
    let slot_arg = raw.get("_slot").and_then(|v| v.as_str()).unwrap_or("");
    let offered = 'found: {
        let Some((start_raw, minutes_raw)) = slot_arg.split_once('|') else {
            break 'found None;
        };
        let (Ok(start), Ok(minutes)) = (
            chrono::DateTime::parse_from_rfc3339(start_raw)
                .map(|t| t.with_timezone(&chrono::Utc)),
            minutes_raw.parse::<u32>(),
        ) else {
            break 'found None;
        };
        let Ok(Some(cache)) = app.db.slots_get(&user.id, &id) else {
            break 'found None;
        };
        let slots: Vec<mecha_manifest::availability::Slot> =
            serde_json::from_str(&cache.slots).unwrap_or_default();
        slots
            .into_iter()
            .find(|s| s.start == start && s.duration_minutes == minutes && s.start > now)
    };
    let Some(slot) = offered else {
        return nothing_here();
    };

    // Keys beginning with `_` are the page's own machinery (`_slot`), not
    // the stranger's answers: stripped before validation here, and the
    // drain side strips them the same way before its re-validation.
    let mut fields_only = raw.clone();
    fields_only.retain(|k, _| !k.starts_with('_'));
    let submission = match parsed.validate_at(&fields_only, mecha_manifest::Phase::Submit) {
        Ok(submission) => submission,
        Err(_) => {
            // Their week back with the message; re-rendering the form with
            // per-field errors folds in when the page grows error display
            // for the POST path.
            return page_with_notice(
                &app, &user, &parsed, &handle, &id,
                "Some answers need attention — please try again.",
            );
        }
    };
    let Some(address) = submission
        .values
        .get(&verification.field)
        .and_then(|v| v.as_str())
    else {
        return nothing_here();
    };

    let policy = parsed.booking_policy();
    let booking_id = crate::intake::mint_token();
    let hold = crate::db::BookingRow {
        id: booking_id.clone(),
        user_id: user.id.clone(),
        instrument_id: id.clone(),
        slot_start: slot.start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        slot_end: slot.end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        duration_minutes: i64::from(slot.duration_minutes),
        state: "held".into(),
        hold_expires: Some(
            (now + chrono::Duration::minutes(i64::from(policy.hold_minutes)))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
        queue_seq: None,
        manage_hash: None,
        ics_sequence: 0,
        created_at: now_stamp.clone(),
        confirmed_at: None,
        cancelled_at: None,
    };
    match app.db.booking_hold(&hold, &now_stamp) {
        Ok(true) => {}
        Ok(false) => {
            // The race, lost gracefully: the refreshed week, with the truth.
            return page_with_notice(
                &app, &user, &parsed, &handle, &id,
                "That time was just taken — these are still open.",
            );
        }
        Err(e) => {
            tracing::error!(%id, error = %e, "taking a hold");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    }

    // The slot rides in the payload as typed values, underscore-prefixed
    // like `_token`: the drain side reads them without a manifest field
    // existing for them, exactly as the page submitted them.
    let mut values = submission.values.clone();
    values.insert("_slot_start".into(), hold.slot_start.clone().into());
    values.insert("_slot_end".into(), hold.slot_end.clone().into());
    values.insert(
        "_duration_minutes".into(),
        slot.duration_minutes.into(),
    );
    values.insert("_booking_id".into(), booking_id.clone().into());

    let token = crate::intake::mint_token();
    let row = crate::db::Submission {
        user_id: user.id.clone(),
        type_id: id.clone(),
        payload: serde_json::to_string(&values).unwrap_or_else(|_| "{}".into()),
        created_at: now_stamp.clone(),
        retain_until: parsed.retain_days.map(crate::db::days_from_now),
        verify_hash: crate::intake::hash_token(&token),
        verify_expires: crate::db::hours_from_now(verification.expires_hours),
        recipient_hash: crate::intake::recipient_hash(address),
    };
    let seq = match app.db.submission_add(&row) {
        Ok(seq) => seq,
        Err(e) => {
            tracing::error!(error = %e, "storing a booking submission");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    };
    let _ = app.db.booking_bind_queue(&booking_id, seq);

    let link = format!(
        "{}/s/{handle}/{id}/c/{token}",
        app.config.base_url(crate::config::Role::Gate)
    );
    app.mailer.send_verification(address, &parsed, &link);
    tracing::info!(handle = %user.handle, %id, seq, "booking held, awaiting verification");

    intake::page(
        StatusCode::OK,
        intake::shell(
            "Check your email",
            &format!(
                "<h1>Almost there</h1><p>Your time is held for {} minutes. A \
                 confirmation link is on its way to {} — clicking it books the \
                 meeting.</p>",
                policy.hold_minutes,
                mecha_manifest::escape_text(address)
            ),
            &format!("/s/{handle}/{id}/"),
        ),
    )
}

/// The weekly page with a line of truth above it — the race loser's view,
/// and the invalid submission's. Rendered by the same path as the GET so
/// what it offers is already narrowed by every live hold including the one
/// that just beat this visitor.
fn page_with_notice(
    app: &Shared,
    user: &UserRow,
    parsed: &RequestType,
    handle: &str,
    id: &str,
    notice: &str,
) -> Response {
    let now = chrono::Utc::now();
    let now_stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let blocking = app
        .db
        .bookings_blocking(&user.id, id, &now_stamp)
        .unwrap_or_default();
    let mut slots: Vec<mecha_manifest::availability::Slot> = app
        .db
        .slots_get(&user.id, id)
        .ok()
        .flatten()
        .and_then(|row| serde_json::from_str(&row.slots).ok())
        .unwrap_or_default();
    slots.retain(|s| {
        !blocking.iter().any(|(b_start, b_end)| {
            match (
                chrono::DateTime::parse_from_rfc3339(b_start),
                chrono::DateTime::parse_from_rfc3339(b_end),
            ) {
                (Ok(bs), Ok(be)) => {
                    bs.with_timezone(&chrono::Utc) < s.end
                        && s.start < be.with_timezone(&chrono::Utc)
                }
                _ => true,
            }
        })
    });
    let options = BookingOptions {
        action: format!("/s/{handle}/{id}"),
        assets: format!("/s/{handle}/{id}/"),
        theme: mecha_manifest::Theme::by_name(&app.config.theme),
        now,
        stale_notice: Some(notice.to_string()),
        ..BookingOptions::default()
    };
    match parsed.booking_page(&slots, &options) {
        Ok(page) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            page.html,
        )
            .into_response(),
        Err(_) => Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response(),
    }
}

/// `GET /s/{handle}/{id}/c/{token}` — one button, no token touched.
///
/// The same scanner rule as the form's confirm: a GET that spent the token
/// would let a mail scanner's robot book meetings. The token is not even
/// looked at — a page that varied on its state would be an oracle.
pub async fn confirm_page(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, id, _token)): Path<(String, String, String)>,
) -> Response {
    use super::intake;
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((_, parsed)) = resolve(&app, &handle, &id) else {
        return nothing_here();
    };
    intake::page(
        StatusCode::OK,
        intake::shell(
            &parsed.title,
            "<h1>Confirm your booking</h1>\
             <p>One click to make it real — this is what keeps a mail \
             scanner's robot from booking on your behalf. Confirming claims \
             your held time.</p>\
             <form method=\"post\"><button type=\"submit\">Confirm and book</button></form>",
            &format!("/s/{handle}/{id}/"),
        ),
    )
}

/// `POST /s/{handle}/{id}/c/{token}` — the click that books.
///
/// Two spends in sequence, and the order is the design: the verify token
/// first (the stranger proved the address; that is true whatever happens
/// next), then the hold's conversion. When the conversion fails — the hold
/// lapsed before the click, or the slot was re-blocked under it — the
/// just-verified queue row is **deleted**, not left to drain: a drained
/// record is a booking home will put on a calendar, and this one never
/// happened. The stranger gets the truth and the week back.
pub async fn confirm(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, id, token)): Path<(String, String, String)>,
) -> Response {
    use super::intake;
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((user, parsed)) = resolve(&app, &handle, &id) else {
        return nothing_here();
    };

    let hash = crate::intake::hash_token(&token);
    let now = crate::db::now();
    let verified = match app
        .db
        .submission_verify(&user.id, &hash, &now, crate::db::VerifyNext::Queued)
    {
        Ok(Some(row)) => row,
        // One page for expired, spent, and never-existed alike — which it
        // was is not a stranger's business.
        Ok(None) => {
            return intake::page(
                StatusCode::NOT_FOUND,
                intake::shell(
                    "Link expired",
                    &format!(
                        "<h1>This link is no longer valid</h1>\
                         <p>It may have expired or already been used. \
                         <a href=\"/s/{handle}/{id}\">Pick a time</a> to start again.</p>"
                    ),
                    &format!("/s/{handle}/{id}/"),
                ),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "verifying a booking");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    };

    let values: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&verified.payload).unwrap_or_default();
    let Some(booking_id) = values.get("_booking_id").and_then(|v| v.as_str()) else {
        // A queue row with no booking id is not a booking submission; do not
        // guess. The row is removed for the same never-drain reason.
        let _ = app.db.queue_ack(&user.id, &[verified.seq]);
        tracing::error!(seq = verified.seq, "a booking confirm with no _booking_id");
        return nothing_here();
    };

    let manage_token = crate::intake::mint_token();
    let converted = app.db.booking_confirm(
        booking_id,
        &crate::intake::hash_token(&manage_token),
        &now,
    );
    match converted {
        Ok(true) => {}
        Ok(false) => {
            let (_, _) = app.db.queue_ack(&user.id, &[verified.seq]).unwrap_or_default();
            tracing::info!(%booking_id, "a hold lapsed before its click");
            return page_with_notice(
                &app, &user, &parsed, &handle, &id,
                "Your held time lapsed before you confirmed — these are still open.",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "converting a hold");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    }
    // The manage URL is minted here and nowhere else — the token was — so
    // it rides home in the payload with the other machinery keys, and the
    // invite home sends can carry it. Plaintext transits the queue briefly:
    // acceptable for a capability whose whole job is to travel in email.
    {
        let mut values = values.clone();
        values.insert(
            "_manage_url".into(),
            format!(
                "{}/s/{handle}/{id}/m/{manage_token}",
                app.config.base_url(crate::config::Role::Gate)
            )
            .into(),
        );
        if let Ok(payload) = serde_json::to_string(&values) {
            if let Err(e) = app.db.queue_set_payload(&user.id, verified.seq, &payload) {
                // The booking stands; only the cancel link is missing from
                // the invite. Loud, because a quiet miss here is a visitor
                // with no way out.
                tracing::error!(seq = verified.seq, error = %e, "storing the manage url");
            }
        }
    }
    tracing::info!(handle = %user.handle, %id, seq = verified.seq, "booked");

    // The when, in the host's zone, from the booking row — the record, not
    // the payload's copy of it.
    let when = app
        .db
        .booking_get(booking_id)
        .ok()
        .flatten()
        .map(|b| booking_when(&parsed, &b))
        .unwrap_or_default();
    let body = match &parsed.confirmation {
        Some(confirmation) => format!(
            "<h1>{}</h1>\n{when}<p>{}</p>\n",
            mecha_manifest::escape_text(&confirmation.heading),
            mecha_manifest::escape_text(confirmation.render(&values).trim()),
        ),
        None => format!("<h1>You're booked</h1>\n{when}"),
    };
    intake::page(
        StatusCode::OK,
        intake::shell(&parsed.title, &body, &format!("/s/{handle}/{id}/")),
    )
}

/// "Tuesday, Aug 25 · 2:00 pm – 2:30 pm (America/New_York)", or nothing if
/// anything about the row does not parse — a wrong time on a confirmation
/// page is worse than none.
fn booking_when(parsed: &RequestType, booking: &crate::db::BookingRow) -> String {
    let Some(Ok(policy)) = parsed.availability_policy() else {
        return String::new();
    };
    let parse = |raw: &str| {
        chrono::DateTime::parse_from_rfc3339(raw).map(|t| t.with_timezone(&policy.timezone))
    };
    let (Ok(start), Ok(end)) = (parse(&booking.slot_start), parse(&booking.slot_end)) else {
        return String::new();
    };
    format!(
        "<p class=\"intro\"><strong>{} · {} – {}</strong> ({})</p>\n",
        start.format("%A, %b %-d"),
        start.format("%-I:%M %p").to_string().to_lowercase(),
        end.format("%-I:%M %p").to_string().to_lowercase(),
        policy.timezone
    )
}

/// What one manage link may currently do, derived from the row and the
/// clock — the states the incumbents' support forums are full of fumbling,
/// each answered honestly.
enum ManageState {
    /// Cancellable now.
    Active,
    /// Confirmed, but inside the cancellation cutoff.
    InsideCutoff(u32),
    /// The meeting already happened (or started).
    Past,
    /// Already cancelled.
    Cancelled,
}

fn manage_state(
    parsed: &RequestType,
    booking: &crate::db::BookingRow,
    now: chrono::DateTime<chrono::Utc>,
) -> ManageState {
    if booking.state.starts_with("cancelled") {
        return ManageState::Cancelled;
    }
    let start = chrono::DateTime::parse_from_rfc3339(&booking.slot_start)
        .map(|t| t.with_timezone(&chrono::Utc));
    let Ok(start) = start else {
        // An unparseable row cannot be reasoned about; "past" offers no
        // action, which is the safe answer.
        return ManageState::Past;
    };
    if start <= now {
        return ManageState::Past;
    }
    let cutoff = parsed.booking_policy().cancel_cutoff_hours;
    if start - now < chrono::Duration::hours(i64::from(cutoff)) {
        return ManageState::InsideCutoff(cutoff);
    }
    ManageState::Active
}

/// The manage page body for a state. The POST is rendered only when it
/// would work; everything else explains itself and offers the booking page.
fn manage_body(
    parsed: &RequestType,
    booking: &crate::db::BookingRow,
    state: &ManageState,
    handle: &str,
    id: &str,
) -> String {
    let when = booking_when(parsed, booking);
    match state {
        ManageState::Active => format!(
            "<h1>Your booking</h1>\n{when}\
             <p>If your plans have changed, you can cancel this meeting. The \
             time goes back to being bookable, and the calendar event is \
             withdrawn.</p>\
             <form method=\"post\"><button type=\"submit\">Cancel this booking</button></form>"
        ),
        ManageState::InsideCutoff(hours) => format!(
            "<h1>Your booking</h1>\n{when}\
             <p>This meeting starts within {hours} hour(s), which is inside \
             the cancellation window — it can no longer be cancelled here. \
             If you cannot make it, please reply to your confirmation email \
             directly.</p>"
        ),
        ManageState::Past => format!(
            "<h1>This meeting has already happened</h1>\n{when}\
             <p>There is nothing to change. <a href=\"/s/{handle}/{id}\">Book \
             a new time</a> if you would like another.</p>"
        ),
        ManageState::Cancelled => format!(
            "<h1>Already cancelled</h1>\n\
             <p>This booking was cancelled and its time freed. \
             <a href=\"/s/{handle}/{id}\">Book a new time</a> whenever you like.</p>"
        ),
    }
}

/// Resolve a manage token to its booking, and the type it belongs to. The
/// dead-link answer is a branded page with a way forward, never a bare 404 —
/// the one cheap win over every incumbent.
fn resolve_manage(
    app: &Shared,
    handle: &str,
    id: &str,
    token: &str,
) -> Result<(UserRow, RequestType, crate::db::BookingRow), Box<Response>> {
    use super::intake;
    let dead = || {
        intake::page(
            StatusCode::NOT_FOUND,
            intake::shell(
                "Link no longer valid",
                &format!(
                    "<h1>This link is no longer valid</h1>\
                     <p>It may be from an older email. \
                     <a href=\"/s/{handle}/{id}\">Book a time</a> if you need one.</p>"
                ),
                &format!("/s/{handle}/{id}/"),
            ),
        )
    };
    let Some((user, parsed)) = resolve(app, handle, id) else {
        return Err(Box::new(dead()));
    };
    let booking = app
        .db
        .booking_by_manage(&crate::intake::hash_token(token))
        .ok()
        .flatten()
        // The token names one booking; a URL whose path disagrees with the
        // row is somebody splicing tokens onto other people's pages.
        .filter(|b| b.user_id == user.id && b.instrument_id == id);
    match booking {
        Some(booking) => Ok((user, parsed, booking)),
        None => Err(Box::new(dead())),
    }
}

/// `GET /s/{handle}/{id}/m/{token}` — state, honestly, mutating nothing.
/// Scanner-safe by construction: the destructive verb is the POST below.
pub async fn manage_page(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, id, token)): Path<(String, String, String)>,
) -> Response {
    use super::intake;
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let (_, parsed, booking) = match resolve_manage(&app, &handle, &id, &token) {
        Ok(found) => found,
        Err(refusal) => return *refusal,
    };
    let state = manage_state(&parsed, &booking, chrono::Utc::now());
    intake::page(
        StatusCode::OK,
        intake::shell(
            &parsed.title,
            &manage_body(&parsed, &booking, &state, &handle, &id),
            &format!("/s/{handle}/{id}/"),
        ),
    )
}

/// `POST /s/{handle}/{id}/m/{token}` — the cancellation.
///
/// The transition only fires from `confirmed`, so a double-click cancels
/// once; the freed slot is immediate (liveness is judged at query time);
/// and the cancellation record queued for home is machinery-only — home
/// deletes the calendar event through its ledger, and the provider mails
/// the retraction. No model, no prose, nothing composed.
pub async fn manage_cancel(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, id, token)): Path<(String, String, String)>,
) -> Response {
    use super::intake;
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let (user, parsed, booking) = match resolve_manage(&app, &handle, &id, &token) {
        Ok(found) => found,
        Err(refusal) => return *refusal,
    };
    let now = chrono::Utc::now();
    let state = manage_state(&parsed, &booking, now);
    if !matches!(state, ManageState::Active) {
        // The POST arrived where the button would not have rendered — a
        // stale tab, a replay. The state page is the answer, not an error.
        return intake::page(
            StatusCode::OK,
            intake::shell(
                &parsed.title,
                &manage_body(&parsed, &booking, &state, &handle, &id),
                &format!("/s/{handle}/{id}/"),
            ),
        );
    }
    let now_stamp = crate::db::now();
    match app
        .db
        .booking_cancel(&booking.id, "cancelled_by_booker", &now_stamp)
    {
        Ok(true) => {}
        Ok(false) => {
            // Raced by another click; the row will say so.
            return manage_page_redirect(&handle, &id, &token);
        }
        Err(e) => {
            tracing::error!(error = %e, "cancelling a booking");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    }
    // The record home turns into a calendar deletion. Machinery keys only:
    // there is no stranger prose in a cancellation, and the drain side
    // treats an all-machinery payload as valid by construction.
    let payload = serde_json::json!({
        "_booking_id": booking.id,
        "_cancelled": true,
        "_slot_start": booking.slot_start,
        "_slot_end": booking.slot_end,
    })
    .to_string();
    if let Err(e) = app.db.queue_add(
        &user.id,
        &id,
        "queued",
        &payload,
        &now_stamp,
        parsed.retain_days.map(crate::db::days_from_now).as_deref(),
    ) {
        // The slot is freed regardless; the calendar copy is now stale until
        // someone notices. Loud, because this is the one path where the two
        // ledgers can drift.
        tracing::error!(error = %e, booking = %booking.id, "queueing a cancellation");
    }
    tracing::info!(handle = %user.handle, %id, booking = %booking.id, "cancelled by booker");
    intake::page(
        StatusCode::OK,
        intake::shell(
            &parsed.title,
            &format!(
                "<h1>Cancelled</h1>\
                 <p>Your booking is cancelled and the time has been freed. \
                 The calendar invite will be withdrawn shortly. \
                 <a href=\"/s/{handle}/{id}\">Book a new time</a> any time.</p>"
            ),
            &format!("/s/{handle}/{id}/"),
        ),
    )
}

fn manage_page_redirect(handle: &str, id: &str, token: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/s/{handle}/{id}/m/{token}"))],
    )
        .into_response()
}
