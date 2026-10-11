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

/// `/view/<id>/<version>` on an artifact origin — a redirect to the real
/// viewer, which lives on the gate where the session is. One viewer, not
/// two: the chrome that knows who you are cannot run here, and a second
/// anonymous chrome would drift from the first. Called through the gate
/// viewer's route dispatcher, which owns the `/view/{a}/{b}` shape.
pub(crate) fn viewer_redirect(app: &Shared, origin: &Origin, id: &str, version: u32) -> Response {
    if origin.role == Role::Gate || mecha_manifest::valid_id(id, "bundle id").is_err() {
        return missing();
    }
    let Some(user) = owner(app, origin) else {
        return missing();
    };
    // Same one-answer rule before revealing even the redirect: a private or
    // unknown bundle's viewer URL answers like nothing was ever here.
    match access(app, &user.id, id) {
        Access::Nothing => return missing(),
        Access::Gone => return gone(id),
        Access::Current(_) => {}
    }
    let target = format!(
        "{}/view/{}/{id}/{version}",
        app.config.base_url(crate::config::Role::Gate),
        user.handle,
    );
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, target),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
    )
        .into_response()
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
            "<li><a href=\"/view/{id}/{v}\">v{v}</a> \
             (<a href=\"/b/{id}/v/{v}/\">bare</a>){}</li>\n",
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
    // The gate's signed-in viewer may frame a bundle; nothing else new may.
    // The local preview server keeps plain `headers()` — it has no gate.
    for (name, value) in row
        .class
        .headers_framed_by(Some(&app.config.base_url(crate::config::Role::Gate)))
    {
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

// ---- dashboard datasets ---------------------------------------------------
//
// A dashboard's rows live beside its versions, never inside one (a version is
// immutable and content-addressed), so they have their own path: `/b/<id>/data/
// <name>.json` for a public page, and `/g/<cap>/data/<name>.json` under a grant
// — the grant covers the data of the bundle it opens (R16 of the dashboard
// design), not only the version's files. Either way the rows served are the
// ones pushed under the **current** release: an older version, still
// addressable, reads today's data, and a panel bound to a column that changed
// fails in place rather than showing stale numbers.
//
// The two paths are not relative to each other: under a grant the rows are a
// sibling of the page (`/g/<cap>/` → `data/x.json`), but a public page lives
// at `/b/<id>/v/<n>/` and its rows at `/b/<id>/data/` — `./data/` there would
// resolve inside the version. The renderer computes the URL from the path's
// shape (mecha's `web/src/lib/hud/source.js`).

/// What a request under a dashboard's `data/` came to.
enum DataAnswer {
    /// A response to send: the rows, a 304, a 404 or a 500.
    Answer(Response),
    /// Not a dataset path of a dashboard — a version that declares no
    /// datasets keeps every path to its own files, `data/` included.
    NotData,
}

fn unavailable() -> Response {
    Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
}

/// The dataset `file` (`<name>.json`) of dashboard `id` at `release`, for a
/// reader already allowed to see `version`'s bytes, on `role`'s origin.
///
/// - **No such dataset, nothing pushed under this release yet, withheld: one
///   404**, so the answer says nothing about which. A ledger that cannot be
///   read is a 500 — never "no rows" on a path a page polls (review of
///   mecha-factory#24).
/// - **The class decides the origin, for the rows as for the page.** A
///   dataset is served only on the origin its bundle's class belongs on, so
///   one dataset never answers under two policies.
/// - **A withhold covers the rows** of the release they belong to, so a grant
///   minted for an older version cannot read what a withheld release pushed.
#[allow(clippy::too_many_arguments)]
fn dataset_response(
    app: &Shared,
    role: Role,
    user_id: &str,
    id: &str,
    version: u32,
    release: u32,
    file: &str,
    seen: Option<&str>,
    cache: &'static str,
) -> DataAnswer {
    let Some(name) = file.strip_suffix(".json") else {
        return DataAnswer::NotData;
    };
    if !mecha_manifest::hud::valid_dataset_name(name) {
        return DataAnswer::NotData;
    }
    let bundle = match app.db.bundle(user_id, id, version) {
        Ok(Some(bundle)) => bundle,
        Ok(None) => return DataAnswer::NotData,
        Err(e) => {
            tracing::error!(%id, version, error = %e, "reading a bundle row");
            return DataAnswer::Answer(unavailable());
        }
    };
    if app
        .files
        .resolve(user_id, id, version, mecha_manifest::hud::DATASETS_FILE)
        .is_none()
    {
        return DataAnswer::NotData;
    }
    if bundle.withheld_at.is_some() || Role::for_class(bundle.class) != role {
        return DataAnswer::Answer(missing());
    }
    if release != version {
        match app.db.bundle(user_id, id, release) {
            Ok(Some(current)) if current.withheld_at.is_none() => {}
            Ok(_) => return DataAnswer::Answer(missing()),
            Err(e) => {
                tracing::error!(%id, release, error = %e, "reading a bundle row");
                return DataAnswer::Answer(unavailable());
            }
        }
    }
    let row = match app.db.dataset_get(user_id, id, name) {
        Ok(Some(row)) if row.release == release => row,
        Ok(_) => return DataAnswer::Answer(missing()),
        Err(e) => {
            tracing::error!(%id, %name, error = %e, "reading a dataset");
            return DataAnswer::Answer(unavailable());
        }
    };
    let tag = format!("\"{}\"", row.digest);
    let mut response = if seen.is_some_and(|s| s.split(',').any(|t| t.trim() == tag)) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            row.body,
        )
            .into_response()
    };
    let headers = response.headers_mut();
    for (name, value) in bundle
        .class
        .headers_framed_by(Some(&app.config.base_url(crate::config::Role::Gate)))
    {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&tag) {
        headers.insert(header::ETAG, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    DataAnswer::Answer(response)
}

/// `/b/<id>/data/<name>.json` — a public dashboard's dataset, under the
/// current release, revalidated by `ETag`.
pub async fn data_file(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((id, file)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    if origin.role == Role::Gate || mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return missing();
    }
    let Some(user) = owner(&app, &origin) else {
        return missing();
    };
    // `access`'s answers, with a ledger error kept apart: it is a 500 on a
    // path a page polls, never "no rows" (review of #24).
    let release = match app.db.alias(&user.id, &id) {
        Ok(None) => return missing(),
        Ok(Some(alias)) => match (alias.version, alias.visibility) {
            (_, Visibility::Private) => return missing(),
            (None, _) => return gone(&id),
            (Some(version), Visibility::Public) => version,
        },
        Err(e) => {
            tracing::error!(%id, error = %e, "reading an alias");
            return unavailable();
        }
    };
    let seen = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    // Rows change with every refresh: revalidate every time, by ETag.
    match dataset_response(
        &app,
        origin.role,
        &user.id,
        &id,
        release,
        release,
        &file,
        seen,
        "public, no-cache",
    ) {
        DataAnswer::Answer(response) => response,
        DataAnswer::NotData => missing(),
    }
}

// ---- the capability path -------------------------------------------------
//
// `/g/<cap>/…` serves one version's bytes to whoever presents the token —
// which is exactly as designed: the gate authorised a *person* (the owner,
// or an email a share names) and minted this short-lived spelling of that
// decision, because this origin holds no session and must learn no identity.
// The alias and its visibility are deliberately not consulted — the
// capability is the authority — but a withhold still covers everything, and
// a reader's capability re-proves its share inside `view_cap_lookup`, so a
// revoked grant dies at the next fetch, not at the token's expiry.

/// `/g/<cap>` — normalise onto the slash, so the bundle's relative paths
/// resolve under the capability.
pub async fn grant_bare(Extension(origin): Extension<Origin>, Path(cap): Path<String>) -> Response {
    if origin.role == Role::Gate || !cap.chars().all(|c| c.is_ascii_hexdigit()) {
        return missing();
    }
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, format!("/g/{cap}/")),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
    )
        .into_response()
}

