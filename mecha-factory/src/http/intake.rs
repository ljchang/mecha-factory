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

/// What sits above a page's content, decided per call site — typed, so a
/// page cannot get account chrome by accident. There is deliberately no
/// `None` variant: the pages that must answer byte-identically whoever asks
/// are exactly the pages with an empty assets prefix, and `shell_with`
/// suppresses chrome on that signal — one rule, structural, instead of two
/// call-site decisions that could disagree.
pub(crate) enum Chrome {
    /// The mark, linked to the gate root — plus a Docs link when the
    /// deployment configured one, and a Sign in link where signing in is the
    /// next thing a person might want (the splash, not a stranger's form).
    Public {
        docs_url: Option<String>,
        sign_in: bool,
    },
    /// The mark plus the account dropdown. Exactly one page earns this.
    Account {
        handle: String,
        email: String,
        /// The session's CSRF token, because the dropdown holds the sign-out
        /// form and a form without it bounces off `mutating()`.
        csrf: String,
        docs_url: Option<String>,
    },
}

impl Chrome {
    fn render(&self) -> String {
        match self {
            Chrome::Public { docs_url, sign_in } => {
                let mut nav = String::new();
                if let Some(url) = docs_url {
                    nav.push_str(&format!(
                        "<a href=\"{}\">Docs</a>",
                        mecha_manifest::escape_text(url)
                    ));
                }
                if *sign_in {
                    nav.push_str("<a href=\"/account\">Sign in</a>");
                }
                if nav.is_empty() {
                    mecha_manifest::site_header()
                } else {
                    format!(
                        "<header class=\"site\">\
                         <a class=\"mark\" href=\"/\" aria-label=\"mecha\">{}</a>\
                         <nav>{nav}</nav></header>\n",
                        mecha_manifest::LOGO_MONO_SVG,
                    )
                }
            }
            Chrome::Account {
                handle,
                email,
                csrf,
                docs_url,
            } => format!(
                "<header class=\"site\">\
                 <a class=\"mark\" href=\"/account\" aria-label=\"mecha\">{logo}</a>\
                 <details class=\"account-menu\">\
                 <summary>{handle}</summary>\
                 <div class=\"menu\">\
                 <p>{email}</p>\
                 <nav><a href=\"#artifacts\">Artifacts</a>\
                 <a href=\"#machines\">Machines</a>{docs}</nav>\
                 <form method=\"post\" action=\"/account/pair\">\
                 <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                 <button type=\"submit\">Connect a machine</button></form>\
                 <form method=\"post\" action=\"/account/signout\">\
                 <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                 <button type=\"submit\">Sign out</button></form>\
                 </div></details></header>\n",
                logo = mecha_manifest::LOGO_MONO_SVG,
                handle = mecha_manifest::escape_text(handle),
                email = mecha_manifest::escape_text(email),
                csrf = mecha_manifest::escape_text(csrf),
                docs = match docs_url {
                    Some(url) => format!(
                        "<a href=\"{}\">Docs</a>",
                        mecha_manifest::escape_text(url)
                    ),
                    None => String::new(),
                },
            ),
        }
    }
}

/// The wrapper every non-form page uses.
///
/// `assets` is a **root-relative** prefix, for the same reason the form's is:
/// these pages are served at three different depths — `/f/<h>/<t>` for the
/// "check your email" page and `/f/<h>/<t>/c/<token>` for the confirmation —
/// and a relative `form.css` resolved differently from each, which is to say
/// wrongly from all of them. An empty prefix means "no stylesheet worth
/// resolving", which is what a 404 that must not reveal whether a handle
/// exists should say — and such a page takes `Chrome::None` too, since a
/// header referencing anything would be a second thing to prove identical.
pub(crate) fn shell(title: &str, body: &str, assets: &str) -> String {
    shell_with(
        title,
        body,
        assets,
        &Chrome::Public {
            docs_url: None,
            sign_in: false,
        },
    )
}

