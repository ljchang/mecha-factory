//! Serving a published bundle: the share URL, the version URL, and the four
//! ways a request gets nothing.
//!
//! ```text
//!   /b/<id>/            → 302 to the version the alias names   (never cached)
//!   /b/<id>/v/<n>/…     → the bytes, under the class's policy  (cached forever)
//! ```
//!
//! The split is the whole versioning story. A version URL addresses bytes that
//! can never change, so it is `immutable` and a reader's browser may keep it
//! forever. The share URL is the one moving part, so it is never cached — a
//! cached alias is a reader stuck on last Monday's briefing, which is the exact
//! bug aliasing exists to prevent.
//!
//! Four refusals, and the reasoning behind each is the interesting part:
//!
//! - **A private bundle is 404, not 403.** Visibility is enforced here for the
//!   first time (it was recorded and unenforced while there was no origin), and
//!   the failure has to be indistinguishable from "no such bundle" or the
//!   status code answers the question the capability was hiding.
//! - **A taken-down bundle is 410 Gone**, and that difference is deliberate:
//!   the reader followed a link somebody sent them, and "this was here and is
//!   not" is more useful than "broken". It applies to the version URLs too —
//!   taking something down means nothing under that id is served, or a takedown
//!   is a suggestion.
//! - **A bundle whose class belongs to the other origin is redirected there**,
//!   not served. The class decides the origin; an origin that served whatever
//!   it was asked for would make the three policies one policy.
//! - **Anything outside the version directory is 404**, proved by
//!   canonicalisation rather than by inspecting the string.

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use mecha_manifest::Visibility;

use super::{Failure, Shared};
use crate::config::{Origin, Role};
use crate::db::UserRow;

/// Whose artifacts this request is for.
///
/// A bare origin name (`artifacts.example.org` with no label) belongs to
/// nobody and serves nothing — there is no "default user", for the same reason
/// there is no default origin. A **retired** handle resolves to nobody too:
/// what was published under it stops being served rather than being served by
/// whoever holds the name next, which is the entire point of never reusing one.
///
/// A **suspended** user serves nothing either, and it reads exactly like a name
/// that never existed.
fn owner(app: &Shared, origin: &Origin) -> Option<UserRow> {
    let handle = origin.handle.as_deref()?;
    match app.db.user_by_handle(handle) {
        Ok(Some(user)) if user.active() => Some(user),
        Ok(_) => None,
        Err(e) => {
            tracing::error!(handle, error = %e, "resolving a handle");
            None
        }
    }
}

/// What the reader is allowed to see, resolved once so every route agrees.
enum Access {
    /// Serve it, from this version.
    Current(u32),
    /// The alias points at nothing. It was here.
    Gone,
    /// Private, or no such bundle. The same answer, deliberately.
    Nothing,
}

fn access(app: &Shared, user_id: &str, id: &str) -> Access {
    match app.db.alias(user_id, id) {
        // No alias row at all means published-but-never-aliased, which is not
        // yet a publication. Serving it would make `--no-alias` meaningless.
        Ok(None) => Access::Nothing,
        Ok(Some(alias)) => match (alias.version, alias.visibility) {
            (_, Visibility::Private) => Access::Nothing,
            (None, _) => Access::Gone,
            (Some(version), Visibility::Public) => Access::Current(version),
        },
        Err(e) => {
            tracing::error!(%id, error = %e, "reading an alias");
            Access::Nothing
        }
    }
}

fn gone(id: &str) -> Response {
    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
         <title>{}</title></head>\n<body><p>This has been taken down.</p></body></html>\n",
        escape(id)
    );
    (
        StatusCode::GONE,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

fn missing() -> Response {
    Failure::text(StatusCode::NOT_FOUND, "not found").into_response()
}

/// `/b/<id>/` — the stable share URL, which follows the alias.
pub async fn share(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(id): Path<String>,
) -> Response {
    if origin.role == Role::Gate || mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return missing();
    }
    let Some(user) = owner(&app, &origin) else {
        return missing();
    };
    match access(&app, &user.id, &id) {
        Access::Nothing => missing(),
        Access::Gone => gone(&id),
        Access::Current(version) => {
            // 302 and never 301: the alias moves, and a permanently cached
            // redirect would pin a reader to whichever version they saw first.
            (
                StatusCode::FOUND,
                [
                    (header::LOCATION, format!("/b/{id}/v/{version}/")),
                    (header::CACHE_CONTROL, "no-store".into()),
                ],
            )
                .into_response()
        }
    }
}

