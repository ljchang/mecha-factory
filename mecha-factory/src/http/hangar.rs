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

use super::intake::{chrome_for, page, session_page, shell, shell_with};
use super::{v1, Shared};
use crate::config::{Origin, Role};
use crate::db::UserRow;
use crate::inventory::{host_of, Inventory, Line};

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
            let label = match (&link.label, link.kind) {
                (Some(label), _) => label.clone(),
                (None, mecha_manifest::LinkKind::Other) => host_of(&link.url),
                (None, kind) => format!("{kind:?}"),
            };
            // **The host is shown whenever the label does not account for
            // it.** A label is the author's own words and can say anything:
            // `label = "Sign in to the factory"` on `https://evil.example`,
            // rendered on the gate origin where the real sign-in and the
            // `__Host-` cookie live, is a credential-harvesting link wearing
            // our chrome.
            //
            // What makes a *kind* able to account for it is that it names a
            // destination we can check — `LinkKind::host_is_surprising`. So
            // `github` pointing at github.com is quiet, and `github` pointing
            // anywhere else is loud, which is the case the whole rule exists
            // for. An exact `label == host` comparison was too blunt in both
            // directions: it printed "Github github.com" as noise, and it
            // would have gone quiet for a label somebody wrote as the host.
            let host = host_of(&link.url);
            let shown = if link.kind.host_is_surprising(&host) {
                format!("<span class=\"muted\"> {}</span>", esc(&host))
            } else {
                String::new()
            };
            out.push_str(&format!(
                "<a href=\"{}\">{}</a>{shown} ",
                esc(&link.url),
                esc(&label)
            ));
        }
        out.push_str("</nav>");
    }
    out
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

