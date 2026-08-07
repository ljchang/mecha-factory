//! The router, and the three rules every response obeys.
//!
//! **1. The origin is resolved before anything is routed.** A request whose
//! `Host` is not one of the three configured names is answered with 404 and
//! never reaches a handler. There is no default origin, because the default
//! would be whichever policy is listed first — and the direction that fails is
//! a static report served under the compute policy, silently gaining
//! `wasm-unsafe-eval`.
//!
//! **2. A path exists on exactly one kind of origin.** `/v1/*` is the gate;
//! `/b/*` is artifacts and compute. The wrong origin gets 404 rather than 403:
//! the resource genuinely does not exist there, and saying "forbidden" would
//! confirm it exists somewhere.
//!
//! **3. Every response carries a policy.** Bundle responses carry the headers
//! their content class declares; everything else — JSON, errors, the gate's own
//! pages — carries the `static` policy, under which nothing executes. Both come
//! from [`mecha_manifest::ContentClass`], so there is one definition of what
//! this server permits and it is the one the local preview server uses too.

pub mod artifacts;
pub mod intake;
pub mod signup;
pub mod v1;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use mecha_manifest::ContentClass;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use crate::bundles::Files;
use crate::config::{Config, Origin, Role};
use crate::db::Db;
use crate::ratelimit::RateLimiter;

pub struct App {
    pub db: Db,
    pub config: Config,
    pub files: Files,
    /// Everything a stranger can reach without a key.
    pub api_limit: RateLimiter,
    /// Static reads, which are the same reader many times: one page of a
    /// notebook is fifty files, so this bucket is an order of magnitude
    /// larger. Two limiters rather than one, because a single number that
    /// suits both is a number that stops a reader or lets a scraper walk the
    /// whole URL space.
    pub asset_limit: RateLimiter,
    /// Where a verification link goes. An interface rather than a feature —
    /// see `crate::intake`.
    pub mailer: Box<dyn crate::intake::Mailer>,
    pub started: Instant,
}

pub type Shared = Arc<App>;

impl App {
    /// Delivery comes from the configuration, and a `[mail]` section that
    /// cannot be honoured stops the box here rather than degrading to logging.
    pub fn new(config: Config, db: Db) -> anyhow::Result<App> {
        let mailer = crate::mail::configured(&config)?;
        App::with_mailer(config, db, mailer)
    }

    /// The same thing with delivery supplied — which is how a test reads the
    /// link a stranger would have been sent, and how a real deployment hands
    /// in a sender without this crate learning what SES is.
    pub fn with_mailer(
        config: Config,
        db: Db,
        mailer: Box<dyn crate::intake::Mailer>,
    ) -> anyhow::Result<App> {
        let files = Files::new(config.bundle_root())?;
        Ok(App {
            api_limit: RateLimiter::new(config.limits.rate_per_minute),
            asset_limit: RateLimiter::new(config.limits.rate_per_minute.saturating_mul(10)),
            files,
            config,
            db,
            mailer,
            started: Instant::now(),
        })
    }
}

