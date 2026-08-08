//! The tenant surface: a signed-in page, and what it may do.
//!
//! ```text
//!   GET  /account              signed out: the sign-in form · signed in: the page
//!   POST /account/signin       email → a link, and one answer whoever asked
//!   GET  /account/s/<token>    the link → a session cookie → back to /account
//!   POST /account/signout      the session stops working now
//!   POST /account/release      move an alias / set visibility — release authority
//!   POST /account/revoke       a machine's key stops working now
//!   POST /account/pair         a fresh pairing code, as the command to run
//! ```
//!
//! **A signed-in session is a release credential.** That sentence is from the
//! plan and it is the whole reason this surface exists: releasing is one
//! narrow capability with two doors — `release.key` for a machine somebody
//! keeps for it, and this page for a person — and `POST /account/release`
//! drives the same `alias_set` the key-authenticated endpoint drives.
//!
//! Three decisions hold the security of it:
//!
//! - **The cookie is `__Host-`-prefixed**, `HttpOnly; Secure; SameSite=Lax;
//!   Path=/`. The prefix is load-bearing, not decoration: a tenant's page on
//!   `alice.art.<domain>` runs their script, and without the prefix it could
//!   set a `Domain=.<domain>` cookie that arrives here — a session fixation
//!   the gate would never see coming. Browsers refuse a `__Host-` cookie that
//!   carries a `Domain` at all, which closes the toss structurally and defers
//!   the move-the-gate-to-its-own-domain question instead of forcing it.
//! - **Every mutating POST carries a CSRF token bound to the session**, on
//!   top of `SameSite=Lax`. Lax is the real defence; the token is the belt
//!   for the browsers and embedders where it is not. The token is derived —
//!   `hash("csrf:" + session token)` — so there is nothing extra to store,
//!   and nothing to desynchronise.
//! - **The sign-in form answers identically whether the address exists.**
//!   Same page, same bytes. The link is budgeted per account per day, so the
//!   form cannot be turned into a way to fill somebody's inbox.
//!
//! What a session deliberately cannot do: publish. Uploading versions is the
//! machines' job through their scoped keys — a browser session that could
//! push bytes would be a third write path with none of the review shape.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;

use super::intake::{form_values, page, shell, shell_with};
use super::{v1, Shared};
use crate::config::{Origin, Role};
use crate::db::UserRow;

/// The session cookie. `__Host-` is what makes a tenant page unable to toss
/// one onto the gate — see the module doc.
const COOKIE: &str = "__Host-factory-session";

fn nothing_here() -> Response {
    page(
        StatusCode::NOT_FOUND,
        shell("Not found", "<h1>Not found</h1>", ""),
    )
}

/// The raw session token out of the Cookie header, if one came.
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value.to_string())
}

/// The signed-in user, plus the raw token the CSRF value derives from.
pub(crate) fn session(app: &Shared, headers: &HeaderMap) -> Option<(String, UserRow)> {
    let token = cookie_token(headers)?;
    let user = app
        .db
        .session_user(&crate::intake::hash_token(&token), &crate::db::now())
        .ok()??;
    Some((token, user))
}

/// The CSRF value for a session — derived, so nothing is stored. It is a
/// hash of the token, never the token: a value printed into every form must
/// not be the credential itself.
pub(crate) fn csrf(token: &str) -> String {
    crate::intake::hash_token(&format!("csrf:{token}"))
}

/// One mutating request's preamble: on the gate, signed in, and carrying the
/// session's CSRF value. Everything failing answers 404 or the sign-in page
/// rather than naming what was wrong.
fn mutating(
    app: &Shared,
    origin: &Origin,
    headers: &HeaderMap,
    body: &str,
) -> Result<(String, UserRow, serde_json::Map<String, serde_json::Value>), Box<Response>> {
    if v1::not_on_gate(origin).is_some() {
        return Err(Box::new(nothing_here()));
    }
    let Some((token, user)) = session(app, headers) else {
        return Err(Box::new(signin_form()));
    };
    let values = form_values(body);
    let sent = values.get("csrf").and_then(|v| v.as_str()).unwrap_or("");
    if sent != csrf(&token) {
        // A missing or stale token is a page re-rendered, not an action
        // taken. 403 rather than 404: the session is real, the request is
        // not — and nothing secret is in the difference.
        return Err(Box::new(page(
            StatusCode::FORBIDDEN,
            shell(
                "Try that again",
                "<h1>Try that again</h1><p>That form was stale. Go back to \
                 <a href=\"/account\">your page</a> and retry.</p>",
                "",
            ),
        )));
    }
    Ok((token, user, values))
}

