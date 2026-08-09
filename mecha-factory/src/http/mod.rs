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

pub mod account;
pub mod admin;
pub mod artifacts;
pub mod booking;
pub mod intake;
pub mod poll;
pub mod signup;
pub mod slides;
pub mod v1;
pub mod viewer;

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
    /// Uploaded form attachments, keyed by minted id. See `crate::attachments`.
    pub attachments: crate::attachments::Store,
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
        let attachments = crate::attachments::Store::new(config.attachments_root())?;
        Ok(App {
            api_limit: RateLimiter::new(config.limits.rate_per_minute),
            asset_limit: RateLimiter::new(config.limits.rate_per_minute.saturating_mul(10)),
            files,
            attachments,
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
        .route(
            "/v1/instruments/{id}/slots",
            axum::routing::put(v1::put_slots),
        )
        .route(
            "/v1/instruments/{id}/polls/{poll_id}",
            axum::routing::put(v1::put_poll).get(v1::get_poll),
        )
        .route(
            "/v1/instruments/{id}/polls/{poll_id}/close",
            post(v1::close_poll),
        )
        .route("/v1/queue", get(v1::drain))
        .route("/v1/queue/ack", post(v1::ack))
        .route("/v1/queue/attachments/{id}", get(v1::attachment))
        // Spending a pairing code. Unauthenticated — the code is the
        // credential — and rate-limited with the rest of the gate.
        .route("/v1/pair", post(v1::pair))
        // A credential retiring itself. Authenticated by the key being
        // revoked — the one thing it can still authorise.
        .route("/v1/disconnect", post(v1::disconnect))
        // The operator's endpoints — Scope::Operate only, which no tenant
        // key carries. This is what retires SSH from routine operation.
        .route("/v1/admin/users", get(v1::admin_users))
        .route(
            "/v1/admin/users/{handle}/status",
            post(v1::admin_user_status),
        )
        .route(
            "/v1/admin/invites",
            get(v1::admin_invites).post(v1::admin_invite_create),
        )
        .route(
            "/v1/admin/invites/{id}/revoke",
            post(v1::admin_invite_revoke),
        )
        .route("/v1/admin/keys", get(v1::admin_keys))
        .route("/v1/admin/keys/{id}/revoke", post(v1::admin_key_revoke))
        .route("/v1/admin/withhold", post(v1::admin_withhold))
        // The operate key asking for a browser: the one bridge between the
        // key surface and the operator's session surface below.
        .route("/v1/admin/signin", post(admin::signin_link))
        // The operator's panel — its own cookie, its own tables, and two
        // ways in: the link the CLI minted, and the email door when an
        // operator address is configured. See http/admin.rs.
        .route("/admin", get(admin::overview))
        .route("/admin/signin", post(admin::email_signin))
        .route(
            "/admin/s/{token}",
            get(admin::finish_page).post(admin::finish),
        )
        .route("/admin/signout", post(admin::signout))
        .route("/admin/status", post(admin::status))
        .route("/admin/invite", post(admin::invite))
        .route("/admin/invite-revoke", post(admin::invite_revoke))
        .route("/admin/key-revoke", post(admin::key_revoke))
        .route("/admin/withhold", post(admin::withhold))
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
        // The booking page: the same gate, the same no-script posture as the
        // forms — server-rendered HTML whose scripts are same-origin files.
        .route(
            "/s/{handle}/{type_id}",
            get(booking::page).post(booking::submit),
        )
        // The live half of the page: what is open right now, as data, for
        // the open tab that polls it. A static segment, so it wins over the
        // `{name}` asset route below.
        .route("/s/{handle}/{type_id}/slots.json", get(booking::slots_json))
        .route("/s/{handle}/{type_id}/{name}", get(booking::asset))
        // GET is a button and POST is the booking, for the same
        // scanner reason as the form's confirm route.
        .route(
            "/s/{handle}/{type_id}/c/{token}",
            get(booking::confirm_page).post(booking::confirm),
        )
        // The cancel capability from the invite. GET states, POST cancels —
        // scanners follow GETs.
        .route(
            "/s/{handle}/{type_id}/m/{token}",
            get(booking::manage_page).post(booking::manage_cancel),
        )
        // The group poll: a capability URL per participant, answers only in
        // the vocabulary the poll itself declared. The results route is
        // token-scoped like the page, because what results a viewer may see
        // depends on who is asking — `after_vote` is a personal reveal.
        .route("/p/a/{name}", get(poll::asset))
        .route(
            "/p/{handle}/{poll_id}/{token}",
            get(poll::page).post(poll::answer),
        )
        .route(
            "/p/{handle}/{poll_id}/{token}/results.json",
            get(poll::results),
        )
        // The link audience's shared door: no token in the path — the
        // browser's cookie is the ballot capability, minted at first save.
        .route(
            "/p/{handle}/{poll_id}",
            get(poll::link_page).post(poll::link_answer),
        )
        .route(
            "/p/{handle}/{poll_id}/results.json",
            get(poll::link_results),
        )
        // The projector: a creator capability, aggregates only, the word
        // cloud standing in for anyone's sentence.
        .route(
            "/p/{handle}/{poll_id}/screen/{token}",
            get(poll::screen_page),
        )
        .route(
            "/p/{handle}/{poll_id}/screen/{token}/data.json",
            get(poll::screen_data),
        )
        // The PowerPoint content add-in's wrapper: the page the sideloaded
        // manifest points at. Holds no data — the pasted projector URL is
        // the capability, and it lives in the deck.
        .route("/slides/addin", get(slides::page))
        .route("/slides/addin.js", get(slides::script))
        // GET is a page with a button and POST is the verification, because
        // mail scanners follow GETs — see `intake::confirm_page`.
        .route(
            "/f/{handle}/{type_id}/c/{token}",
            get(intake::confirm_page).post(intake::confirm),
        )
        // The upload step, for types with file fields. The only multipart
        // route in the binary, and the second route to raise the 2 MB body
        // default — to the configured submission ceiling, with the type's own
        // attachment budget enforced beneath it in the handler.
        .route(
            "/f/{handle}/{type_id}/u/{token}",
            get(intake::upload_page).post(intake::upload).layer(
                axum::extract::DefaultBodyLimit::max(
                    app.config.limits.max_submission_bytes as usize,
                ),
            ),
        )
        // Claiming a handle from an invite. Gate-only like the forms, and for
        // the same reason: server-rendered HTML that executes nothing.
        .route("/signup/{token}", get(signup::form).post(signup::submit))
        .route("/signup/{token}/{name}", get(signup::asset))
        // The tenant surface: a signed-in page carrying release authority —
        // the second door to Scope::Release the scope split named. Gate-only,
        // server-rendered, no script.
        .route("/account", get(account::overview))
        .route("/account/signin", post(account::signin))
        .route(
            "/account/s/{token}",
            get(account::finish_page).post(account::finish),
        )
        .route("/account/signout", post(account::signout))
        .route("/account/release", post(account::release))
        .route("/account/revoke", post(account::revoke))
        .route("/account/pair", post(account::pair))
        // Sharing: the owner's grants, driven from the viewer's Manage menu
        // on the same session and CSRF as every other account verb.
        .route("/account/share", post(account::share))
        .route("/account/share-revoke", post(account::share_revoke))
        .route("/account/a/{name}", get(account::asset))
        .route("/b/{id}", get(artifacts::share))
        .route("/b/{id}/", get(artifacts::share))
        // The version switcher sits at the bare `v/`, a path the version
        // scheme reserves and no bundle file can occupy.
        .route("/b/{id}/v", get(artifacts::versions_index))
        .route("/b/{id}/v/", get(artifacts::versions_index))
        // The signed-in viewer lives on the GATE — where the session is —
        // and frames the bundle cross-origin; see http/viewer.rs for the
        // inversion. `/view/{a}/{b}` is two pages behind one shape: the
        // artifact-origin redirect, and the gate's bare viewer URL a share
        // mail carries — the dispatcher branches on origin.
        .route("/view/{handle}/{id}/{version}", get(viewer::view))
        .route("/view/{a}/{b}", get(viewer::two_seg))
        // The reader's way in: an email proves its inbox and becomes the
        // third session surface. See http/viewer.rs.
        .route("/view/signin", post(viewer::signin))
        .route(
            "/view/r/{token}",
            get(viewer::finish_page).post(viewer::finish),
        )
        .route("/view/signout", post(viewer::signout))
        .route("/b/{id}/v/{version}", get(artifacts::version_root))
        .route("/b/{id}/v/{version}/", get(artifacts::version_root))
        .route("/b/{id}/v/{version}/{*path}", get(artifacts::version_file))
        // The capability path: one version's bytes for whoever presents a
        // token the gate minted. Artifact origins only; see http/artifacts.rs.
        .route("/g/{cap}", get(artifacts::grant_bare))
        .route("/g/{cap}/", get(artifacts::grant_root))
        .route("/g/{cap}/{*path}", get(artifacts::grant_file))
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
        // A configured redirect host — the apex, `www` — sends the browser to
        // the gate and keeps the path: `mecha-factory.ai/account` should land
        // on the account page, not the splash.
        let bare = host.split(':').next().unwrap_or(&host).to_ascii_lowercase();
        if app
            .config
            .redirect_hosts
            .iter()
            .any(|h| h.to_ascii_lowercase() == bare)
        {
            let target = format!(
                "{}{}",
                app.config.base_url(Role::Gate),
                request.uri().path()
            );
            return (
                StatusCode::MOVED_PERMANENTLY,
                [(axum::http::header::LOCATION, target)],
            )
                .into_response();
        }
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
async fn root(
    State(app): State<Shared>,
    origin: Extension<Origin>,
    headers: axum::http::HeaderMap,
) -> Response {
    if origin.role != Role::Gate {
        return Failure::text(StatusCode::NOT_FOUND, "not found").into_response();
    }
    // The splash knows who is looking: a session gets its own dropdown, and
    // everyone else gets the way in — far right, as a front page should.
    let chrome = match account::session(&app, &headers) {
        Some((token, user)) => intake::Chrome::Account {
            handle: user.handle.clone(),
            email: user.email.clone(),
            csrf: account::csrf(&token),
            docs_url: app.config.docs_url.clone(),
        },
        None => intake::Chrome::Public {
            docs_url: app.config.docs_url.clone(),
            sign_in: true,
        },
    };
    // The splash: what this machine is, and where everything else lives. The
    // GitHub links are ordinary anchors — navigation, which no CSP directive
    // governs — so the no-external-references rule for *resources* holds
    // untouched.
    let docs = match &app.config.docs_url {
        Some(url) => format!(
            "<li><a href=\"{}\">Documentation</a> — how to run one of these.</li>",
            mecha_manifest::escape_text(url)
        ),
        None => String::new(),
    };
    let body = format!(
        "<h1>mecha factory</h1>\
         <p class=\"intro\">The public surface for <strong>mecha</strong>, an \
         agent harness: a place for what an agent makes to live, and a typed \
         way for the outside world to get in.</p>\
         <p>Artifacts published here are durable, versioned and permissioned — \
         a report an agent rendered, released by a person. Requests arrive \
         through typed forms, verified by email, and reach an agent's owner \
         with the free text quarantined. Nothing on this box holds a \
         credential that reaches anyone's home machine.</p>\
         <ul>\
         <li><a href=\"{repo}\">mecha-factory on GitHub</a> — this server, \
         the publisher, and the manifest contract.</li>\
         <li><a href=\"https://github.com/ljchang/mecha\">mecha on GitHub</a> \
         — the agent harness this is the public surface of.</li>\
         {docs}\
         <li><a href=\"/account\">Your page</a> — if one of the handles here \
         is yours.</li>\
         </ul>",
        repo = env!("CARGO_PKG_REPOSITORY"),
    );
    intake::page(
        StatusCode::OK,
        intake::shell_with("mecha factory", &body, "/account/a/", &chrome),
    )
}

async fn fallback(request: Request) -> Response {
    if request.uri().path().starts_with("/v1/") {
        Failure::json(StatusCode::NOT_FOUND, "no such endpoint").into_response()
    } else {
        Failure::text(StatusCode::NOT_FOUND, "not found").into_response()
    }
}

pub use axum::Extension;

/// A session cookie wearing its armour, single-sourced. The attributes are
/// the security — the tests assert them one by one — so no surface
/// hand-writes the list: a copy that dropped `HttpOnly` or `SameSite` would
/// ship a weaker cookie with no compiler help. Max-Age 0 is the deletion
/// spelling, with an empty value.
pub(crate) fn session_cookie(name: &str, value: &str, max_age_secs: i64) -> String {
    format!("{name}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age_secs}")
}

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
