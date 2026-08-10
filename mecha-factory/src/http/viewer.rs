//! The signed-in artifact viewer, on the gate — and the reader's door.
//!
//! The inversion that makes owner chrome possible at all: rather than
//! carrying identity out to the artifact origins — where every tenant's
//! published scripts run, and where an ambient credential would be theirs to
//! spend — the viewer lives here, where the session already is, and frames
//! the bundle cross-origin. The gate page is ours, so it may know your
//! handle, wear the account dropdown, and hold the release controls, all on
//! the same CSRF machinery the account page uses. The framed bundle keeps
//! its own origin: its scripts cannot reach this page's DOM, cookies or
//! session, which is the browser's isolation and not a policy of ours.
//!
//! What this page never does: serve a bundle's bytes. For a public bundle
//! the frame loads them from the artifact origin exactly as the world does.
//! For a private one, the gate — having decided *here* that the visitor may
//! read — mints a short-lived **capability** and frames `/g/<cap>/` on the
//! artifact origin instead: the token in the frame URL is the entire
//! authority, the artifact origin holds no session and learns no identity,
//! and a reader's capability re-proves its grant on every fetch, so a
//! revoked share stops the bytes mid-page rather than at the token's expiry.
//!
//! Who may read a private bundle: its owner, and any address the owner
//! **shared** it with. A shared address proves itself the way everything
//! here does — a link mailed to it — and becomes a *viewer session*: the
//! third session surface, deliberately parallel to the other two and
//! deliberately none of them. Its cookie is `__Host-factory-viewer`, its
//! table joins on an **email** (never a user, never a key), and a tenant
//! signed in with a matching account email just works, because the grant
//! names an address and their session already proved one.
//!
//! Oracle rules, because a share must not leak what it protects: a visitor
//! with no session gets the same sign-in page whether the bundle is
//! private, unshared, or was never published; the sign-in form answers
//! identically whether the address has grants; and a signed-in reader whose
//! email is not on the list gets the same 404 a stranger gets.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use std::collections::HashMap;

use super::intake::{account_dropdown, form_values, page, shell, shell_with, Chrome};
use super::{account, artifacts, v1, Failure, Shared};
use crate::config::{Origin, Role};
use crate::db::UserRow;

/// The reader's cookie. A third name, so the three session surfaces cannot
/// even collide in a cookie jar.
const COOKIE: &str = "__Host-factory-viewer";

fn missing() -> Response {
    Failure::text(StatusCode::NOT_FOUND, "not found").into_response()
}

fn unavailable() -> Response {
    Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
}

/// The raw viewer-session token out of the Cookie header, if one came.
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

/// The reader's verified email, plus the raw token the CSRF value derives
/// from. `Ok(None)` is signed out; `Err` is the ledger failing, which is a
/// 5xx and never a sign-in page.
fn reader(app: &Shared, headers: &HeaderMap) -> Result<Option<(String, String)>, Box<Response>> {
    let Some(token) = cookie_token(headers) else {
        return Ok(None);
    };
    match app
        .db
        .viewer_session_email(&crate::intake::hash_token(&token), &crate::db::now())
    {
        Ok(Some(email)) => Ok(Some((token, email))),
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::error!(error = %e, "reading a viewer session");
            Err(Box::new(unavailable()))
        }
    }
}

/// A return path a redirect may follow: a viewer path, never an authority.
fn safe_return(path: &str) -> Option<&str> {
    (path.starts_with("/view/") && !path.starts_with("//")).then_some(path)
}

