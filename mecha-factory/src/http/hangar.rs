//! `GET /@{handle}` — everything one person has made public, in one place.
//!
//! The lines are **wired from the inventory**, not declared: whatever is
//! public is on it, grouped by what the thing is for. A switchboard
//! (`/@{handle}/{slug}`) is the same page with its lines patched by hand.
//!
//! Four decisions hold it up.
//!
//! **It is a view, never a permission.** Every line comes from
//! [`crate::inventory::Reach`], which is computed once and by the same
//! reasoning `http/artifacts.rs` uses before it will serve a byte. This page
//! can only ever subtract from what visibility already allows, and it must
//! never reach its own conclusion about who may read what — the *title* of a
//! private bundle is the leak, long before the bytes are.
//!
//! **A disabled hangar answers what a nonexistent one answers**, byte for
//! byte: a suspended user, a retired handle, a handle that never existed, and
//! a person who simply has not turned their page on are one refusal. Anything
//! else makes the 404 a directory of who has an account here.
//!
//! **A bundle line goes through the viewer**, `/view/{handle}/{id}`, and not
//! to the artifact origin's share URL. The reader keeps the header and the
//! styling on every artifact instead of being dropped onto a bare origin, the
//! owner gets the release controls in place, and the two-segment spelling
//! follows the alias — a version-pinned line would go stale silently, still
//! working, showing last month.
//!
//! **The chrome is ours and it stays.** An earlier draft of the design said a
//! public page should carry none, to give future custom CSS nothing to spoof.
//! `http/viewer.rs` already makes the better argument: the chrome is safe
//! because it is ours, on our origin, wrapped around content that is framed
//! rather than inlined. What survives of the objection is narrow and already
//! lives in the type — `Chrome::Public { sign_in: false }`, the same answer a
//! stranger's form takes, because a page in somebody's email signature should
//! not offer a sign-in box to a visitor with no account.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use mecha_manifest::escape_text as esc;

use super::intake::{page, shell, shell_with, Chrome};
use super::{account, v1, Shared};
use crate::config::{Origin, Role};
use crate::db::UserRow;
use crate::inventory::Inventory;

/// The one refusal. See the module doc: every way of not having a page here
/// answers identically.
pub(crate) fn nothing_here() -> Response {
    page(
        StatusCode::NOT_FOUND,
        shell("Not found", "<h1>Not found</h1>", ""),
    )
}

/// Whose page, if there is one to serve.
///
/// Returns the profile too, because `enabled` is part of "is there a page"
/// rather than a second question asked afterwards.
pub(crate) fn owner(app: &Shared, handle: &str) -> Option<(UserRow, mecha_manifest::Profile)> {
    let user = match app.db.user_by_handle(handle) {
        Ok(Some(user)) if user.active() => user,
        _ => return None,
    };
    let profile = read_profile(app, &user);
    Some((user, profile))
}

/// The stored profile, or an empty one. A record that no longer parses reads
/// as absent rather than failing the page: the owner sees the error in the
/// cockpit, and a stranger does not need to know the difference.
pub(crate) fn read_profile(app: &Shared, user: &UserRow) -> mecha_manifest::Profile {
    app.db
        .record_get(&user.id, crate::db::RECORD_PROFILE, "")
        .ok()
        .flatten()
        .and_then(|row| mecha_manifest::Profile::from_toml(&row.effective).ok())
        .unwrap_or_default()
}

/// A board record by slug, if it is there and still parses.
pub(crate) fn read_board(
    app: &Shared,
    user: &UserRow,
    slug: &str,
) -> Option<mecha_manifest::Board> {
    app.db
        .record_get(&user.id, crate::db::RECORD_BOARD, slug)
        .ok()
        .flatten()
        .and_then(|row| mecha_manifest::Board::from_toml(&row.effective).ok())
}

/// The chrome for whoever is looking: the owner gets their dropdown, and a
/// stranger gets the mark and nothing to sign into.
pub(crate) fn chrome_for(app: &Shared, headers: &HeaderMap, user: &UserRow) -> Chrome {
    match account::session(app, headers) {
        Some((token, viewer)) if viewer.id == user.id => Chrome::Account {
            handle: viewer.handle.clone(),
            email: viewer.email.clone(),
            csrf: account::csrf(&token),
            docs_url: app.config.docs_url.clone(),
        },
        _ => Chrome::Public {
            docs_url: app.config.docs_url.clone(),
            sign_in: false,
        },
    }
}

/// The identity block: who this is, and where else they are.
pub(crate) fn masthead(profile: &mecha_manifest::Profile, user: &UserRow) -> String {
    let name = profile.display_name.as_deref().unwrap_or(&user.handle);
    let mut out = format!("<h1>{}</h1>", esc(name));
    if let Some(tagline) = &profile.tagline {
        out.push_str(&format!("<p class=\"intro\">{}</p>", esc(tagline)));
    }
    if let Some(bio) = &profile.bio {
        out.push_str(&format!("<p>{}</p>", esc(bio)));
    }
    let mut facts = Vec::new();
    if let Some(location) = &profile.location {
        facts.push(esc(location));
    }
    if let Some(tz) = &profile.timezone {
        facts.push(esc(tz));
    }
    if !facts.is_empty() {
        out.push_str(&format!("<p class=\"muted\">{}</p>", facts.join(" · ")));
    }
    if !profile.links.is_empty() {
        out.push_str("<nav class=\"links\">");
        for link in &profile.links {
            // A label if there is one, else the kind's own name, else the
            // host — which is the honest fallback for a link we know nothing
            // about, and the same rule a switchboard's `link` line follows.
            let label = match (&link.label, link.kind) {
                (Some(label), _) => label.clone(),
                (None, mecha_manifest::LinkKind::Other) => host_of(&link.url),
                (None, kind) => format!("{kind:?}"),
            };
            out.push_str(&format!(
                "<a href=\"{}\">{}</a> ",
                esc(&link.url),
                esc(&label)
            ));
        }
        out.push_str("</nav>");
    }
    out
}