/// Everything the server answers.
pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/v1/health", get(v1::health))
        .route("/v1/types", get(v1::list_types))
        .route("/v1/types/{id}", get(v1::get_type).put(v1::put_type))
        .route(
            "/v1/bundles",
            // The body limit is the configured bundle cap rather than axum's
            // default 2 MB, which a vendored Pyodide tree passes on the way out
            // the door. It is set on this route alone: every other endpoint
            // takes a small JSON body and has no reason to accept more.
            post(v1::publish).layer(axum::extract::DefaultBodyLimit::max(
                app.config.limits.max_bundle_bytes as usize,
            )),
        )
        .route("/v1/bundles/{id}/alias", post(v1::alias))
        .route("/v1/queue", get(v1::drain))
        .route("/v1/queue/ack", post(v1::ack))
        // Spending a pairing code. Unauthenticated — the code is the
        // credential — and rate-limited with the rest of the gate.
        .route("/v1/pair", post(v1::pair))
        // The typed way in. On the gate, path-scoped by handle rather than
        // subdomain-scoped: a form is server-rendered HTML with no script, so
        // it executes nothing and there is nothing for an origin to separate
        // (§14.3). The artifact origins are a different story, and that is why
        // they are different origins.
        .route(
            "/f/{handle}/{type_id}",
            get(intake::form).post(intake::submit),
        )
        .route("/f/{handle}/{type_id}/{name}", get(intake::asset))
        .route("/f/{handle}/{type_id}/c/{token}", get(intake::confirm))
        // Claiming a handle from an invite. Gate-only like the forms, and for
        // the same reason: server-rendered HTML that executes nothing.
        .route("/signup/{token}", get(signup::form).post(signup::submit))
        .route("/signup/{token}/{name}", get(signup::asset))
        .route("/b/{id}", get(artifacts::share))
        .route("/b/{id}/", get(artifacts::share))
        .route("/b/{id}/v/{version}", get(artifacts::version_root))
        .route("/b/{id}/v/{version}/", get(artifacts::version_root))
        .route("/b/{id}/v/{version}/{*path}", get(artifacts::version_file))
        .fallback(fallback)
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// The origin table, the rate limit, and the default policy — in that order,
/// before any handler runs.
async fn guard(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let Some(origin) = app.config.origins.role_of(&host) else {
        // Not "misdirected request": a name we do not serve is told nothing
        // about what we do serve.
        tracing::debug!(%host, "request for an unserved name");
        return Failure::text(StatusCode::NOT_FOUND, "not found").into_response();
    };

    let path = request.uri().path().to_string();
    let limiter = if path.starts_with("/b/") {
        &app.asset_limit
    } else {
        &app.api_limit
    };
    if !limiter.allow(peer.ip()) {
        tracing::warn!(peer = %peer.ip(), %path, "rate limited");
        return Failure::text(StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

    let role = origin.role;
    let mut request = request;
    request.extensions_mut().insert(origin);
    let mut response = next.run(request).await;

    // The default policy, applied only where a handler did not already declare
    // one. A bundle response carries its class's headers; an artifact origin
    // falls back to `static`, under which nothing executes.
    //
    // The gate is the exception, and it was a real breakage rather than a
    // nicety: `static` carries `form-action 'none'`, so a browser silently
    // refused to submit the one kind of page this origin exists to serve.
    let defaults = if role == Role::Gate {
        mecha_manifest::gate_headers()
    } else {
        ContentClass::Static.headers()
    };
    let headers = response.headers_mut();
    for (name, value) in defaults {
        if !headers.contains_key(name) {
            if let Ok(value) = HeaderValue::from_str(&value) {
                headers.insert(name, value);
            }
        }
    }
    response
}

/// The gate's own front page. Deliberately almost nothing: this origin exists
/// to serve an API and, later, forms — not to say who runs it.
async fn root(origin: Extension<Origin>) -> Response {
    if origin.role != Role::Gate {
        return Failure::text(StatusCode::NOT_FOUND, "not found").into_response();
    }
    let body = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
                <title>factory</title></head>\n<body><p>This is a mecha factory. \
                Nothing here is for browsing.</p></body></html>\n";
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

async fn fallback(request: Request) -> Response {
    if request.uri().path().starts_with("/v1/") {
        Failure::json(StatusCode::NOT_FOUND, "no such endpoint").into_response()
    } else {
        Failure::text(StatusCode::NOT_FOUND, "not found").into_response()
    }
}

pub use axum::Extension;

/// A refusal, in the shape the caller can read.
///
/// JSON on the API because a program is reading it; text everywhere else
/// because a person is. Neither ever says more than it must: a 404 for a
/// private bundle and a 404 for a bundle that never existed are the same
/// bytes, or the difference between them is the answer to "does this exist".
pub struct Failure {
    status: StatusCode,
    body: String,
    json: bool,
}

impl Failure {
    pub fn json(status: StatusCode, message: impl Into<String>) -> Failure {
        Failure {
            status,
            body: message.into(),
            json: true,
        }
    }

    pub fn text(status: StatusCode, message: impl Into<String>) -> Failure {
        Failure {
            status,
            body: message.into(),
            json: false,
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (content_type, body) = if self.json {
            (
                "application/json",
                serde_json::json!({ "error": self.body }).to_string(),
            )
        } else {
            ("text/plain; charset=utf-8", self.body)
        };
        (
            self.status,
            [(axum::http::header::CONTENT_TYPE, content_type)],
            body,
        )
            .into_response()
    }
}