/// The sign-in page a session-less visitor gets on any private-or-absent
/// viewer URL. One page for every such URL — the only thing that varies is
/// the return path, which is the URL the visitor themselves asked for.
///
/// **It wears the tenant sign-in corner as well as the reader form**, and the
/// two are not interchangeable: the form in the body mails a link to an
/// address a *share* names, and answers a tenant identically to a stranger
/// because the owner of a bundle has no share to themselves. So an owner
/// arriving here signed out — which is now the ordinary way to arrive, since
/// a publish reports this URL and a private bundle is what a publish key can
/// produce — would fill the form in, be told a link was on its way, and
/// receive nothing. The corner is the door they actually need.
///
/// It costs the oracle nothing: the corner is the same on every refusal, so
/// private, unshared and never-published still answer identically.
fn reader_gate(app: &Shared, return_to: &str) -> Response {
    let esc = mecha_manifest::escape_text;
    page(
        StatusCode::OK,
        shell_with(
            "Sign in to view",
            &format!(
                "<h1>Sign in to view</h1>\
                 <p>There is nothing at this address for a visitor &mdash; \
                 but if it was shared with your email, prove the inbox and \
                 it will be here. A link is mailed to you; there is no \
                 password and no account.</p>\
                 <form method=\"post\" action=\"/view/signin\">\
                 <input type=\"hidden\" name=\"return\" value=\"{return_to}\">\
                 <label for=\"reader-email\">Email</label>\
                 <input id=\"reader-email\" name=\"email\" type=\"email\" required>\
                 <button type=\"submit\">Send the link</button></form>\
                 <p>Published this yourself? Sign in to your account from the \
                 corner instead &mdash; a bundle you own is not shared with \
                 you.</p>",
                return_to = esc(return_to),
            ),
            "/account/a/",
            &Chrome::Public {
                docs_url: app.config.docs_url.clone(),
                sign_in: true,
            },
        ),
    )
}

/// `POST /view/signin` — one answer, whoever asked, whatever they asked for.
pub async fn signin(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    body: String,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return missing();
    }
    let values = form_values(&body);
    let field = |name: &str| {
        values
            .get(name)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let email = crate::intake::normalize_email(&field("email"));
    let return_to = field("return");

    // Everything below must not change what the page says: the work happens
    // when an address actually holds a grant, and the answer is the same
    // either way — a form that said "no grants for you" would be a directory
    // of who has been shared with.
    if !email.is_empty() && email.contains('@') {
        match app.db.email_has_shares(&email) {
            Ok(true) => send_viewer_link(&app, &email, &return_to),
            Ok(false) => {}
            Err(e) => tracing::error!(error = %e, "checking an address for grants"),
        }
    }
    page(
        StatusCode::OK,
        shell(
            "Check your email",
            "<h1>Check your email</h1><p>If that address has pages shared \
             with it, a sign-in link is on its way. It works once and \
             expires in a few minutes.</p>",
            "/account/a/",
        ),
    )
}

fn send_viewer_link(app: &Shared, email: &str, return_to: &str) {
    let today = crate::db::today();
    match app.db.viewer_links_today(email, &today) {
        Ok(sent) if sent >= crate::intake::VIEWER_LINKS_PER_DAY => {
            tracing::warn!("reader link budget exhausted for an address today");
            return;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "counting reader links");
            return;
        }
    }
    let token = crate::intake::mint_token();
    let expires = (chrono::Utc::now()
        + chrono::Duration::minutes(crate::intake::VIEWER_LINK_EXPIRY_MINUTES))
    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    if let Err(e) = app.db.viewer_link_create(
        email,
        &crate::intake::hash_token(&token),
        &crate::db::now(),
        &expires,
    ) {
        tracing::error!(error = %e, "recording a reader link");
        return;
    }
    // The return path rides the link as a query so the click lands back on
    // the page that sent the reader here. Only a charset-clean viewer path
    // is carried; anything else signs in and lands on the front page.
    let to = safe_return(return_to)
        .filter(|path| {
            path.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        })
        .unwrap_or("");
    let link = format!(
        "{}/view/r/{token}{}",
        app.config.base_url(Role::Gate),
        if to.is_empty() {
            String::new()
        } else {
            format!("?to={to}")
        }
    );
    app.mailer.send_viewer_link(email, &link);
}

/// `GET /view/r/<token>` — one button, no database read; the same
/// scanner-proof split every token here wears.
pub async fn finish_page(Extension(origin): Extension<Origin>) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return missing();
    }
    page(
        StatusCode::OK,
        shell(
            "Sign in",
            "<h1>Almost in</h1>\
             <p>One click to finish &mdash; this is what keeps a mail \
             scanner's robot from spending your link before you can.</p>\
             <form method=\"post\">\
             <button type=\"submit\">Continue</button></form>",
            "/account/a/",
        ),
    )
}