/// The host out of a URL, for showing where an off-origin link goes.
///
/// String work rather than a URL parser: the value already passed the
/// manifest's `http(s)`-only check, so what is left is display, and a
/// dependency for one `split` would be the wrong trade.
pub(crate) fn host_of(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest))
        .unwrap_or(url)
        .to_string()
}

/// One group of lines.
fn group(title: &str, lines: Vec<String>) -> String {
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "<h2>{}</h2><ul class=\"lines\">{}</ul>",
        esc(title),
        lines.join("")
    )
}

fn line(gate: &str, path: &str, label: &str, note: &str) -> String {
    let note = if note.is_empty() {
        String::new()
    } else {
        format!("<span class=\"muted\"> — {}</span>", esc(note))
    };
    format!(
        "<li><a href=\"{gate}{}\">{}</a>{note}</li>",
        esc(path),
        esc(label)
    )
}

/// `GET /@{handle}`.
pub async fn show(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    Path(handle): Path<String>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((user, profile)) = owner(&app, &handle) else {
        return nothing_here();
    };
    // The toggle. Note what it does *not* govern: a switchboard keeps working
    // when this is off, because its URL is in somebody's email signature and
    // those are separate publications with separate audiences.
    if !profile.enabled {
        return nothing_here();
    }

    let board = read_board(&app, &user, "");
    let inv = Inventory::read(&app.db, &user);
    let gate = app.config.base_url(Role::Gate);

    let mut body = String::new();
    match board.as_ref().and_then(|b| b.heading.as_deref()) {
        // A hangar's own heading replaces the masthead's name, when it has
        // one; the rest of the identity block stays either way.
        Some(heading) => {
            body.push_str(&format!("<h1>{}</h1>", esc(heading)));
            let mut rest = masthead(&profile, &user);
            if let Some(cut) = rest.find("</h1>") {
                rest = rest[cut + 5..].to_string();
            }
            body.push_str(&rest);
        }
        None => body.push_str(&masthead(&profile, &user)),
    }
    if let Some(intro) = board.as_ref().and_then(|b| b.intro.as_deref()) {
        body.push_str(&format!("<p>{}</p>", esc(intro)));
    }

    // Grouped by what the thing is *for*, never by content class — `static`
    // versus `compute` is a CSP axis and no reader has ever cared about it.
    body.push_str(&group(
        "Talk to me",
        inv.bookings
            .iter()
            .chain(inv.forms.iter())
            .filter_map(|t| {
                t.reach
                    .path()
                    .map(|path| line(&gate, path, &t.title, &t.id))
            })
            .collect(),
    ));
    body.push_str(&group(
        "Reports",
        inv.bundles
            .iter()
            .filter_map(|b| {
                b.reach
                    .path()
                    .map(|path| line(&gate, path, &b.title, &b.id))
            })
            .collect(),
    ));
    body.push_str(&group(
        "Polls",
        inv.polls
            .iter()
            .filter_map(|p| p.reach.path().map(|path| line(&gate, path, &p.title, "")))
            .collect(),
    ));

    if inv.public_count() == 0 {
        // Honest rather than empty. A page that says nothing reads as broken;
        // one that says there is nothing yet reads as early.
        body.push_str("<p class=\"muted\">Nothing public here yet.</p>");
    }

    let chrome = chrome_for(&app, &headers, &user);
    page(
        StatusCode::OK,
        shell_with(
            profile.display_name.as_deref().unwrap_or(&user.handle),
            &body,
            "/@a/",
            &chrome,
        ),
    )
}

/// `GET /@a/{name}` — the stylesheet and the dropdown's script.
///
/// Handle-free on purpose: the assets are the gate's own and identical for
/// everyone, so putting a handle in the path would make one cached file per
/// tenant and hand a scanner a way to ask whether a handle exists.
pub async fn asset(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(name): Path<String>,
) -> Response {
    super::intake::serve_asset(&app, &origin, &name)
}

/// The artifact origin's root, which used to serve nothing.
///
/// A person who has only ever seen an artifact URL will try its root, and a
/// redirect is a better answer than silence. It goes to the gate because that
/// is where the page lives — see the design's §3.1.
pub async fn artifact_root(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
) -> Response {
    let Some(handle) = origin.handle.as_deref() else {
        return nothing_here();
    };
    if origin.role != Role::Artifacts {
        return nothing_here();
    }
    let target = format!("{}/@{handle}", app.config.base_url(Role::Gate));
    (
        StatusCode::FOUND,
        [(axum::http::header::LOCATION, target)],
        "",
    )
        .into_response()
}
