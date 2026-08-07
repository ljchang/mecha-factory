//! The signed-in artifact viewer, on the gate.
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
//! What this page never does: serve a bundle's bytes. The frame loads them
//! from the artifact origin under the artifact policy, so a private bundle
//! frames as the same 404 the world sees — which for the owner is not a bug
//! but the truth about what is currently reachable.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;

use super::intake::account_dropdown;
use super::{account, v1, Shared};
use crate::config::{Origin, Role};

fn missing() -> Response {
    super::Failure::text(StatusCode::NOT_FOUND, "not found").into_response()
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
    if mecha_manifest::valid_id(&id, "bundle id").is_err() {
        return missing();
    }
    let Ok(Some(owner)) = app.db.user_by_handle(&handle) else {
        return missing();
    };
    if !owner.active() {
        return missing();
    }

    let session = account::session(&app, &headers);
    let is_owner = session
        .as_ref()
        .is_some_and(|(_, user)| user.id == owner.id);

    // What the alias says. A non-owner gets exactly the artifact origin's
    // rules: public and aliased, or nothing. The owner sees the viewer for
    // their own bundle whatever its state — the controls are the point, and
    // the frame below shows what the world currently gets.
    let alias = app.db.alias(&owner.id, &id).ok().flatten();
    let (live, visibility) = match &alias {
        Some(alias) => (alias.version, alias.visibility),
        None => (None, mecha_manifest::Visibility::Private),
    };
    let public = visibility == mecha_manifest::Visibility::Public;
    if !is_owner && (!public || live.is_none()) {
        return missing();
    }

    let versions = app.db.bundle_versions(&owner.id, &id).unwrap_or_default();
    if !versions.contains(&version) {
        return missing();
    }

    // Which origin the frame loads from is the class's decision, exactly as
    // it is when a reader arrives directly.
    let class = match app.db.bundle(&owner.id, &id, version) {
        Ok(Some(row)) => row.class,
        _ => return missing(),
    };
    let art_base = app.config.user_url(Role::for_class(class), &handle);
    let frame_src = format!("{art_base}/b/{id}/v/{version}/");
    let esc = mecha_manifest::escape_text;

    // The version menu.
    let mut version_links = String::new();
    for v in &versions {
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

    // The owner's controls, in their own menu: the same /account/release the
    // account page posts, with a return address back here. Absent entirely
    // for everyone else — not disabled, absent.
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
                        .map(|v| format!(
                            "<input type=\"hidden\" name=\"version\" value=\"{v}\">"
                        ))
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
                items.push_str(&form(
                    live.or(Some(version)),
                    "public",
                    "Release publicly",
                ));
            }
            if live.is_some() {
                items.push_str(&form(None, "private", "Take down"));
            }
            let state = match (public, live) {
                (true, Some(v)) => format!("public — share URL shows v{v}"),
                (true, None) => "public, but pointing at nothing".into(),
                (false, Some(v)) => format!("private — v{v} staged, served to nobody"),
                (false, None) => "private, pointing at nothing".into(),
            };
            let _ = user;
            format!(
                "<details class=\"account-menu\">\
                 <summary>Manage</summary>\
                 <div class=\"menu\"><p>{state}</p>{items}</div></details>",
            )
        }
        _ => String::new(),
    };

    // The identity slot: the real account dropdown for a session, the
    // sign-in dropdown otherwise — the same corner as everywhere else.
    let identity = match &session {
        Some((token, user)) => account_dropdown(
            &user.handle,
            &user.email,
            &account::csrf(token),
            app.config.docs_url.as_deref(),
        ),
        None => "<details class=\"account-menu\">\
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
         <a href=\"{frame_src}\">bare page</a>\
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

