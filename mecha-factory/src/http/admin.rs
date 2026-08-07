//! The operator's page: the admin API, worn by a browser.
//!
//! ```text
//!   POST /v1/admin/signin      operate key → a one-time URL (the CLI's verb)
//!   GET  /admin                signed out: how to sign in · signed in: the panel
//!   GET  /admin/s/<token>      a page with one button, and no database read
//!   POST /admin/s/<token>      the link becomes a session cookie
//!   POST /admin/signout        the session stops working now
//!   POST /admin/status         suspend or restore an account
//!   POST /admin/invite         mint an invite; the box mails it
//!   POST /admin/invite-revoke  stop an unclaimed invite working
//!   POST /admin/key-revoke     break-glass, any key
//!   POST /admin/withhold       take a version out of service, or put it back
//! ```
//!
//! **The way in is the CLI, and there is deliberately no form.** A signed-out
//! `/admin` tells you to run `factory-publish operator signin`; it has nothing
//! to type into. The operate key is the most powerful credential this box
//! knows, and a page that accepted it would teach its holder to paste it into
//! browsers — which is what a look-alike page needs and a one-time link
//! forecloses: the CLI holds the key, asks the box it already knows for a URL,
//! and the URL is worthless in minutes and after one use. The GET/POST split
//! on redeeming is the same mail-scanner armour the tenant links wear, not
//! because these links travel through mail — they travel through a terminal —
//! but because a prefetching browser bar hits GETs too, and one rule for
//! every token is one rule.
//!
//! **An operator session shares nothing with a tenant session.** Its own
//! cookie (`__Host-factory-operator`), its own tables, and the opposite join:
//! a tenant session resolves to a *user*, an operator session resolves to a
//! *key* — `operator_session_key` never reads `users` at all. So neither
//! cookie means anything at the other surface, there is no account for the
//! tenant authoriser to find behind an operator session, and break-glass
//! revoking the operate key ends its browser sessions in the same query that
//! would have authorised them.
//!
//! Everything a POST here does is the same row the `/v1/admin/*` endpoint
//! drives — the panel is the CLI's verbs with a rendered ledger beside them,
//! never a second implementation of any of them.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;

use super::account;
use super::intake::{form_values, page, shell};
use super::{v1, Failure, Shared};
use crate::config::{Origin, Role};
use crate::db::KeyRow;

/// The operator's cookie. A different name from the tenant one on purpose:
/// the two sessions must not even collide in a cookie jar.
const COOKIE: &str = "__Host-factory-operator";

/// How long the one-time URL waits. The person who minted it is watching
/// their own terminal, so this bounds an attacker's window, not a human's.
pub const LINK_EXPIRY_MINUTES: i64 = 10;

/// A working day, not a tenant's week: this session suspends accounts and
/// kills keys, so it re-earns itself daily.
pub const SESSION_EXPIRY_HOURS: i64 = 12;

fn nothing_here() -> Response {
    page(
        StatusCode::NOT_FOUND,
        shell("Not found", "<h1>Not found</h1>", ""),
    )
}

/// `POST /v1/admin/signin` — the operate key asks for a browser.
///
/// Lives under `/v1` with the other operator endpoints because it *is* one:
/// key-authenticated JSON, driven by the CLI. What it returns is the only
/// bridge between the key surface and the session surface.
pub async fn signin_link(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = v1::not_on_gate(&origin) {
        return refusal;
    }
    let key = match v1::authorised_operator(&app, &headers) {
        Ok(key) => key,
        Err(refusal) => return *refusal,
    };
    let token = crate::intake::mint_token();
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(LINK_EXPIRY_MINUTES))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    if let Err(e) = app.db.operator_link_create(
        &key.id,
        &crate::intake::hash_token(&token),
        &crate::db::now(),
        &expires,
    ) {
        tracing::error!(error = %e, "recording an operator link");
        return Failure::json(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
    }
    tracing::info!(key = %key.id, "operator sign-in link minted");
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "url": format!("{}/admin/s/{token}", app.config.base_url(Role::Gate)),
            "expires_at": expires,
        })),
    )
        .into_response()
}