/// `/g/<cap>/` — the version's index.
pub async fn grant_root(
    state: State<Shared>,
    origin: Extension<Origin>,
    Path(cap): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    serve_cap(state, origin, cap, String::new(), headers).await
}

/// `/g/<cap>/<path>` — a file inside the version.
pub async fn grant_file(
    state: State<Shared>,
    origin: Extension<Origin>,
    Path((cap, path)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    serve_cap(state, origin, cap, path, headers).await
}

async fn serve_cap(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    cap: String,
    path: String,
    headers: axum::http::HeaderMap,
) -> Response {
    if origin.role == Role::Gate {
        return missing();
    }
    let Some(user) = owner(&app, &origin) else {
        return missing();
    };
    let row = match app
        .db
        .view_cap_lookup(&crate::intake::hash_token(&cap), &crate::db::now())
    {
        Ok(Some(row)) => row,
        Ok(None) => return missing(),
        Err(e) => {
            tracing::error!(error = %e, "reading a view capability");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    // A capability only opens bytes under their owner's own origins — one
    // tenant's token must never make another tenant's subdomain serve.
    if row.user_id != user.id {
        return missing();
    }

    let bundle = match app.db.bundle(&user.id, &row.bundle_id, row.version) {
        Ok(Some(bundle)) => bundle,
        Ok(None) => return missing(),
        Err(e) => {
            tracing::error!(error = %e, "reading a bundle row");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    // A withhold covers capabilities too, or it is a suggestion.
    if bundle.withheld_at.is_some() {
        return missing();
    }
    // The class decides the origin here exactly as on the public path.
    let belongs = Role::for_class(bundle.class);
    if belongs != origin.role {
        let target = format!(
            "{}/g/{cap}/{}",
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

    // A dashboard's data, under the grant that opened its page (R16): the rows
    // of the release the alias is at now, private like the page.
    if let Some(file) = path.strip_prefix("data/") {
        let current = match app.db.alias(&user.id, &row.bundle_id) {
            Ok(alias) => alias.and_then(|a| a.version),
            Err(e) => {
                tracing::error!(error = %e, "reading an alias");
                return unavailable();
            }
        };
        if let Some(release) = current {
            let seen = headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok());
            if let DataAnswer::Answer(response) = dataset_response(
                &app,
                origin.role,
                &user.id,
                &row.bundle_id,
                row.version,
                release,
                file,
                seen,
                "private, no-cache",
            ) {
                return response;
            }
        }
    }

    let Some(file) = app
        .files
        .resolve(&user.id, &row.bundle_id, row.version, &path)
    else {
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
    for (name, value) in bundle
        .class
        .headers_framed_by(Some(&app.config.base_url(crate::config::Role::Gate)))
    {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    // Private bytes, unlike a public version's: nothing here may rest in a
    // shared cache, however immutable the underlying version is.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
