//! The typed way in: a form, a link, and a row.
//!
//! ```text
//!   GET  /f/<handle>/<type>            the form, rendered from the manifest
//!   POST /f/<handle>/<type>            validated → `submitted` + a link sent
//!   GET  /f/<handle>/<type>/c/<token>  clicked → `queued`, and the confirmation
//! ```
//!
//! **Nothing here requires an agent.** That is §14.4's invariant and it is the
//! whole reason a form keeps working when the mecha that owns it is off: the
//! origin renders from a manifest mecha uploaded earlier, validates against a
//! schema mecha uploaded earlier, and answers with a confirmation mecha wrote
//! earlier. A detached instrument degrades from responsive to collecting, which
//! is what every form product does when nobody reads the responses — they still
//! land.
//!
//! Four rules, each of which is the difference between a form and a liability:
//!
//! - **Unverified never enters the queue.** `submitted` is not drainable; only
//!   a clicked link promotes a row to `queued`. Verification is what stands
//!   between an unauthenticated endpoint and a triage run per stranger, and it
//!   is why an unverified row costs nothing but a little disk.
//! - **The link is single-use and stored as a hash.** Reading the ledger off a
//!   lost box does not let anyone verify somebody else's submission, and a
//!   forwarded link works once.
//! - **A stranger is never told whether an address exists**, whether a type is
//!   somebody's, or whether their row was accepted for any reason other than
//!   shape. The only thing the response varies on is validation, which is their
//!   own form coming back with errors on it.
//! - **Sending is budgeted per recipient and per user.** Verification means
//!   this box sends mail to strangers on a user's behalf, which is a spam
//!   cannon with a form in front of it. Forty mails to one person is abuse;
//!   forty people once may be a conference.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Extension;
use mecha_manifest::{FormOptions, RequestType};

use super::{v1, Failure, Shared};
use crate::config::Origin;
use crate::db::UserRow;

/// A page, under the gate's own policy. The middleware adds the headers.
pub(crate) fn page(status: StatusCode, body: String) -> Response {
    (status, Html(body)).into_response()
}

/// The wrapper every non-form page uses.
///
/// `assets` is a **root-relative** prefix, for the same reason the form's is:
/// these pages are served at three different depths — `/f/<h>/<t>` for the
/// "check your email" page and `/f/<h>/<t>/c/<token>` for the confirmation —
/// and a relative `form.css` resolved differently from each, which is to say
/// wrongly from all of them. An empty prefix means "no stylesheet worth
/// resolving", which is what a 404 that must not reveal whether a handle
/// exists should say.
pub(crate) fn shell(title: &str, body: &str, assets: &str) -> String {
    let style = if assets.is_empty() {
        String::new()
    } else {
        format!(
            "<link rel=\"stylesheet\" href=\"{}form.css\">",
            mecha_manifest::escape_text(assets)
        )
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>{style}</head>\n\
         <body><main>{body}</main></body></html>\n",
        mecha_manifest::escape_text(title)
    )
}

/// Whose form, and which one — or nothing, in a way that says nothing.
///
/// A stranger learns the same thing from "no such user", "suspended user", "no
/// such type" and "a type that cannot be served": that there is no form here.
/// Distinguishing them would turn this endpoint into a directory of who has an
/// account and what they collect.
fn resolve(app: &Shared, handle: &str, type_id: &str) -> Option<(UserRow, RequestType)> {
    let user = match app.db.user_by_handle(handle) {
        Ok(Some(user)) if user.active() => user,
        _ => return None,
    };
    let stored = app.db.type_get(&user.id, type_id).ok()??;
    let parsed = RequestType::from_toml(&stored.manifest).ok()?;
    // Refused rather than served unverified: see `RequestType::servable`.
    parsed.servable().ok()?;
    Some((user, parsed))
}

fn nothing_here() -> Response {
    page(
        StatusCode::NOT_FOUND,
        // No stylesheet: this page is served for an unknown handle, an
        // unknown type and a suspended user alike, so it must not resolve an
        // asset path that would tell them apart.
        shell(
            "Not found",
            "<h1>Not found</h1><p>There is no form here.</p>",
            "",
        ),
    )
}

/// `GET /f/<handle>/<type>` — the form.
pub async fn form(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, type_id)): Path<(String, String)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((_, request_type)) = resolve(&app, &handle, &type_id) else {
        return nothing_here();
    };
    render_form(
        &request_type,
        &handle,
        &type_id,
        &app.config.theme,
        Default::default(),
        &[],
    )
}

