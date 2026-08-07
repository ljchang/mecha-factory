//! Claiming a handle, from an invite link.
//!
//! ```text
//!   GET  /signup/<token>            the claim form, if the invite is live
//!   POST /signup/<token>            handle validated → the account exists
//!   GET  /signup/<token>/form.css   the same stylesheet the forms use
//! ```
//!
//! The signup endpoint ends by calling the same user-creation path the CLI
//! does (`Db::invite_claim` → `create_user_in`), which is the promise `user
//! create`'s help text has made since before this existed: the front door is
//! new, the mechanism is not. The moment the row commits, the certificate
//! reconciler notices it on its next pass and the new hostnames start
//! answering — nothing here knows certificates exist.
//!
//! What this deliberately does not have:
//!
//! - **No second verification email.** The invite arrived by email and the
//!   token in the link is single-use; clicking it *is* the proof of address.
//!   A magic link to confirm a magic link would be ceremony.
//! - **No oracle.** Claimed, revoked, expired and never-existed are one page
//!   with one set of bytes, exactly like a dead verification link: which of
//!   the four it was is not the visitor's business, and "already claimed"
//!   would tell whoever forwarded the link that somebody used it.
//! - **No say in the email.** The account gets the address the invite was
//!   sent to. Letting the form change it would turn "this address received
//!   the link" into "this address was typed into a text field", which is the
//!   difference between proof and claim.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;

use super::intake::{form_values, page, shell};
use super::{v1, Shared};
use crate::config::{Origin, Role};
use crate::db::Claim;

/// One page for every kind of dead invite. Bytes-identical on purpose.
fn nothing_here() -> Response {
    page(
        StatusCode::NOT_FOUND,
        shell(
            "That invite is not valid",
            "<h1>That invite is not valid</h1>\
             <p>Invite links work once and for a limited time. If you were \
             expecting this one to work, ask whoever sent it for a fresh \
             one.</p>",
            "",
        ),
    )
}

/// The claim form. `error` is this page's whole state: `None` first time,
/// the reason on a re-render — with whatever they typed kept, because
/// retyping a rejected name is how typos become permanent handles.
fn claim_form(token: &str, attempted: &str, error: Option<&str>) -> Response {
    let error_html = match error {
        Some(text) => format!(
            "<p role=\"alert\"><strong>{}</strong></p>",
            mecha_manifest::escape_text(text)
        ),
        None => String::new(),
    };
    let body = format!(
        "<h1>Claim your handle</h1>\
         <p>Your handle is the name your pages live under: \
         <code>&lt;handle&gt;.art.…</code> — lowercase letters, digits and \
         hyphens, up to 63 characters. <strong>It is permanent</strong>: it \
         can never be changed, and once retired it is never reissued.</p>\
         {error_html}\
         <form method=\"post\" action=\"/signup/{token}\">\
         <label for=\"handle\">Handle</label>\
         <input id=\"handle\" name=\"handle\" value=\"{attempted}\" required \
         maxlength=\"63\" autocomplete=\"off\" spellcheck=\"false\">\
         <button type=\"submit\">Claim it</button>\
         </form>",
        token = mecha_manifest::escape_text(token),
        attempted = mecha_manifest::escape_text(attempted),
    );
    page(
        if error.is_some() {
            // Understood and refused: a 200 would tell a scripted client it
            // had succeeded, the same rule the intake forms follow.
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        },
        shell("Claim your handle", &body, &format!("/signup/{token}/")),
    )
}

/// `GET /signup/<token>` — the form, or the one dead-invite page.
pub async fn form(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(token): Path<String>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let hash = crate::intake::hash_token(&token);
    match app.db.invite_by_token(&hash, &crate::db::now()) {
        Ok(Some(_)) => claim_form(&token, "", None),
        Ok(None) => nothing_here(),
        Err(e) => {
            tracing::error!(error = %e, "reading an invite");
            super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `POST /signup/<token>` — the claim.
pub async fn submit(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(token): Path<String>,
    body: String,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }

    // Trimmed and lowercased before validation: a person typing `Alice` on a
    // phone that capitalised it for them means `alice`, and a hostname is
    // lowercase anyway. What is *not* forgiven is anything `valid_handle`
    // refuses after that — silently mangling `alice_c` into something legal
    // would hand somebody a permanent name they never chose.
    let attempted = form_values(&body)
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let hash = crate::intake::hash_token(&token);
    if let Err(e) = crate::config::valid_handle(&attempted) {
        // The invite is checked even on the invalid-handle path, so a dead
        // link never gets a form back. Order matters: the page must not
        // reveal more to a dead token than `GET` would.
        return match app.db.invite_by_token(&hash, &crate::db::now()) {
            Ok(Some(_)) => claim_form(&token, &attempted, Some(&e.to_string())),
            Ok(None) => nothing_here(),
            Err(e) => {
                tracing::error!(error = %e, "reading an invite");
                super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                    .into_response()
            }
        };
    }

    match app.db.invite_claim(&hash, &attempted, &crate::db::now()) {
        Ok(Claim::Created(user)) => {
            tracing::info!(handle = %user.handle, "handle claimed from an invite");
            let art = app.config.user_url(Role::Artifacts, &user.handle);
            let compute = app.config.user_url(Role::Compute, &user.handle);
            let body = format!(
                "<h1>You are <code>{handle}</code></h1>\
                 <p>Your pages will live at \
                 <a href=\"{art}\">{art}</a> and notebooks at \
                 <a href=\"{compute}\">{compute}</a>.</p>\
                 <p>A certificate for those names is being ordered now — they \
                 start answering within a minute or two.</p>\
                 <p>Next, connect the machine that will publish for you. That \
                 flow is on its way; today it is a key the operator mints \
                 for you.</p>",
                handle = mecha_manifest::escape_text(&user.handle),
                art = mecha_manifest::escape_text(&art),
                compute = mecha_manifest::escape_text(&compute),
            );
            page(
                StatusCode::OK,
                shell("Welcome", &body, &format!("/signup/{token}/")),
            )
        }
        // Taken is the one refusal with detail, and the detail is one bit.
        // Never whose, never since when, and never whether it is live or
        // retired — the claim form is reachable by anyone holding a link, so
        // anything more would be a directory query with extra steps.
        Ok(Claim::HandleTaken) => claim_form(
            &token,
            &attempted,
            Some(&format!("`{attempted}` is not available")),
        ),
        Ok(Claim::InviteGone) => nothing_here(),
        Err(e) => {
            tracing::error!(error = %e, "claiming a handle");
            super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `GET /signup/<token>/<name>` — the stylesheet.
///
/// Served whether or not the token is live: there is one stylesheet for
/// everyone, so answering it for a dead token reveals nothing — and refusing
/// it would make the dead-invite page distinguishable by its missing CSS.
pub async fn asset(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((_token, name)): Path<(String, String)>,
) -> Response {
    super::intake::serve_asset(&app, &origin, &name)
}