fn signin_form() -> Response {
    page(
        StatusCode::OK,
        shell_with(
            "Sign in",
            "<h1>Sign in</h1>\
             <p>Your account page is reached by a link, not a password.</p>\
             <form method=\"post\" action=\"/account/signin\">\
             <label for=\"email\">Email</label>\
             <input id=\"email\" name=\"email\" type=\"email\" required>\
             <button type=\"submit\">Send the link</button>\
             </form>",
            "/account/a/",
            &crate::http::intake::Chrome::Public {
                docs_url: None,
                sign_in: true,
            },
        ),
    )
}

/// `GET /account` — the page, or the way in.
pub async fn overview(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((token, user)) = session(&app, &headers) else {
        return signin_form();
    };
    render_overview(&app, &token, &user)
}

fn render_overview(app: &Shared, token: &str, user: &UserRow) -> Response {
    let csrf = csrf(token);
    let esc = mecha_manifest::escape_text;

    let bundles = app.db.bundles_overview(&user.id).unwrap_or_default();
    let artifacts = if bundles.is_empty() {
        "<p>Nothing published yet. A connected machine publishes with \
         <code>factory-publish publish</code>; what lands here is yours to \
         release.</p>"
            .to_string()
    } else {
        let mut rows = String::from(
            "<table><tr><th>bundle</th><th>versions</th><th>share URL shows</th>\
             <th>who may read</th><th></th></tr>",
        );
        // One form builder, many buttons: every action here is the same
        // `alias_set` the release key drives, spelled as (version?,
        // visibility) — and each button says which spelling it means, because
        // the old "Make private" sent no version and so silently un-aliased
        // as well, which is two decisions wearing one label.
        let release_form = |id: &str, version: Option<u32>, visibility: &str, label: &str| {
            format!(
                "<form method=\"post\" action=\"/account/release\">\
                 <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                 <input type=\"hidden\" name=\"id\" value=\"{id}\">\
                 {version}\
                 <input type=\"hidden\" name=\"visibility\" value=\"{visibility}\">\
                 <button type=\"submit\">{label}</button></form>",
                id = esc(id),
                version = version
                    .map(|v| format!("<input type=\"hidden\" name=\"version\" value=\"{v}\">"))
                    .unwrap_or_default(),
            )
        };
        for bundle in &bundles {
            let public = bundle.visibility == mecha_manifest::Visibility::Public;
            let vis_word = if public { "public" } else { "private" };
            let shown = match bundle.aliased {
                Some(version) => format!("v{version}"),
                None => "nothing".to_string(),
            };
            let url = format!(
                "{}/b/{}/",
                app.config.user_url(Role::Artifacts, &user.handle),
                bundle.id
            );
            // Every stored version, viewable at its immutable URL, with a pin
            // for whichever the share URL should follow.
            let versions = app
                .db
                .bundle_versions(&user.id, &bundle.id)
                .unwrap_or_default();
            let mut versions_cell = String::new();
            for v in &versions {
                versions_cell.push_str(&format!("<a href=\"{url}v/{v}/\">v{v}</a> "));
                if bundle.aliased == Some(*v) {
                    versions_cell.push_str("(live) ");
                } else {
                    versions_cell.push_str(&release_form(
                        &bundle.id,
                        Some(*v),
                        vis_word,
                        &format!("pin v{v}"),
                    ));
                }
            }
            let mut actions = String::new();
            if public {
                // Who may read changes; where the share URL points does not.
                actions.push_str(&release_form(
                    &bundle.id,
                    bundle.aliased,
                    "private",
                    "Make private",
                ));
            } else if let Some(version) = bundle.aliased.or(Some(bundle.latest)) {
                actions.push_str(&release_form(
                    &bundle.id,
                    Some(version),
                    "public",
                    &format!("Release v{version} publicly"),
                ));
            }
            if bundle.aliased.is_some() {
                // The share URL points at nothing afterwards; every version
                // stays on disk — nothing on this page deletes one.
                actions.push_str(&release_form(&bundle.id, None, "private", "Take down"));
            }
            rows.push_str(&format!(
                "<tr><td><a href=\"{url}\">{id}</a> — {title}</td>\
                 <td>{versions_cell}</td>\
                 <td>{shown}</td><td>{vis}</td><td>{actions}</td></tr>",
                id = esc(&bundle.id),
                title = esc(&bundle.title),
                vis = if public { "everyone" } else { "nobody" },
            ));
        }
        rows.push_str("</table>");
        rows
    };

    let keys = app.db.keys_for_user(&user.id).unwrap_or_default();
    let mut machines = String::from(
        "<table><tr><th>key</th><th>scope</th><th>label</th>\
         <th>minted</th><th>last used</th><th></th></tr>",
    );
    for key in &keys {
        let state = match &key.revoked_at {
            Some(at) => format!("revoked {}", esc(at)),
            None => format!(
                "<form method=\"post\" action=\"/account/revoke\">\
                 <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                 <input type=\"hidden\" name=\"key\" value=\"{id}\">\
                 <button type=\"submit\">Revoke</button></form>",
                id = esc(&key.id),
            ),
        };
        machines.push_str(&format!(
            "<tr><td><code>{id}</code></td><td>{scope}</td><td>{label}</td>\
             <td>{minted}</td><td>{used}</td><td>{state}</td></tr>",
            id = esc(&key.id),
            scope = key.scope.as_str(),
            label = esc(&key.label),
            minted = esc(&key.created_at),
            used = esc(key.last_used_at.as_deref().unwrap_or("never")),
        ));
    }
    machines.push_str("</table>");

    // Sign-out, pairing and the identity line live in the header's dropdown
    // now; the page body is the two ledgers and nothing twice.
    let body = format!(
        "<h1><code>{handle}</code></h1>\
         <h2 id=\"artifacts\">Artifacts</h2>{artifacts}\
         <h2 id=\"machines\">Machines</h2>\
         <p>Each connected machine holds its own keys. Revoking here is what \
         makes a lost laptop somebody else's brick — a key that is used is a \
         machine that is alive.</p>{machines}",
        handle = esc(&user.handle),
    );
    let chrome = crate::http::intake::Chrome::Account {
        handle: user.handle.clone(),
        email: user.email.clone(),
        csrf: csrf.clone(),
        docs_url: app.config.docs_url.clone(),
    };
    page(
        StatusCode::OK,
        shell_with(&user.handle, &body, "/account/a/", &chrome),
    )
}