fn render_form(
    request_type: &RequestType,
    handle: &str,
    type_id: &str,
    theme: &str,
    values: serde_json::Map<String, serde_json::Value>,
    errors: &[mecha_manifest::ValidationError],
) -> Response {
    let rendered = request_type.form(&FormOptions {
        action: format!("/f/{handle}/{type_id}"),
        // Root-relative, so the same document works wherever it is served
        // from — the form URL, a re-render after validation errors, and the
        // confirmation page, which sits one path segment deeper.
        assets: format!("/f/{handle}/{type_id}/"),
        theme: mecha_manifest::Theme::by_name(theme),
        token: None,
        values,
        errors: errors.to_vec(),
        step: None,
    });
    let status = if errors.is_empty() {
        StatusCode::OK
    } else {
        // The submission was understood and refused. A 200 here would tell a
        // scripted client it had succeeded.
        StatusCode::BAD_REQUEST
    };
    // `form()` renders a whole document — head, stylesheet link and all — so
    // this serves it as it is. Wrapping it in a second shell produced a page
    // with two doctypes, which a test caught and a browser would have
    // forgiven, quietly.
    page(status, rendered.html)
}

/// `GET /f/<handle>/<type>/form.css` — the stylesheet the form references.
///
/// Served from the manifest crate rather than from disk: there is one form
/// stylesheet, it ships in the binary, and a form that renders differently
/// depending on what is on the box would be a form nobody could check.
pub async fn asset(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((_handle, _type_id, name)): Path<(String, String, String)>,
) -> Response {
    serve_asset(&app, &origin, &name)
}

/// The named built-in asset, from whichever route wants it — the intake forms
/// and the signup page reference the same stylesheet, and serving it from one
/// place is what keeps them agreeing about what it is.
pub(crate) fn serve_asset(app: &Shared, origin: &Origin, name: &str) -> Response {
    if v1::not_on_gate(origin).is_some() {
        return nothing_here();
    }
    let page = RequestType {
        id: "x".into(),
        version: 1,
        title: String::new(),
        description: None,
        retain_days: None,
        fields: Vec::new(),
        steps: Vec::new(),
        acknowledgments: Vec::new(),
        verification: None,
        confirmation: None,
    }
    .form(&FormOptions {
        theme: mecha_manifest::Theme::by_name(&app.config.theme),
        ..FormOptions::default()
    });
    for (asset_name, body) in page.assets() {
        if *asset_name == *name {
            return (
                StatusCode::OK,
                [(
                    header::CONTENT_TYPE,
                    mecha_manifest::content_type(asset_name),
                )],
                // Owned: the stylesheet is built from the theme, so it does
                // not outlive the page it came from.
                body.to_string(),
            )
                .into_response();
        }
    }
    nothing_here()
}

