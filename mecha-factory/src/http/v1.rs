//! The API, on the gate origin only.
//!
//! Six endpoints, JSON in and out, bearer token. Deliberately boring — the
//! interesting decisions are all in what they refuse.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use super::{Failure, Shared};
use crate::config::Role;
use crate::keys;

/// Read the bearer header, if there is one.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
}

/// Only the gate serves the API. Anywhere else it does not exist.
///
/// Returns the refusal rather than taking a `Result`, so the call site reads
/// as one line and there is no error type large enough for a whole `Response`
/// to travel in.
pub fn not_on_gate(role: Role) -> Option<Response> {
    (role != Role::Gate).then(|| Failure::text(StatusCode::NOT_FOUND, "not found").into_response())
}

/// `GET /v1/health` — is it up, what version, how many queued.
///
/// Public, because the thing that watches it is a trigger that must cost
/// nothing and must work when every key has just been rotated. **The counts are
/// not public**: queue depth is a fact about how many strangers wrote to us this
/// week, and a health check is not a reason to publish it. A caller holding any
/// live key gets the detail.
pub async fn health(
    State(app): State<Shared>,
    Extension(role): Extension<Role>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = not_on_gate(role) {
        return refusal;
    }
    let mut body = serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": app.started.elapsed().as_secs(),
    });

    if let Ok(row) = keys::authenticate_any(&app.db, bearer(&headers)) {
        let bundles = app.db.bundle_count().unwrap_or(-1);
        let queued = app.db.queue_depth().unwrap_or(-1);
        body["bundles"] = bundles.into();
        body["queued"] = queued.into();
        body["key"] = row.id.into();
    }
    (StatusCode::OK, Json(body)).into_response()
}