/// `POST /account/signin` — one answer, whoever asked.
pub async fn signin(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    body: String,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let email = form_values(&body)
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    // Everything below this line must not change what the page says. The
    // work happens if there is work; the answer is the same either way.
    if !email.is_empty() {
        match app.db.users_by_email(&email) {
            Ok(users) => {
                for user in users {
                    send_signin_link(&app, &user);
                }
            }
            Err(e) => tracing::error!(error = %e, "looking up a sign-in address"),
        }
    }
    page(
        StatusCode::OK,
        shell(
            "Check your email",
            "<h1>Check your email</h1><p>If that address has an account, a \
             sign-in link is on its way. It works once and expires in a few \
             minutes.</p>",
            "/account/a/",
        ),
    )
}

fn send_signin_link(app: &Shared, user: &UserRow) {
    let today = crate::db::today();
    match app.db.signin_links_today(&user.id, &today) {
        Ok(sent) if sent >= crate::intake::SIGNIN_LINKS_PER_DAY => {
            tracing::warn!(handle = %user.handle, "sign-in link budget exhausted for today");
            return;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "counting sign-in links");
            return;
        }
    }
    let token = crate::intake::mint_token();
    let expires = (chrono::Utc::now()
        + chrono::Duration::minutes(crate::intake::SIGNIN_LINK_EXPIRY_MINUTES))
    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    if let Err(e) = app.db.signin_link_create(
        &user.id,
        &crate::intake::hash_token(&token),
        &crate::db::now(),
        &expires,
    ) {
        tracing::error!(error = %e, "recording a sign-in link");
        return;
    }
    let link = format!("{}/account/s/{token}", app.config.base_url(Role::Gate));
    app.mailer.send_signin(&user.email, &user.handle, &link);
}

/// `GET /account/s/<token>` — a page with one button, and deliberately no
/// database read at all.
///
/// The token used to be spent by this GET, and the first real sign-in from a
/// university mailbox arrived already dead: Microsoft Safe Links (and every
/// scanner like it) fetches a mail's URLs on delivery, so the robot's GET
/// redeemed the link seconds before the person's click. Scanners follow GETs
/// and do not submit forms — so the GET renders a Continue button and the
/// POST below does everything the GET used to. No peek at the ledger here
/// either: a page that varied on the token's state would hand the scanner's
/// GET an oracle, and the POST answers soon enough.
pub async fn finish_page(Extension(origin): Extension<Origin>) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    page(
        StatusCode::OK,
        shell(
            "Sign in",
            "<h1>Almost in</h1>\
             <p>One click to finish — this is what keeps a mail scanner's \
             robot from spending your link before you can.</p>\
             <form method=\"post\">\
             <button type=\"submit\">Continue to your page</button></form>",
            "/account/a/",
        ),
    )
}