/// The signed-in operate key, plus the raw token the CSRF value derives
/// from. `Ok(None)` is signed out; `Err` is the ledger failing, which must
/// answer 5xx rather than the sign-in page — a valid session rendered as
/// expired sends the operator back to the CLI to work around a database
/// fault, with the fault never logged.
fn session(
    app: &Shared,
    headers: &HeaderMap,
) -> Result<Option<(String, KeyRow)>, Box<Response>> {
    let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|header| header.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .filter_map(|pair| pair.trim().split_once('='))
                .find(|(name, _)| *name == COOKIE)
                .map(|(_, value)| value.to_string())
        })
    else {
        return Ok(None);
    };
    match app
        .db
        .operator_session_key(&crate::intake::hash_token(&token), &crate::db::now())
    {
        Ok(Some(key)) => {
            // The stamp every bearer call gets (`keys::authenticate`), on
            // the same contract: a panel session is the key acting, and the
            // liveness ledger must not miss exactly the most powerful
            // activity on the box. Best-effort there, best-effort here.
            app.db.key_touch(&key.id, &crate::db::now());
            Ok(Some((token, key)))
        }
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::error!(error = %e, "reading an operator session");
            Err(Box::new(
                Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response(),
            ))
        }
    }
}

/// The signed-out page: instructions, and deliberately nothing to type into.
fn how_to_sign_in() -> Response {
    page(
        StatusCode::OK,
        shell(
            "Operator",
            "<h1>Operator</h1>\
             <p>This page signs in from the operator&rsquo;s CLI, not from a \
             form. On the machine holding the operate key, run:</p>\
             <pre><code>factory-publish operator signin</code></pre>\
             <p>and open the link it prints. The link works once and expires \
             in minutes; the key itself never enters a browser.</p>",
            "/account/a/",
        ),
    )
}

/// `GET /admin` — the panel, or the way in.
pub async fn overview(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    match session(&app, &headers) {
        Ok(Some((token, key))) => render_panel(&app, &token, &key, None),
        Ok(None) => how_to_sign_in(),
        Err(refusal) => *refusal,
    }
}

/// `GET /admin/s/<token>` — one button, no database read; see the module doc
/// for why a GET must spend nothing.
pub async fn finish_page(Extension(origin): Extension<Origin>) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    page(
        StatusCode::OK,
        shell(
            "Operator sign-in",
            "<h1>Almost in</h1>\
             <p>One click to finish &mdash; nothing is spent until the \
             button.</p>\
             <form method=\"post\">\
             <button type=\"submit\">Continue to the panel</button></form>",
            "/account/a/",
        ),
    )
}

/// `POST /admin/s/<token>` — the link becomes a session.
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
    let expires = (chrono::Utc::now() + chrono::Duration::hours(SESSION_EXPIRY_HOURS))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // One transaction spends the link and mints the session, so a failure
    // between the two cannot burn the link — a retried click still works,
    // and the "expired" page below is only ever the truth. The same redeem
    // refuses a link whose key has died or lost `operate` since minting.
    let key_id = match app.db.operator_signin(
        &crate::intake::hash_token(&token),
        &crate::intake::hash_token(&session_token),
        &now,
        &expires,
    ) {
        Ok(Some(key_id)) => key_id,
        Ok(None) => {
            return page(
                StatusCode::NOT_FOUND,
                shell(
                    "That link has expired",
                    "<h1>That link has expired</h1><p>Operator links work \
                     once and briefly. Mint another with \
                     <code>factory-publish operator signin</code>.</p>",
                    "",
                ),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "signing an operator in");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    };
    tracing::info!(key = %key_id, "operator signed in from a link");
    // 303, like the tenant finish: a refresh must land on the panel, never
    // re-redeem a dead link.
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/admin".to_string()),
            (
                header::SET_COOKIE,
                super::session_cookie(COOKIE, &session_token, SESSION_EXPIRY_HOURS * 60 * 60),
            ),
        ],
    )
        .into_response()
}

/// One mutating request's preamble: on the gate, signed in as the operator,
/// and carrying the session's CSRF value.
fn mutating(
    app: &Shared,
    origin: &Origin,
    headers: &HeaderMap,
    body: &str,
) -> Result<(String, KeyRow, serde_json::Map<String, serde_json::Value>), Box<Response>> {
    if v1::not_on_gate(origin).is_some() {
        return Err(Box::new(nothing_here()));
    }
    let (token, key) = match session(app, headers) {
        Ok(Some(pair)) => pair,
        Ok(None) => return Err(Box::new(how_to_sign_in())),
        Err(refusal) => return Err(refusal),
    };
    let values = form_values(body);
    let sent = values.get("csrf").and_then(|v| v.as_str()).unwrap_or("");
    if sent != account::csrf(&token) {
        return Err(Box::new(page(
            StatusCode::FORBIDDEN,
            shell(
                "Try that again",
                "<h1>Try that again</h1><p>That form was stale. Go back to \
                 <a href=\"/admin\">the panel</a> and retry.</p>",
                "",
            ),
        )));
    }
    Ok((token, key, values))
}