/// `POST /f/<handle>/<type>` — a submission.
pub async fn submit(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, type_id)): Path<(String, String)>,
    body: String,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((user, request_type)) = resolve(&app, &handle, &type_id) else {
        return nothing_here();
    };
    let verification = match request_type.servable() {
        Ok(verification) => verification,
        Err(_) => return nothing_here(),
    };

    let raw = form_values(&body);
    let submission = match request_type.validate(&raw) {
        Ok(submission) => submission,
        // Their own form back, with the errors on it. This is the only thing
        // the response varies on, deliberately.
        Err(errors) => {
            return render_form(
                &request_type,
                &handle,
                &type_id,
                &app.config.theme,
                raw,
                &errors,
            )
        }
    };

    let Some(address) = submission
        .values
        .get(&verification.field)
        .and_then(|v| v.as_str())
    else {
        return nothing_here();
    };
    let recipient = crate::intake::recipient_hash(address);

    // Budgets, before anything is written. Both directions matter: one address
    // hammered is abuse, and one user's form mailing thousands is the thing
    // that costs a domain its reputation.
    match app
        .db
        .sends_today(&user.id, &recipient, &crate::db::today())
    {
        Ok((to_recipient, by_user)) => {
            if to_recipient >= crate::intake::PER_RECIPIENT_PER_DAY
                || by_user >= user.send_budget.max(0)
            {
                tracing::warn!(
                    handle = %user.handle,
                    to_recipient,
                    by_user,
                    "verification send refused by budget"
                );
                // Told as a rate limit rather than as "you already applied":
                // whether an address has submitted before is not this
                // endpoint's to disclose.
                return page(
                    StatusCode::TOO_MANY_REQUESTS,
                    shell(
                        "Too many",
                        "<h1>Too many requests</h1><p>Please try again tomorrow.</p>",
                        &format!("/f/{handle}/{type_id}/"),
                    ),
                );
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "reading send budgets");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    }

    let token = crate::intake::mint_token();
    let payload = match serde_json::to_string(&submission.values) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::error!(error = %e, "serialising a submission");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    let row = crate::db::Submission {
        user_id: user.id.clone(),
        type_id: type_id.clone(),
        payload,
        created_at: crate::db::now(),
        // Retention starts at submission, not at verification: an abandoned
        // row is still a stranger's data sitting on the box.
        retain_until: request_type.retain_days.map(crate::db::days_from_now),
        verify_hash: crate::intake::hash_token(&token),
        verify_expires: crate::db::hours_from_now(verification.expires_hours),
        recipient_hash: recipient,
    };
    let seq = match app.db.submission_add(&row) {
        Ok(seq) => seq,
        Err(e) => {
            tracing::error!(error = %e, "storing a submission");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };

    let link = format!(
        "{}/f/{handle}/{type_id}/c/{token}",
        app.config.base_url(crate::config::Role::Gate)
    );
    app.mailer.send_verification(address, &request_type, &link);
    tracing::info!(handle = %user.handle, %type_id, seq, "submission awaiting verification");

    page(
        StatusCode::OK,
        shell(
            "Check your email",
            &format!(
                "<h1>Check your email</h1><p>A confirmation link is on its way to \
                 {}. Nothing reaches anyone until you click it, and the link \
                 works once.</p>",
                mecha_manifest::escape_text(address)
            ),
            &format!("/f/{handle}/{type_id}/"),
        ),
    )
}

/// `GET /f/<handle>/<type>/c/<token>` — the click that makes it real.
pub async fn confirm(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, type_id, token)): Path<(String, String, String)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((user, request_type)) = resolve(&app, &handle, &type_id) else {
        return nothing_here();
    };

    // Looked up by the hash of what was presented, so the stored value is a
    // verifier rather than a link somebody could replay off the disk.
    let hash = crate::intake::hash_token(&token);
    let verified = match app.db.submission_verify(&user.id, &hash, &crate::db::now()) {
        Ok(Some(row)) => row,
        Ok(None) => {
            // One page for expired, already-used, and never-existed. Which of
            // the three it was is not a stranger's business, and a "this link
            // was already used" would confirm to whoever forwarded it that
            // somebody clicked.
            return page(
                StatusCode::NOT_FOUND,
                shell(
                    "That link has expired",
                    "<h1>That link has expired</h1><p>Confirmation links work \
                     once and for a limited time. Submitting the form again \
                     will send a new one.</p>",
                    &format!("/f/{handle}/{type_id}/"),
                ),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "verifying a submission");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    tracing::info!(handle = %user.handle, %type_id, seq = verified.seq, "verified and queued");

    let values: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&verified.payload).unwrap_or_default();
    let body = match &request_type.confirmation {
        Some(confirmation) => {
            let mut html = format!(
                "<h1>{}</h1>\n<p>{}</p>\n",
                mecha_manifest::escape_text(&confirmation.heading),
                mecha_manifest::escape_text(confirmation.render(&values).trim())
            );
            if let Some(within) = &confirmation.expect_reply_within {
                html.push_str(&format!(
                    "<p class=\"intro\">You should hear back within {}.</p>\n",
                    mecha_manifest::escape_text(within)
                ));
            }
            html
        }
        // A type with no confirmation still confirms. Saying nothing after a
        // click is how somebody submits three times.
        None => "<h1>Confirmed</h1><p>Thank you — that is now with a person.</p>".to_string(),
    };
    page(
        StatusCode::OK,
        shell(
            &request_type.title,
            &body,
            &format!("/f/{handle}/{type_id}/"),
        ),
    )
}

/// `a=1&b=hello%20there` → a map of strings.
///
/// Strings, because that is what a form posts and what
/// `RequestType::validate` coerces from. Repeated keys keep the last, which is
/// what a browser means by a checkbox group's hidden default followed by its
/// checked value.
pub(crate) fn form_values(body: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(&key.replace('+', " "));
        if key.is_empty() {
            continue;
        }
        out.insert(
            key,
            serde_json::Value::String(percent_decode(&value.replace('+', " "))),
        );
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_body_decodes_the_way_a_browser_encodes_one() {
        let values = form_values("name=Ada+Lovelace&email=ada%40example.org&note=a%2Bb&empty=");
        assert_eq!(values["name"], "Ada Lovelace");
        assert_eq!(values["email"], "ada@example.org");
        assert_eq!(values["note"], "a+b", "an encoded plus is a plus");
        assert_eq!(values["empty"], "");
        assert!(form_values("").is_empty());
    }
}