/// `POST /account/s/<token>` — the link becomes a session.
pub async fn finish(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(token): Path<String>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let now = crate::db::now();
    let session_token = crate::intake::mint_token();
    let expires = (chrono::Utc::now() + chrono::Duration::days(crate::intake::SESSION_EXPIRY_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // One transaction spends the link and mints the session, so a failure
    // between the two cannot burn the link — a retried click still works,
    // and the "expired" page below is only ever the truth.
    match app.db.signin(
        &crate::intake::hash_token(&token),
        &crate::intake::hash_token(&session_token),
        &now,
        &expires,
    ) {
        Ok(Some(_)) => {}
        Ok(None) => {
            // Spent, expired, never real, or a suspended account: one page,
            // like every dead token in this program.
            return page(
                StatusCode::NOT_FOUND,
                shell(
                    "That link has expired",
                    "<h1>That link has expired</h1><p>Sign-in links work once \
                     and briefly. <a href=\"/account\">Ask for another.</a></p>",
                    "",
                ),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "signing in");
            return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    }

    // 303: the token has been spent, and a refresh of this URL must land on
    // the page rather than re-redeeming a dead link.
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/account".to_string()),
            (
                header::SET_COOKIE,
                super::session_cookie(
                    COOKIE,
                    &session_token,
                    crate::intake::SESSION_EXPIRY_DAYS * 24 * 60 * 60,
                ),
            ),
        ],
    )
        .into_response()
}

/// `POST /account/signout`.
pub async fn signout(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, _, _) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let _ = app
        .db
        .session_revoke(&crate::intake::hash_token(&token), &crate::db::now());
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/account".to_string()),
            (header::SET_COOKIE, super::session_cookie(COOKIE, "", 0)),
        ],
    )
        .into_response()
}

/// `POST /account/release` — the session's release authority.
///
/// The same `alias_set` the release key drives, under the same rules: the
/// version must exist, and the user is the session's, never a form field.
pub async fn release(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, user, values) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let field = |name: &str| {
        values
            .get(name)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let id = field("id");
    if mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return nothing_here();
    }
    let visibility = match field("visibility").as_str() {
        "public" => mecha_manifest::Visibility::Public,
        "private" => mecha_manifest::Visibility::Private,
        _ => return nothing_here(),
    };
    let version = match field("version").as_str() {
        "" => None,
        text => match text.parse::<u32>() {
            Ok(version) => Some(version),
            Err(_) => return nothing_here(),
        },
    };
    if let Some(version) = version {
        match app.db.bundle(&user.id, &id, version) {
            Ok(Some(_)) => {}
            _ => return nothing_here(),
        }
    }
    if let Err(e) = app
        .db
        .alias_set(&user.id, &id, version, visibility, &crate::db::now())
    {
        tracing::error!(error = %e, %id, "releasing from a session");
        return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
            .into_response();
    }
    tracing::info!(handle = %user.handle, %id, ?version, ?visibility, "released from a session");
    // The viewer's controls come back to the viewer. Only a viewer path is
    // honoured — a relative one with no scheme and no authority — so the
    // field can never become an open redirect.
    let back = field("return");
    if back.starts_with("/view/") && !back.starts_with("//") {
        return (StatusCode::SEE_OTHER, [(header::LOCATION, back)]).into_response();
    }
    render_overview(&app, &token, &user)
}

/// `POST /account/revoke` — a machine dies now.
pub async fn revoke(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, user, values) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let key = values
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    // Ownership is the WHERE clause: somebody else's key id revokes nothing.
    match app.db.key_revoke_for(&user.id, key, &crate::db::now()) {
        Ok(revoked) => {
            if revoked {
                tracing::info!(handle = %user.handle, key, "key revoked from the account page");
            }
        }
        Err(e) => tracing::error!(error = %e, "revoking a key"),
    }
    render_overview(&app, &token, &user)
}

/// Where a share verb lands afterwards: back on the viewer that posted it,
/// or the overview.
fn back_or_overview(
    app: &Shared,
    token: &str,
    user: &UserRow,
    values: &serde_json::Map<String, serde_json::Value>,
) -> Response {
    let back = values
        .get("return")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if back.starts_with("/view/") && !back.starts_with("//") {
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, back.to_string())],
        )
            .into_response();
    }
    render_overview(app, token, user)
}