/// `POST /admin/signout`.
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
    // Sign-out's claim is server-side revocation, so a failed revoke must
    // not answer as success: the cookie would be gone from this browser
    // while the session stayed alive for anyone who captured it.
    if let Err(e) = app
        .db
        .operator_session_revoke(&crate::intake::hash_token(&token), &crate::db::now())
    {
        tracing::error!(error = %e, "revoking an operator session");
        return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/admin".to_string()),
            (header::SET_COOKIE, super::session_cookie(COOKIE, "", 0)),
        ],
    )
        .into_response()
}

fn field(values: &serde_json::Map<String, serde_json::Value>, name: &str) -> String {
    values
        .get(name)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// `POST /admin/status` — suspend or restore, the same row the JSON endpoint
/// drives.
pub async fn status(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, key, values) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let handle = field(&values, "handle");
    let status = field(&values, "status");
    if !matches!(status.as_str(), "active" | "suspended") {
        return nothing_here();
    }
    let notice = match app.db.user_by_handle(&handle) {
        Ok(Some(user)) => match app.db.user_status(&user.id, &status) {
            Ok(_) => {
                tracing::info!(%handle, %status, "operator set a status from the panel");
                format!("{handle} is now {status}.")
            }
            Err(e) => {
                tracing::error!(error = %e, "setting a status");
                "Unavailable — nothing changed.".to_string()
            }
        },
        Ok(None) => format!("No such handle: {handle}."),
        Err(e) => {
            tracing::error!(error = %e, "reading a user");
            "Unavailable — nothing changed.".to_string()
        }
    };
    render_panel(&app, &token, &key, Some(&notice))
}

/// `POST /admin/invite` — mint and mail, through the same one definition the
/// JSON endpoint uses.
pub async fn invite(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, key, values) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let email = field(&values, "email");
    let note = field(&values, "note");
    match v1::mint_invite(&app, &email, &note) {
        // Post-redirect-get, because this verb mails a stranger: the
        // render-after-POST the idempotent verbs share would let a refresh
        // mint and mail a second invite. The new pending row on the panel
        // is the confirmation; the copyable link stays a CLI affordance
        // (`factory-publish operator invite`).
        Ok(_) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, "/admin".to_string())],
        )
            .into_response(),
        Err(v1::InviteRefused::BadAddress) => {
            render_panel(&app, &token, &key, Some("An email address is required."))
        }
        Err(v1::InviteRefused::Unavailable) => {
            render_panel(&app, &token, &key, Some("Unavailable — nothing minted."))
        }
    }
}

/// `POST /admin/invite-revoke`.
pub async fn invite_revoke(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, key, values) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let id = field(&values, "id");
    let notice = match app.db.invite_revoke(&id, &crate::db::now()) {
        Ok(true) => "Invite revoked — the link no longer works.".to_string(),
        Ok(false) => "Not a pending invite (claimed, already revoked, or unknown).".to_string(),
        Err(e) => {
            tracing::error!(error = %e, "revoking an invite");
            "Unavailable — nothing changed.".to_string()
        }
    };
    render_panel(&app, &token, &key, Some(&notice))
}

/// `POST /admin/key-revoke` — break-glass, any key, including the one whose
/// session is asking. The next page load says so, because the session died
/// with it.
pub async fn key_revoke(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, key, values) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let id = field(&values, "id");
    let notice = match app.db.key_revoke(&id, &crate::db::now()) {
        Ok(true) => {
            tracing::info!(key = %id, "operator revoked a key from the panel");
            format!("Key {id} revoked.")
        }
        Ok(false) => "Not a live key.".to_string(),
        Err(e) => {
            tracing::error!(error = %e, "revoking a key");
            "Unavailable — nothing changed.".to_string()
        }
    };
    // If the operator just revoked their own key, this render is the last
    // thing the session shows — `session()` will refuse it next time. That
    // is correct, and the how-to-sign-in page is where they land.
    render_panel(&app, &token, &key, Some(&notice))
}

