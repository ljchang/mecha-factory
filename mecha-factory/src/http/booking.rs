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
    let cache = app.db.slots_get(&user.id, &id).ok().flatten();
    let (slots, stale_notice) = match &cache {
        Some(row) => {
            let slots: Vec<mecha_manifest::availability::Slot> =
                serde_json::from_str(&row.slots).unwrap_or_default();
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