/// `POST /account/share` — grant an address this bundle, and have the box
/// mail them the viewer link.
///
/// The grant is the owner's decision about their own bundle, but the mail
/// reaches a stranger on a tenant's say-so — the shape of a mail cannon —
/// so it is budgeted per owner per day, and a duplicate grant mails
/// nothing: the row already exists, and the reader page mails its own
/// sign-in links on request.
pub async fn share(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, user, values) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let field = |name: &str| {
        values
            .get(name)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let id = field("id");
    if mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return nothing_here();
    }
    let email = crate::intake::normalize_email(&field("email"));
    if email.is_empty() || !email.contains('@') {
        return nothing_here();
    }
    // Only something that exists is shareable — a grant on nothing would
    // mail a stranger a link to a 404 signed with your handle.
    let versions = app.db.bundle_versions(&user.id, &id).unwrap_or_default();
    if versions.is_empty() {
        return nothing_here();
    }
    match app.db.shares_today(&user.id, &crate::db::today()) {
        Ok(minted) if minted >= crate::intake::SHARES_PER_DAY => {
            return page(
                StatusCode::OK,
                shell(
                    "Sharing limit reached",
                    "<h1>Sharing limit reached</h1><p>Sharing mails people \
                     on your behalf, so it is bounded per day. Try again \
                     tomorrow.</p><p><a href=\"/account\">Back to your \
                     page.</a></p>",
                    "/account/a/",
                ),
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "counting shares");
            return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    }
    match app
        .db
        .share_create(&user.id, &id, &email, &crate::db::now())
    {
        Ok(true) => {
            // The mailed link is the bare viewer URL: stable while the
            // alias moves, and the sign-in flow hangs off it.
            let live = app
                .db
                .alias(&user.id, &id)
                .ok()
                .flatten()
                .and_then(|alias| alias.version);
            let title = live
                .or_else(|| versions.iter().max().copied())
                .and_then(|v| app.db.bundle(&user.id, &id, v).ok().flatten())
                .map(|row| row.title)
                .unwrap_or_else(|| id.clone());
            let link = format!(
                "{}/view/{}/{id}",
                app.config.base_url(Role::Gate),
                user.handle
            );
            app.mailer.send_share(&email, &user.handle, &title, &link);
            tracing::info!(handle = %user.handle, %id, "bundle shared");
        }
        Ok(false) => {
            // Already granted: nothing new to record, nobody to re-mail.
        }
        Err(e) => {
            tracing::error!(error = %e, "recording a share");
            return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    }
    back_or_overview(&app, &token, &user, &values)
}

/// `POST /account/share-revoke` — a grant stops working now. Ownership is
/// the WHERE clause: somebody else's share id revokes nothing.
pub async fn share_revoke(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, user, values) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let share = values
        .get("share")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match app.db.share_revoke(&user.id, share, &crate::db::now()) {
        Ok(revoked) => {
            if revoked {
                tracing::info!(handle = %user.handle, share, "share revoked");
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "revoking a share");
            return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    }
    back_or_overview(&app, &token, &user, &values)
}

/// `POST /account/pair` — a fresh code, as the command to run.
pub async fn pair(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (_, user, _) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let code = match crate::keys::mint_pairing(&app.db, &user.id) {
        Ok(code) => code,
        Err(e) => {
            tracing::error!(error = %e, "minting a pairing code");
            return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    };
    let esc = mecha_manifest::escape_text;
    let body = format!(
        "<h1>Connect a machine</h1>\
         <p>On the machine that will publish for <code>{handle}</code>, run:</p>\
         <pre><code>factory-publish connect --gate {gate} --handle {handle} \
{code}</code></pre>\
         <p>The code works once and expires in {expiry}&nbsp;minutes. \
         <a href=\"/account\">Back to your page.</a></p>",
        handle = esc(&user.handle),
        gate = esc(&app.config.base_url(Role::Gate)),
        code = esc(&code),
        expiry = crate::keys::PAIR_EXPIRY_MINUTES,
    );
    page(
        StatusCode::OK,
        shell("Connect a machine", &body, "/account/a/"),
    )
}

/// `GET /account/a/<name>` — the shared stylesheet, at this depth.
pub async fn asset(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(name): Path<String>,
) -> Response {
    super::intake::serve_asset(&app, &origin, &name)
}