pub(crate) fn shell_with(title: &str, body: &str, assets: &str, chrome: &Chrome) -> String {
    let style = if assets.is_empty() {
        String::new()
    } else {
        format!(
            "<link rel=\"stylesheet\" href=\"{}form.css\">",
            mecha_manifest::escape_text(assets)
        )
    };
    // No stylesheet means a 404-class page; whatever the caller said, chrome
    // that references an asset universe the page has opted out of is wrong.
    let chrome = if assets.is_empty() {
        String::new()
    } else {
        chrome.render()
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>{style}</head>\n\
         <body>{chrome}<main>{body}</main></body></html>\n",
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
        // The public form never carries a file input — uploads happen after
        // verification, on their own page (see the upload flow).
        file_inputs: false,
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
    // Phase::Submit: file fields are neither due nor accepted here — the
    // upload step comes after the email is verified.
    let submission = match request_type.validate_at(&raw, mecha_manifest::Phase::Submit) {
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

/// `GET /f/<handle>/<type>/c/<token>` — one button, and no token touched.
///
/// The same scanner problem as the sign-in link, fixed the same way:
/// Microsoft Safe Links and its kind fetch a mail's URLs on delivery, so a
/// GET that spent the verification token verified submissions no human had
/// confirmed — and for a type with file fields, moved rows to the upload
/// step on a robot's initiative. The GET renders a Confirm button; the POST
/// below is the click that makes it real. The token is not even looked at
/// here — a page that varied on its state would be an oracle for the
/// scanner's GET.
pub async fn confirm_page(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, type_id, _token)): Path<(String, String, String)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((_, request_type)) = resolve(&app, &handle, &type_id) else {
        return nothing_here();
    };
    page(
        StatusCode::OK,
        shell(
            &request_type.title,
            &format!(
                "<h1>Confirm your email address</h1>\
                 <p>One click to verify — this is what keeps a mail scanner's \
                 robot from spending your link before you can. Confirming \
                 sends your {} request on its way.</p>\
                 <form method=\"post\">\
                 <button type=\"submit\">Confirm</button></form>",
                mecha_manifest::escape_text(&request_type.title)
            ),
            &format!("/f/{handle}/{type_id}/"),
        ),
    )
}

/// `POST /f/<handle>/<type>/c/<token>` — the click that makes it real.
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
    //
    // A type with file fields goes through the upload step instead of
    // straight to the queue. The upload token is minted here — the verify
    // token is spent by this very click, and reusing it would make the link
    // in the email replayable.
    let hash = crate::intake::hash_token(&token);
    let upload_token = request_type
        .has_file_fields()
        .then(crate::intake::mint_token);
    let next = match &upload_token {
        None => crate::db::VerifyNext::Queued,
        Some(t) => crate::db::VerifyNext::AwaitingUpload {
            upload_hash: crate::intake::hash_token(t),
            upload_expires: crate::db::hours_from_now(UPLOAD_EXPIRES_HOURS),
        },
    };
    let verified = match app.db.submission_verify(&user.id, &hash, &crate::db::now(), next) {
        Ok(Some(row)) => row,
        Ok(None) => {
            // One page for expired, already-used, and never-existed. Which of
            // the three it was is not a stranger's business, and a "this link
            // was already used" would confirm to whoever forwarded it that
            // somebody clicked.
            return expired_link(&handle, &type_id);
        }
        Err(e) => {
            tracing::error!(error = %e, "verifying a submission");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    tracing::info!(handle = %user.handle, %type_id, seq = verified.seq, "verified");

    let values: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&verified.payload).unwrap_or_default();

    if let Some(token) = upload_token {
        // Only file fields *visible under the submitted values* are asked
        // for: a conditional file field whose condition failed at submit time
        // is not a thing this requester was ever offered. If none are
        // visible, the row completes immediately through the same transaction
        // an upload would use — one path to `queued`, not two.
        let any_visible = request_type
            .visible_fields(&values)
            .into_iter()
            .any(|f| matches!(f.kind, mecha_manifest::FieldKind::File { .. }));
        if any_visible {
            // A redirect rather than a page: the confirm token is spent, so
            // the URL the browser lands on has to be one that can be
            // reloaded. The upload token is in the location, hashed at rest,
            // exactly like the confirm token was.
            return (
                StatusCode::SEE_OTHER,
                [(header::LOCATION, format!("/f/{handle}/{type_id}/u/{token}"))],
            )
                .into_response();
        }
        let completed = app.db.upload_complete(
            &user.id,
            &crate::intake::hash_token(&token),
            &crate::db::now(),
            &verified.payload,
            &[],
        );
        match completed {
            Ok(Some(_)) => {}
            Ok(None) => return nothing_here(),
            Err(e) => {
                tracing::error!(error = %e, "completing a fileless upload step");
                return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                    .into_response();
            }
        }
    }
    tracing::info!(handle = %user.handle, %type_id, seq = verified.seq, "queued");
    confirmation_page(&request_type, &handle, &type_id, &values)
}

/// The thank-you page, shown when a request actually reaches the queue —
/// after the click for plain types, after the upload for types with files.
fn confirmation_page(
    request_type: &RequestType,
    handle: &str,
    type_id: &str,
    values: &serde_json::Map<String, serde_json::Value>,
) -> Response {
    let body = match &request_type.confirmation {
        Some(confirmation) => {
            let mut html = format!(
                "<h1>{}</h1>\n<p>{}</p>\n",
                mecha_manifest::escape_text(&confirmation.heading),
                mecha_manifest::escape_text(confirmation.render(values).trim())
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

/// One page for an expired, already-used, and never-existed link alike.
fn expired_link(handle: &str, type_id: &str) -> Response {
    page(
        StatusCode::NOT_FOUND,
        shell(
            "That link has expired",
            "<h1>That link has expired</h1><p>Confirmation links work \
             once and for a limited time. Submitting the form again \
             will send a new one.</p>",
            &format!("/f/{handle}/{type_id}/"),
        ),
    )
}

/// How long a verified requester has to attach their files. Generous like the
/// verification window: the person already proved their address, and a CV is
/// sometimes on the other machine.
const UPLOAD_EXPIRES_HOURS: u32 = 48;

/// Resolve an upload token to its pending row, or the one expired page.
/// (The `Err` is boxed on clippy's advice — a `Response` is large, and this
/// returns through two hot handlers.)
fn upload_target(
    app: &Shared,
    handle: &str,
    type_id: &str,
    token: &str,
) -> Result<(UserRow, RequestType, crate::db::QueueRow, String), Box<Response>> {
    let Some((user, request_type)) = resolve(app, handle, type_id) else {
        return Err(Box::new(nothing_here()));
    };
    let hash = crate::intake::hash_token(token);
    match app.db.upload_pending(&user.id, &hash, &crate::db::now()) {
        Ok(Some(row)) => Ok((user, request_type, row, hash)),
        Ok(None) => Err(Box::new(expired_link(handle, type_id))),
        Err(e) => {
            tracing::error!(error = %e, "looking up an upload token");
            Err(Box::new(
                Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response(),
            ))
        }
    }
}

fn render_upload_form(
    request_type: &RequestType,
    handle: &str,
    type_id: &str,
    token: &str,
    theme: &str,
    values: serde_json::Map<String, serde_json::Value>,
    errors: &[mecha_manifest::ValidationError],
) -> Response {
    let rendered = request_type.upload_form(&FormOptions {
        action: format!("/f/{handle}/{type_id}/u/{token}"),
        assets: format!("/f/{handle}/{type_id}/"),
        theme: mecha_manifest::Theme::by_name(theme),
        token: None,
        values,
        errors: errors.to_vec(),
        step: None,
        file_inputs: true,
    });
    let status = if errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    page(status, rendered.html)
}

/// `GET /f/<handle>/<type>/u/<token>` — the upload page. A pure read, so it
/// survives a reload; only a successful POST spends the token.
pub async fn upload_page(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, type_id, token)): Path<(String, String, String)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let (_, request_type, row, _) = match upload_target(&app, &handle, &type_id, &token) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let values = serde_json::from_str(&row.payload).unwrap_or_default();
    render_upload_form(
        &request_type,
        &handle,
        &type_id,
        &token,
        &app.config.theme,
        values,
        &[],
    )
}

/// `POST /f/<handle>/<type>/u/<token>` — the files arrive, and the request
/// finally reaches the queue.
///
/// The only multipart handler in the binary. The claimed Content-Type and the
/// filename's extension are advisory throughout: the sniffed magic decides
/// what a file is, and the validator refuses what the field does not accept.
/// Order-independent over parts, because nothing here needs cross-part state
/// until validation — which runs once, over the stored values and the new
/// file metadata together, at Phase::Complete.
pub async fn upload(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Path((handle, type_id, token)): Path<(String, String, String)>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let (user, request_type, row, hash) = match upload_target(&app, &handle, &type_id, &token) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let stored: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&row.payload).unwrap_or_default();

    // A box that cannot afford the bytes says so now, loudly and temporarily,
    // rather than discovering a full disk under the ledger later.
    match app.attachments.free_bytes() {
        Ok(free) if free < app.config.limits.min_free_bytes => {
            tracing::warn!(free, "upload refused: disk headroom");
            return Failure::text(StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response();
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "reading disk headroom");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    }

    // Read every part. Only file fields exist on this form, so any other
    // part name is a probe, not a mistake worth a gentle error.
    let mut files: Vec<(String, String, Vec<u8>)> = Vec::new();
    let mut received: u64 = 0;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = %e, "unreadable multipart");
                return Failure::text(StatusCode::BAD_REQUEST, "unreadable upload")
                    .into_response();
            }
        };
        let Some(name) = field.name().map(str::to_string) else {
            return Failure::text(StatusCode::BAD_REQUEST, "unnamed part").into_response();
        };
        let is_file_field = request_type
            .field(&name)
            .is_some_and(|f| matches!(f.kind, mecha_manifest::FieldKind::File { .. }));
        if !is_file_field {
            return Failure::text(StatusCode::BAD_REQUEST, "not a field of this page")
                .into_response();
        }
        let filename = field.file_name().unwrap_or_default().to_string();
        let bytes = match field.bytes().await {
            Ok(bytes) => bytes,
            // The route-level body limit surfaces here as a read error.
            Err(_) => {
                return Failure::text(StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response()
            }
        };
        if filename.is_empty() && bytes.is_empty() {
            // An untouched file input: the browser sends an empty part. This
            // is what declining an optional attachment looks like.
            continue;
        }
        received += bytes.len() as u64;
        files.push((name, filename, bytes.to_vec()));
    }

    // Budgets on what actually arrived, before anything lands on disk. The
    // type's own budget bounds one request; the per-address budget bounds a
    // verified address that turns hostile. Counted even when validation
    // fails below — the bandwidth was spent either way.
    if received > request_type.attachment_budget() {
        return Failure::text(StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
    }
    let ip_hash = crate::intake::hash_token(&peer.ip().to_string());
    let today = crate::db::today();
    match app.db.upload_bytes_today(&ip_hash, &today) {
        Ok(already) if already.saturating_add(received as i64)
            > app.config.limits.daily_upload_bytes_per_ip as i64 =>
        {
            tracing::warn!("upload refused by the daily byte budget");
            return page(
                StatusCode::TOO_MANY_REQUESTS,
                shell(
                    "Too many",
                    "<h1>Too much for today</h1><p>Please try again tomorrow.</p>",
                    &format!("/f/{handle}/{type_id}/"),
                ),
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "reading the upload budget");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    }
    if received > 0 {
        if let Err(e) = app.db.upload_bytes_add(&ip_hash, &today, received as i64) {
            tracing::error!(error = %e, "recording upload bytes");
        }
    }

    // Measure the bytes, and let the one validator judge the measurements: a
    // sniff that recognises nothing becomes a content type no field accepts,
    // so mime spoofing and honest wrong-kind files fail through one path.
    let mut merged = stored.clone();
    let mut pending: Vec<(String, mecha_manifest::FileMeta, Vec<u8>)> = Vec::new();
    for (name, filename, bytes) in files {
        use sha2::Digest;
        let content_type = mecha_manifest::sniff(&bytes)
            .map(|t| t.mime().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let meta = mecha_manifest::FileMeta {
            filename,
            size: bytes.len() as u64,
            sha256: format!("sha256:{:x}", sha2::Sha256::digest(&bytes)),
            content_type,
            attachment_id: Some(crate::attachments::Store::mint_id()),
        };
        merged.insert(
            name.clone(),
            serde_json::to_value(&meta).expect("FileMeta serialises"),
        );
        pending.push((name, meta, bytes));
    }
    let submission = match request_type.validate_at(&merged, mecha_manifest::Phase::Complete) {
        Ok(submission) => submission,
        Err(errors) => {
            // Their page back, errors beside the inputs, the token still
            // live: no blob was written, so nothing orphans and nothing is
            // half-spent. The browser cannot repopulate a file input, so the
            // files must be chosen again — the price of never holding
            // unvalidated bytes.
            return render_upload_form(
                &request_type,
                &handle,
                &type_id,
                &token,
                &app.config.theme,
                stored,
                &errors,
            );
        }
    };

    // Blobs first, then the one transaction. A crash between the two leaves
    // files the orphan sweep reclaims — never a queued row missing its bytes.
    let now = crate::db::now();
    let mut rows = Vec::new();
    for (field_name, meta, bytes) in &pending {
        let id = meta.attachment_id.clone().expect("minted above");
        if let Err(e) = app.attachments.write(&user.id, &id, bytes) {
            tracing::error!(error = %e, "writing an attachment");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
        rows.push(crate::db::AttachmentRow {
            id,
            user_id: user.id.clone(),
            seq: row.seq,
            field: field_name.clone(),
            filename: meta.filename.clone(),
            content_type: meta.content_type.clone(),
            size: meta.size as i64,
            sha256: meta.sha256.clone(),
            created_at: now.clone(),
        });
    }
    let payload = match serde_json::to_string(&submission.values) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::error!(error = %e, "serialising a completed submission");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    match app.db.upload_complete(&user.id, &hash, &now, &payload, &rows) {
        Ok(Some(seq)) => {
            tracing::info!(handle = %user.handle, %type_id, seq, files = rows.len(), "queued");
            confirmation_page(&request_type, &handle, &type_id, &submission.values)
        }
        Ok(None) => {
            // Raced by its own expiry or a second tab that won. The blobs
            // just written belong to nothing; take them back out.
            for row in &rows {
                app.attachments.delete(&user.id, &row.id);
            }
            expired_link(&handle, &type_id)
        }
        Err(e) => {
            tracing::error!(error = %e, "completing an upload");
            for row in &rows {
                app.attachments.delete(&user.id, &row.id);
            }
            Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
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