/// `/b/<id>/v/` — the version switcher: every version of a publicly served
/// bundle, each at its immutable URL, with the live one named.
///
/// A factory-owned page at a path the version scheme already reserves, so it
/// can never collide with a file inside a bundle — and deliberately *not* a
/// banner injected into the bundles themselves: published bytes are
/// content-addressed, and what was verified locally must be what the world
/// sees, byte for byte. Same access rule as the bytes: private and unknown
/// answer identically, a takedown covers this page too.
pub async fn versions_index(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(id): Path<String>,
) -> Response {
    if origin.role == Role::Gate || mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return missing();
    }
    let Some(user) = owner(&app, &origin) else {
        return missing();
    };
    let live = match access(&app, &user.id, &id) {
        Access::Nothing => return missing(),
        Access::Gone => return gone(&id),
        Access::Current(version) => version,
    };
    let versions = app.db.bundle_versions(&user.id, &id).unwrap_or_default();
    let mut items = String::new();
    for v in &versions {
        items.push_str(&format!(
            "<li><a href=\"/b/{id}/v/{v}/\">v{v}</a>{}</li>\n",
            if *v == live {
                " — what the share URL shows"
            } else {
                ""
            },
        ));
    }
    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{id} — versions</title></head>\n\
         <body><h1><code>{id}</code></h1>\
         <p>Every published version is immutable and stays reachable; \
         <a href=\"/b/{id}/\">the share URL</a> follows the alias.</p>\
         <ul>{items}</ul></body></html>\n",
        id = escape(&id),
    );
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // The list changes when a version is published or the alias
            // moves, so it is never cached the way the versions themselves
            // are.
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// `/b/<id>/v/<n>/` — a version's index.
pub async fn version_root(
    state: State<Shared>,
    origin: Extension<Origin>,
    Path((id, version)): Path<(String, u32)>,
) -> Response {
    serve(state, origin, id, version, String::new()).await
}

/// `/b/<id>/v/<n>/<path>` — a file inside a version.
pub async fn version_file(
    state: State<Shared>,
    origin: Extension<Origin>,
    Path((id, version, path)): Path<(String, u32, String)>,
) -> Response {
    serve(state, origin, id, version, path).await
}

async fn serve(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    id: String,
    version: u32,
    path: String,
) -> Response {
    if origin.role == Role::Gate || mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return missing();
    }
    let Some(user) = owner(&app, &origin) else {
        return missing();
    };
    match access(&app, &user.id, &id) {
        Access::Nothing => return missing(),
        // A takedown covers the version URLs too, or it is a suggestion.
        Access::Gone => return gone(&id),
        Access::Current(_) => {}
    }

    let row = match app.db.bundle(&user.id, &id, version) {
        Ok(Some(row)) => row,
        Ok(None) => return missing(),
        Err(e) => {
            tracing::error!(%id, version, error = %e, "reading a bundle row");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };

    // Withheld on a report: served to nobody, and indistinguishable from a
    // bundle that never existed. The bytes are still on disk — withholding is
    // reversible and destroying evidence in response to a complaint is not.
    if row.withheld_at.is_some() {
        return missing();
    }

    // The class decides the origin, and this is where that is enforced. A
    // reader who followed a compute bundle's link to the artifact origin is
    // sent to the right one rather than served under the wrong policy.
    let belongs = Role::for_class(row.class);
    if belongs != origin.role {
        let target = format!(
            "{}/b/{id}/v/{version}/{}",
            app.config.user_url(belongs, &user.handle),
            path.trim_start_matches('/')
        );
        return (
            StatusCode::FOUND,
            [
                (header::LOCATION, target),
                (header::CACHE_CONTROL, "no-store".into()),
            ],
        )
            .into_response();
    }

    let Some(file) = app.files.resolve(&user.id, &id, version, &path) else {
        return missing();
    };
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(path = %file.display(), error = %e, "reading a published file");
            return missing();
        }
    };

    let mut response = (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            mecha_manifest::content_type(&file.to_string_lossy()),
        )],
        bytes,
    )
        .into_response();

    let headers = response.headers_mut();
    // The policy the class declares — the same table the local preview server
    // serves under, so a bundle verified at home is verified against this.
    for (name, value) in row.class.headers() {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    // A version's bytes can never change, which is the one thing that makes an
    // aggressive cache correct rather than a hazard.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