/// `POST /view/r/<token>` — the link becomes a reader session.
pub async fn finish(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(token): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return missing();
    }
    let now = crate::db::now();
    let session_token = crate::intake::mint_token();
    let expires = (chrono::Utc::now()
        + chrono::Duration::days(crate::intake::VIEWER_SESSION_EXPIRY_DAYS))
    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // The transactional shape all three sign-ins share: the link is spent
    // only when the session lands.
    match app.db.viewer_signin(
        &crate::intake::hash_token(&token),
        &crate::intake::hash_token(&session_token),
        &now,
        &expires,
    ) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return page(
                StatusCode::NOT_FOUND,
                shell(
                    "That link has expired",
                    "<h1>That link has expired</h1><p>Sign-in links work \
                     once and briefly. Go back to the page you were sent \
                     and ask for another.</p>",
                    "",
                ),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "signing a reader in");
            return unavailable();
        }
    }
    let to = query
        .get("to")
        .and_then(|to| safe_return(to))
        .unwrap_or("/")
        .to_string();
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, to),
            (
                header::SET_COOKIE,
                super::session_cookie(
                    COOKIE,
                    &session_token,
                    crate::intake::VIEWER_SESSION_EXPIRY_DAYS * 24 * 60 * 60,
                ),
            ),
        ],
    )
        .into_response()
}

/// `POST /view/signout` — the reader session stops working now.
pub async fn signout(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return missing();
    }
    let Some(token) = cookie_token(&headers) else {
        return missing();
    };
    let sent = form_values(&body)
        .get("csrf")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if sent != account::csrf(&token) {
        return missing();
    }
    // The same rule the other two sign-outs learned: a failed revoke is a
    // failure, never a cleared cookie over a live session.
    if let Err(e) = app
        .db
        .viewer_session_revoke(&crate::intake::hash_token(&token), &crate::db::now())
    {
        tracing::error!(error = %e, "revoking a viewer session");
        return unavailable();
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/".to_string()),
            (header::SET_COOKIE, super::session_cookie(COOKIE, "", 0)),
        ],
    )
        .into_response()
}

/// `/view/{a}/{b}` — two very different pages behind one route shape. On an
/// artifact origin it is the old redirect to the gate viewer; on the gate it
/// is the **bare** viewer URL a share mail carries, which resolves to the
/// live version and redirects — so the mailed link stays stable while the
/// alias moves.
pub async fn two_seg(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    Path((a, b)): Path<(String, String)>,
) -> Response {
    if origin.role != Role::Gate {
        // The artifact-origin spelling: /view/{id}/{version}.
        let Ok(version) = b.parse::<u32>() else {
            return missing();
        };
        return artifacts::viewer_redirect(&app, &origin, &a, version);
    }
    let (handle, id) = (a, b);
    let here = format!("/view/{handle}/{id}");

    let session = account::session(&app, &headers);
    let reader = match reader(&app, &headers) {
        Ok(reader) => reader,
        Err(refusal) => return *refusal,
    };
    let anonymous = session.is_none() && reader.is_none();

    // Everything that is not a readable bundle answers the same way: the
    // sign-in page for a visitor with no session at all, the plain 404 for
    // one who has proved an identity that does not open this.
    let refused = || {
        if anonymous {
            reader_gate(&app, &here)
        } else {
            missing()
        }
    };

    if mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return refused();
    }
    let Ok(Some(owner)) = app.db.user_by_handle(&handle) else {
        return refused();
    };
    if !owner.active() {
        return refused();
    }
    let is_owner = session
        .as_ref()
        .is_some_and(|(_, user)| user.id == owner.id);
    let alias = app.db.alias(&owner.id, &id).ok().flatten();
    let live = alias.as_ref().and_then(|alias| alias.version);
    let public = alias
        .as_ref()
        .is_some_and(|alias| alias.visibility == mecha_manifest::Visibility::Public);

    let version = if is_owner {
        // The owner lands somewhere useful whatever the state: the live
        // version, or the newest one staged.
        match live.or_else(|| {
            app.db
                .bundle_versions(&owner.id, &id)
                .unwrap_or_default()
                .into_iter()
                .max()
        }) {
            Some(version) => version,
            None => return missing(),
        }
    } else {
        // Everyone else sees the version the alias names or nothing — a
        // share never opens the version history, only what is released.
        let Some(live) = live else {
            return refused();
        };
        if !public && !granted_email(&app, &owner.id, &id, &session, &reader) {
            return refused();
        }
        live
    };
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/view/{handle}/{id}/{version}"))],
    )
        .into_response()
}