/// `POST /admin/withhold` — out of service or back in, by the `undo` field.
pub async fn withhold(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, key, values) = match mutating(&app, &origin, &headers, &body) {
        Ok(ok) => ok,
        Err(response) => return *response,
    };
    let handle = field(&values, "handle");
    let id = field(&values, "id");
    let undo = !field(&values, "undo").is_empty();
    let reason = field(&values, "reason");
    let Ok(version) = field(&values, "version").parse::<u32>() else {
        return render_panel(&app, &token, &key, Some("A version number is required."));
    };
    let notice = match app.db.user_by_handle(&handle) {
        Ok(Some(user)) => {
            let now = crate::db::now();
            match app.db.bundle_withhold(
                &user.id,
                &id,
                version,
                if undo || reason.is_empty() {
                    None
                } else {
                    Some(&reason)
                },
                if undo { None } else { Some(&now) },
            ) {
                Ok(true) => {
                    tracing::info!(%handle, %id, version, undo,
                                   "operator changed a withhold from the panel");
                    if undo {
                        format!("{handle}/{id} v{version} is back in service.")
                    } else {
                        format!("{handle}/{id} v{version} is withheld.")
                    }
                }
                Ok(false) => format!("No such version: {handle}/{id} v{version}."),
                Err(e) => {
                    tracing::error!(error = %e, "withholding");
                    "Unavailable — nothing changed.".to_string()
                }
            }
        }
        Ok(None) => format!("No such handle: {handle}."),
        Err(e) => {
            tracing::error!(error = %e, "reading a user");
            "Unavailable — nothing changed.".to_string()
        }
    };
    render_panel(&app, &token, &key, Some(&notice))
}