/// One generated line.
///
/// **No id.** These used to trail the artifact's internal id — "Book a
/// meeting — book" — which is bookkeeping the owner uses to address a thing
/// from a command line, and means nothing to the visitor the page is for. A
/// public page that prints its own primary keys reads as a debug dump.
fn line(gate: &str, path: &str, label: &str) -> String {
    format!(
        "<li><a href=\"{gate}{}\">{}</a></li>",
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
            .filter_map(|t| t.reach.path().map(|path| line(&gate, path, &t.title)))
            .collect(),
    ));
    body.push_str(&group(
        "Reports",
        inv.bundles
            .iter()
            .filter_map(|b| b.reach.path().map(|path| line(&gate, path, &b.title)))
            .collect(),
    ));
    body.push_str(&group(
        "Polls",
        inv.polls
            .iter()
            .filter_map(|p| p.reach.path().map(|path| line(&gate, path, &p.title)))
            .collect(),
    ));

    if inv.public_count() == 0 {
        // Honest rather than empty. A page that says nothing reads as broken;
        // one that says there is nothing yet reads as early.
        body.push_str("<p class=\"muted\">Nothing public here yet.</p>");
    }

    let chrome = chrome_for(&app, &headers, &user);
    session_page(
        StatusCode::OK,
        shell_with(
            profile.display_name.as_deref().unwrap_or(&user.handle),
            &body,
            "/a/",
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

/// `GET /@{handle}/{slug}` — a switchboard.
///
/// The same page as the hangar with its lines patched by hand instead of
/// wired from the inventory. Two differences that are both deliberate:
///
/// **It is not governed by `enabled`.** That toggle is the hangar's. A
/// switchboard's URL is in somebody's email signature, and hiding an index
/// must not silently break a link a stranger is holding — separate
/// publications, separate audiences.
///
/// **A dark line is omitted, not rendered dead.** A button that answers 404
/// in the page in your email signature is worse than an absent one, because
/// the person who clicks it concludes something about you. The owner is told
/// in the cockpit instead.
pub async fn switchboard(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    Path((handle, slug)): Path<(String, String)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((user, profile)) = owner(&app, &handle) else {
        return nothing_here();
    };
    // A slug the manifest would refuse can never have been stored, so this
    // is a cheap way to answer before touching the database — and it keeps
    // the reserved names answering exactly what an unclaimed one answers.
    if mecha_manifest::Board::from_toml(&format!("slug = {slug:?}\n")).is_err() {
        return nothing_here();
    }
    let Some(board) = read_board(&app, &user, &slug) else {
        return nothing_here();
    };

    let inv = Inventory::read(&app.db, &user);
    let gate = app.config.base_url(Role::Gate);

    let heading = board
        .heading
        .clone()
        .or_else(|| profile.display_name.clone())
        .unwrap_or_else(|| user.handle.clone());
    let mut body = format!("<h1>{}</h1>", esc(&heading));
    if let Some(intro) = &board.intro {
        body.push_str(&format!("<p class=\"intro\">{}</p>", esc(intro)));
    }

    let lit: Vec<String> = inv
        .resolve_all(&board)
        .into_iter()
        .filter_map(|line| match line {
            Line::Dark { .. } => None,
            Line::Lit {
                href,
                label,
                blurb,
                external_host,
            } => {
                // A reference is gate-relative and an external link is
                // absolute, which is also the difference the reader is shown:
                // the host, always, for anything that leaves our origin.
                let (target, note) = match &external_host {
                    Some(host) => (href.clone(), Some(host.clone())),
                    None => (format!("{gate}{href}"), None),
                };
                // The blurb goes *inside* the anchor so the whole card is
                // one target — a label and its explanation are one thing to
                // click, and a blurb outside the border reads as a caption
                // that fell off.
                let mut cell = format!(
                    "<li><a class=\"line\" href=\"{}\"><span class=\"label\">{}</span>",
                    esc(&target),
                    esc(&label)
                );
                if let Some(blurb) = &blurb {
                    cell.push_str(&format!("<span class=\"blurb\">{}</span>", esc(blurb)));
                }
                cell.push_str("</a>");
                // The host stays outside it: where a line goes is a fact
                // about the destination, not part of the thing you press.
                if let Some(host) = note {
                    cell.push_str(&format!("<span class=\"muted\"> {}</span>", esc(&host)));
                }
                cell.push_str("</li>");
                Some(cell)
            }
        })
        .collect();

    if lit.is_empty() {
        // Honest rather than blank. Every line being dark is a real state and
        // the owner needs to see the page say so when they open their own.
        body.push_str("<p class=\"muted\">Nothing here right now.</p>");
    } else {
        body.push_str(&format!("<ul class=\"lines\">{}</ul>", lit.join("")));
    }

    // A footer back to the hangar, but only when there is one to go to.
    if profile.enabled {
        body.push_str(&format!(
            "<p class=\"muted\"><a href=\"{gate}/@{}\">Everything else</a></p>",
            esc(&user.handle)
        ));
    }

    let chrome = chrome_for(&app, &headers, &user);
    session_page(StatusCode::OK, shell_with(&heading, &body, "/a/", &chrome))
}

/// Every dark line a user has, across all their boards — for the cockpit.
///
/// The other half of "omitted, not rendered dead": the page stays clean and
/// the owner is the one who finds out. Reported here rather than computed in
/// the account page, so the page and the report can never disagree about
/// which lines are lit.
pub(crate) fn dark_lines(app: &Shared, user: &UserRow) -> Vec<(String, String, String)> {
    let inv = Inventory::read(&app.db, user);
    let mut out = Vec::new();
    for (slug, row) in app
        .db
        .records(&user.id, crate::db::RECORD_BOARD)
        .unwrap_or_default()
    {
        let Ok(board) = mecha_manifest::Board::from_toml(&row.effective) else {
            continue;
        };
        for line in inv.resolve_all(&board) {
            if let Line::Dark { label, why } = line {
                let name = if slug.is_empty() {
                    "the hangar".to_string()
                } else {
                    slug.clone()
                };
                out.push((name, label, why));
            }
        }
    }
    out
}
