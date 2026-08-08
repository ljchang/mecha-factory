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