/// The panel: every ledger the admin API serves, with the verb beside the
/// row it acts on.
fn render_panel(app: &Shared, token: &str, key: &KeyRow, notice: Option<&str>) -> Response {
    // The panel is the security ledger, so a failed read must answer as a
    // failure: an empty Accounts or Keys table rendered over a database
    // error reads as "nothing there", which is the one lie this page is
    // for. Everything is fetched before any HTML exists — one refusal
    // covers all five reads, and the queue counts come grouped so N
    // tenants are one query rather than N.
    let fetched = (|| -> anyhow::Result<_> {
        Ok((
            app.db.users()?,
            app.db.queue_depths()?,
            app.db.invites()?,
            app.db.keys()?,
            app.db.withheld()?,
        ))
    })();
    let (users, queue_depths, invites, keys, withheld) = match fetched {
        Ok(data) => data,
        Err(e) => {
            tracing::error!(error = %e, "reading the operator ledgers");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    };

    let csrf = account::csrf(token);
    let esc = mecha_manifest::escape_text;

    let notice = match notice {
        Some(text) => format!("<p class=\"notice\"><strong>{}</strong></p>", esc(text)),
        None => String::new(),
    };

    // A one-button form, which is most of what an operator does.
    let act = |action: &str, fields: &[(&str, &str)], label: &str| {
        let hidden: String = fields
            .iter()
            .map(|(name, value)| {
                format!(
                    "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
                    esc(value)
                )
            })
            .collect();
        format!(
            "<form method=\"post\" action=\"{action}\">\
             <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
             {hidden}<button type=\"submit\">{label}</button></form>"
        )
    };

    let mut accounts = String::from(
        "<table><tr><th>handle</th><th>email</th><th>status</th>\
         <th>since</th><th>queued</th><th></th></tr>",
    );
    for user in &users {
        let flip = if user.active() {
            act(
                "/admin/status",
                &[("handle", &user.handle), ("status", "suspended")],
                "Suspend",
            )
        } else {
            act(
                "/admin/status",
                &[("handle", &user.handle), ("status", "active")],
                "Restore",
            )
        };
        accounts.push_str(&format!(
            "<tr><td><code>{handle}</code></td><td>{email}</td><td>{status}</td>\
             <td>{since}</td><td>{queued}</td><td>{flip}</td></tr>",
            handle = esc(&user.handle),
            email = esc(&user.email),
            status = esc(&user.status),
            since = esc(&user.created_at),
            queued = queue_depths.get(&user.id).copied().unwrap_or(0),
        ));
    }
    accounts.push_str("</table>");

    let now = crate::db::now();
    let mut invites_out = format!(
        "<form method=\"post\" action=\"/admin/invite\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <label for=\"invite-email\">Email</label>\
         <input id=\"invite-email\" name=\"email\" type=\"email\" required>\
         <label for=\"invite-note\">Note</label>\
         <input id=\"invite-note\" name=\"note\" type=\"text\">\
         <button type=\"submit\">Invite</button></form>"
    );
    if !invites.is_empty() {
        invites_out.push_str(
            "<table><tr><th>email</th><th>state</th><th>expires</th>\
             <th>note</th><th></th></tr>",
        );
        for row in &invites {
            let state = row.status(&now);
            let action = if state == "pending" {
                act("/admin/invite-revoke", &[("id", &row.id)], "Revoke")
            } else {
                String::new()
            };
            invites_out.push_str(&format!(
                "<tr><td>{email}</td><td>{state}{claimed}</td><td>{expires}</td>\
                 <td>{note}</td><td>{action}</td></tr>",
                email = esc(&row.email),
                state = esc(state),
                claimed = match &row.claimed_by {
                    Some(handle) => format!(" by <code>{}</code>", esc(handle)),
                    None => String::new(),
                },
                expires = esc(&row.expires_at),
                note = esc(&row.note),
            ));
        }
        invites_out.push_str("</table>");
    }

    let mut keys_out = String::from(
        "<table><tr><th>key</th><th>whose</th><th>scope</th><th>label</th>\
         <th>last used</th><th></th></tr>",
    );
    for row in &keys {
        let whose = users
            .iter()
            .find(|u| u.id == row.user_id)
            .map(|u| u.handle.as_str())
            .unwrap_or(if row.user_id.is_empty() {
                "(operator)"
            } else {
                "(unknown)"
            });
        let state = match &row.revoked_at {
            Some(at) => format!("revoked {}", esc(at)),
            None => act("/admin/key-revoke", &[("id", &row.id)], "Revoke"),
        };
        keys_out.push_str(&format!(
            "<tr><td><code>{id}</code></td><td>{whose}</td><td>{scope}</td>\
             <td>{label}</td><td>{used}</td><td>{state}</td></tr>",
            id = esc(&row.id),
            whose = esc(whose),
            scope = row.scope.as_str(),
            label = esc(&row.label),
            used = esc(row.last_used_at.as_deref().unwrap_or("never")),
        ));
    }
    keys_out.push_str("</table>");

    let mut withheld_out = String::new();
    if !withheld.is_empty() {
        withheld_out.push_str(
            "<table><tr><th>version</th><th>since</th><th>reason</th><th></th></tr>",
        );
        for row in &withheld {
            let restore = act(
                "/admin/withhold",
                &[
                    ("handle", &row.handle),
                    ("id", &row.id),
                    ("version", &row.version.to_string()),
                    ("undo", "1"),
                ],
                "Restore",
            );
            withheld_out.push_str(&format!(
                "<tr><td><code>{handle}/{id}</code> v{version}</td>\
                 <td>{since}</td><td>{reason}</td><td>{restore}</td></tr>",
                handle = esc(&row.handle),
                id = esc(&row.id),
                version = row.version,
                since = esc(&row.withheld_at),
                reason = esc(row.reason.as_deref().unwrap_or("")),
            ));
        }
        withheld_out.push_str("</table>");
    }
    withheld_out.push_str(&format!(
        "<form method=\"post\" action=\"/admin/withhold\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <label for=\"wh-handle\">Handle</label>\
         <input id=\"wh-handle\" name=\"handle\" required>\
         <label for=\"wh-id\">Bundle</label>\
         <input id=\"wh-id\" name=\"id\" required>\
         <label for=\"wh-version\">Version</label>\
         <input id=\"wh-version\" name=\"version\" type=\"number\" min=\"1\" required>\
         <label for=\"wh-reason\">Reason</label>\
         <input id=\"wh-reason\" name=\"reason\">\
         <button type=\"submit\">Withhold</button></form>"
    ));

    let body = format!(
        "<h1>Operator</h1>\
         <p>Signed in from operate key <code>{key_id}</code>{label}. \
         Everything here is the same row the CLI drives.</p>\
         {signout}{notice}\
         <h2 id=\"accounts\">Accounts</h2>{accounts}\
         <h2 id=\"invites\">Invites</h2>\
         <p>The box mails the link; nothing to copy unless mail fails.</p>\
         {invites_out}\
         <h2 id=\"keys\">Keys</h2>\
         <p>Break-glass: revoking here kills any key, including the one this \
         session rode in on.</p>{keys_out}\
         <h2 id=\"withheld\">Withheld</h2>\
         <p>Out of service, reversibly, on a report. The bytes stay.</p>\
         {withheld_out}",
        key_id = esc(&key.id),
        label = if key.label.is_empty() {
            String::new()
        } else {
            format!(" (<code>{}</code>)", esc(&key.label))
        },
        signout = act("/admin/signout", &[], "Sign out"),
    );
    page(StatusCode::OK, shell("Operator", &body, "/account/a/"))
}