/// The verified email this visitor speaks for, if any: the reader session's,
/// or a signed-in tenant's account address — a grant names an address, and a
/// tenant session already proved one.
fn visitor_email(
    session: &Option<(String, UserRow)>,
    reader: &Option<(String, String)>,
) -> Option<String> {
    reader.as_ref().map(|(_, email)| email.clone()).or_else(|| {
        session
            .as_ref()
            .map(|(_, user)| crate::intake::normalize_email(&user.email))
    })
}

fn granted_email(
    app: &Shared,
    owner_id: &str,
    id: &str,
    session: &Option<(String, UserRow)>,
    reader: &Option<(String, String)>,
) -> bool {
    let Some(email) = visitor_email(session, reader) else {
        return false;
    };
    app.db
        .share_allows(owner_id, id, &email)
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "checking a grant");
            false
        })
}

/// `GET /view/{handle}/{id}/{version}` — the framed viewer, signed in when
/// you are.
pub async fn view(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    Path((handle, id, version)): Path<(String, String, u32)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return missing();
    }
    let here = format!("/view/{handle}/{id}/{version}");

    let session = account::session(&app, &headers);
    let reader = match reader(&app, &headers) {
        Ok(reader) => reader,
        Err(refusal) => return *refusal,
    };
    let anonymous = session.is_none() && reader.is_none();
    let refused = || {
        if anonymous {
            reader_gate(&app, &here)
        } else {
            missing()
        }
    };

    if mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return refused();
    }
    let Ok(Some(owner)) = app.db.user_by_handle(&handle) else {
        return refused();
    };
    if !owner.active() {
        return refused();
    }

    let is_owner = session
        .as_ref()
        .is_some_and(|(_, user)| user.id == owner.id);

    // What the alias says. A non-owner gets the artifact origin's rules —
    // public and aliased — or, new, a live grant to their proven email,
    // which opens exactly the version the alias names. The owner sees the
    // viewer for their own bundle whatever its state.
    let alias = app.db.alias(&owner.id, &id).ok().flatten();
    let (live, visibility) = match &alias {
        Some(alias) => (alias.version, alias.visibility),
        None => (None, mecha_manifest::Visibility::Private),
    };
    let public = visibility == mecha_manifest::Visibility::Public;
    let granted = !is_owner && !public && granted_email(&app, &owner.id, &id, &session, &reader);
    if !is_owner {
        let allowed = (public && live.is_some()) || (granted && Some(version) == live);
        if !allowed {
            return refused();
        }
    }

    let versions = app.db.bundle_versions(&owner.id, &id).unwrap_or_default();
    if !versions.contains(&version) {
        return refused();
    }

    // Which origin the frame loads from is the class's decision, exactly as
    // it is when a reader arrives directly.
    let class = match app.db.bundle(&owner.id, &id, version) {
        Ok(Some(row)) => row.class,
        _ => return refused(),
    };
    let art_base = app.config.user_url(Role::for_class(class), &handle);

    // What the frame loads. Public-and-aliased is the plain URL, exactly
    // what the world fetches — the artifact origin serves every version of
    // a public bundle. Anything the gate authorised beyond that — the
    // owner's preview, a reader's grant — frames a fresh capability, which
    // is the only spelling of "these bytes, this once" the artifact origin
    // accepts for a private bundle.
    let frame_src = if public && live.is_some() {
        format!("{art_base}/b/{id}/v/{version}/")
    } else {
        let cap = crate::intake::mint_token();
        let expires = (chrono::Utc::now()
            + chrono::Duration::minutes(crate::intake::VIEW_CAP_EXPIRY_MINUTES))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let email = if is_owner {
            String::new()
        } else {
            visitor_email(&session, &reader).unwrap_or_default()
        };
        if let Err(e) = app.db.view_cap_create(
            &crate::db::ViewCapRow {
                user_id: owner.id.clone(),
                bundle_id: id.clone(),
                version,
                email,
            },
            &crate::intake::hash_token(&cap),
            &crate::db::now(),
            &expires,
        ) {
            tracing::error!(error = %e, "minting a view capability");
            return unavailable();
        }
        format!("{art_base}/g/{cap}/")
    };
    let esc = mecha_manifest::escape_text;

    // The version menu.
    let mut version_links = String::new();
    for v in &versions {
        // A reader sees one version; the menu only offers what the visitor
        // may open.
        if !is_owner && !public && *v != version {
            continue;
        }
        let marks = match (*v == version, Some(*v) == live) {
            (true, true) => " — viewing, live",
            (true, false) => " — viewing",
            (false, true) => " — live",
            (false, false) => "",
        };
        if *v == version {
            version_links.push_str(&format!("<strong>v{v}{marks}</strong>"));
        } else {
            version_links.push_str(&format!(
                "<a href=\"/view/{handle}/{id}/{v}\">v{v}{marks}</a>",
                handle = esc(&handle),
                id = esc(&id),
            ));
        }
    }

    // The owner's controls, in their own menu: release, and now the share
    // list — who may read this while it is private, each revocable, plus
    // the form that grants another address. Absent entirely for everyone
    // else — not disabled, absent.
    let manage = match &session {
        Some((token, user)) if is_owner => {
            let csrf = account::csrf(token);
            let here = format!("/view/{handle}/{id}/{version}");
            let form = |ver: Option<u32>, vis: &str, label: &str| {
                format!(
                    "<form method=\"post\" action=\"/account/release\">\
                     <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                     <input type=\"hidden\" name=\"id\" value=\"{id}\">\
                     {version}\
                     <input type=\"hidden\" name=\"visibility\" value=\"{vis}\">\
                     <input type=\"hidden\" name=\"return\" value=\"{here}\">\
                     <button type=\"submit\">{label}</button></form>",
                    id = esc(&id),
                    version = ver
                        .map(|v| format!("<input type=\"hidden\" name=\"version\" value=\"{v}\">"))
                        .unwrap_or_default(),
                )
            };
            let vis_word = if public { "public" } else { "private" };
            let mut items = String::new();
            if Some(version) != live {
                items.push_str(&form(
                    Some(version),
                    vis_word,
                    &format!("Point share URL at v{version}"),
                ));
            }
            if public {
                items.push_str(&form(live, "private", "Make private"));
            } else {
                items.push_str(&form(live.or(Some(version)), "public", "Release publicly"));
            }
            if live.is_some() {
                items.push_str(&form(None, "private", "Take down"));
            }
            let state = match (public, live) {
                (true, Some(v)) => format!("public — share URL shows v{v}"),
                (true, None) => "public, but pointing at nothing".into(),
                (false, Some(v)) => format!("private — v{v} shared by grant only"),
                (false, None) => "private, pointing at nothing".into(),
            };

            // The share ledger. A failed read refuses the page rather than
            // rendering "shared with nobody" over an error.
            let shares = match app.db.shares_for_bundle(&owner.id, &id) {
                Ok(shares) => shares,
                Err(e) => {
                    tracing::error!(error = %e, "reading a bundle's shares");
                    return unavailable();
                }
            };
            let mut share_items = String::new();
            for share in &shares {
                share_items.push_str(&format!(
                    "<form method=\"post\" action=\"/account/share-revoke\">\
                     <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                     <input type=\"hidden\" name=\"share\" value=\"{sid}\">\
                     <input type=\"hidden\" name=\"return\" value=\"{here}\">\
                     <button type=\"submit\">Unshare {email}</button></form>",
                    sid = esc(&share.id),
                    email = esc(&share.email),
                ));
            }
            let _ = user;
            format!(
                "<details class=\"account-menu\">\
                 <summary>Manage</summary>\
                 <div class=\"menu\"><p>{state}</p>{items}\
                 <p>Shared with {count} address{plural}. A reader sees the \
                 live version, signed in with the granted email.</p>\
                 {share_items}\
                 <form method=\"post\" action=\"/account/share\">\
                 <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                 <input type=\"hidden\" name=\"id\" value=\"{id}\">\
                 <input type=\"hidden\" name=\"return\" value=\"{here}\">\
                 <label for=\"share-email\">Share with</label>\
                 <input id=\"share-email\" name=\"email\" type=\"email\" required>\
                 <button type=\"submit\">Share</button></form>\
                 </div></details>",
                count = shares.len(),
                plural = if shares.len() == 1 { "" } else { "es" },
                id = esc(&id),
            )
        }
        _ => String::new(),
    };

    // The identity slot: the account dropdown for a tenant session, the
    // reader's email for a viewer session, the sign-in dropdown otherwise.
    let identity = match (&session, &reader) {
        (Some((token, user)), _) => account_dropdown(
            &user.handle,
            &user.email,
            &account::csrf(token),
            app.config.docs_url.as_deref(),
        ),
        (None, Some((token, email))) => format!(
            "<details class=\"account-menu\">\
             <summary>{email}</summary>\
             <div class=\"menu\">\
             <p>Signed in as a reader.</p>\
             <form method=\"post\" action=\"/view/signout\">\
             <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
             <button type=\"submit\">Sign out</button></form>\
             </div></details>",
            email = esc(email),
            csrf = account::csrf(token),
        ),
        (None, None) => "<details class=\"account-menu\">\
                 <summary>Sign in</summary>\
                 <div class=\"menu\">\
                 <p>Your page is reached by a link, not a password.</p>\
                 <form method=\"post\" action=\"/account/signin\">\
                 <label for=\"signin-email\">Email</label>\
                 <input id=\"signin-email\" name=\"email\" type=\"email\" required>\
                 <button type=\"submit\">Send the link</button>\
                 </form></div></details>"
            .to_string(),
    };

    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{id} — v{version}</title>\n\
         {favicon}\n\
         <link rel=\"stylesheet\" href=\"/account/a/form.css\">\
         <script src=\"/account/a/menu.js\" defer></script></head>\n\
         <body class=\"viewer\">\
         <header class=\"site\">\
         <a class=\"mark\" href=\"/\" aria-label=\"mecha\">{logo}</a>\
         <div class=\"site-right\">\
         <details class=\"account-menu\">\
         <summary>v{version} ▾</summary>\
         <div class=\"menu\">\
         <p><code>{handle}/{id}</code> — every version is immutable; the \
         share URL follows the live one.</p>\
         <nav>{version_links}\
         <a href=\"{art_base}/b/{id}/\">share URL</a></nav>\
         </div></details>\
         {manage}{identity}\
         </div></header>\n\
         <iframe class=\"artifact\" src=\"{frame_src}\" \
         title=\"{id} v{version}\"></iframe>\
         </body></html>\n",
        id = esc(&id),
        handle = esc(&handle),
        favicon = mecha_manifest::FAVICON_LINK,
        logo = mecha_manifest::LOGO_MONO_SVG,
    );

    // The gate's own policy plus exactly the frames this page exists to
    // hold: the two artifact origins of the handle being viewed.
    let frame_srcs = format!(
        "{} {}",
        app.config.user_url(Role::Artifacts, &handle),
        app.config.user_url(Role::Compute, &handle)
    );
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (
                header::CONTENT_SECURITY_POLICY,
                format!(
                    "default-src 'none'; style-src 'self'; script-src 'self'; \
                     img-src 'self' data:; frame-src {frame_srcs}; \
                     base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
                ),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response()
}
