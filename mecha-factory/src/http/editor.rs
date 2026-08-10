//! Writing a profile or a board from the cockpit.
//!
//! ```text
//!   GET  /account/edit/{what}          profile · hangar
//!   GET  /account/edit/board/{slug}    one switchboard
//!   POST /account/edit                 save
//!   POST /account/boards               claim a slug and start a board
//! ```
//!
//! The same record a machine pushes, edited by a person. Both writers land on
//! the store's `effective` column and leave `baseline` alone, which is what
//! lets the next push tell what was changed here and fold around it rather
//! than flatten it.
//!
//! Four rules, and the second is the one that decides whether the editor gets
//! used twice.
//!
//! **The validator is `mecha_manifest`'s, the same one `PUT /v1/profile`
//! runs.** One validator, two doors. A record the cockpit accepts and a push
//! rejects — or the reverse — is the bug this rules out by construction.
//!
//! **A rejected save never loses the text.** The page comes back with the
//! submitted source still in the textarea and the error above it.
//! [`account::mutating`] as written re-renders a *stale* page, which is right
//! for a button and wrong for forty lines somebody just typed: losing them to
//! a misplaced bracket is how an in-place editor stops being used.
//!
//! **A dark line warns, it does not refuse.** Writing the board before
//! creating the form it points at is an ordinary order to work in. The save
//! lands and the page says which lines point at nothing.
//!
//! **Claiming a slug is permanent, so the form says so and confirms.** A slug
//! is a URL somebody may put in an email signature; it can never be reissued,
//! for the reason a handle cannot. A "New page" button that quietly burned a
//! name would be a trap that only shows itself on the second attempt.
//!
//! What this does *not* grant is publishing. A board is a list of references
//! to things that already exist and are already permissioned — authoring one
//! opens no read that was not already open, which is `inventory::Reach` doing
//! its second job.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use mecha_manifest::escape_text as esc;

use super::intake::{page, shell_with, Chrome};
use super::{account, v1, Shared};
use crate::config::Origin;
use crate::db::UserRow;
use crate::inventory::{Inventory, Line};

/// Which record is being edited.
enum Target {
    Profile,
    Hangar,
    Board(String),
}

impl Target {
    fn parse(what: &str, slug: Option<String>) -> Option<Target> {
        match (what, slug) {
            ("profile", None) => Some(Target::Profile),
            ("hangar", None) => Some(Target::Hangar),
            ("board", Some(slug)) => Some(Target::Board(slug)),
            _ => None,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Target::Profile => crate::db::RECORD_PROFILE,
            _ => crate::db::RECORD_BOARD,
        }
    }

    fn slug(&self) -> &str {
        match self {
            Target::Board(slug) => slug,
            _ => "",
        }
    }

    fn title(&self) -> String {
        match self {
            Target::Profile => "Your profile".into(),
            Target::Hangar => "Your hangar".into(),
            Target::Board(slug) => format!("Switchboard: {slug}"),
        }
    }

    /// The path this record's editor is at, for a form's action and a link.
    fn href(&self) -> String {
        match self {
            Target::Profile => "/account/edit/profile".into(),
            Target::Hangar => "/account/edit/hangar".into(),
            Target::Board(slug) => format!("/account/edit/board/{slug}"),
        }
    }

    fn form_fields(&self) -> String {
        match self {
            Target::Profile => "<input type=\"hidden\" name=\"what\" value=\"profile\">".into(),
            Target::Hangar => "<input type=\"hidden\" name=\"what\" value=\"hangar\">".into(),
            Target::Board(slug) => format!(
                "<input type=\"hidden\" name=\"what\" value=\"board\">\
                 <input type=\"hidden\" name=\"slug\" value=\"{}\">",
                esc(slug)
            ),
        }
    }

    /// The same check the push endpoint runs, plus the slug agreement a URL
    /// implies. Errors come back as prose because they go on the page.
    fn check(&self, text: &str) -> Result<(), String> {
        match self {
            Target::Profile => mecha_manifest::Profile::from_toml(text)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Target::Hangar | Target::Board(_) => {
                let board = mecha_manifest::Board::from_toml(text).map_err(|e| e.to_string())?;
                let expected = match self {
                    Target::Board(slug) => Some(slug.as_str()),
                    _ => None,
                };
                if board.slug.as_deref() != expected {
                    return Err(format!(
                        "this page is {}, so the record must say so",
                        expected
                            .map(|s| format!("`{s}`"))
                            .unwrap_or_else(|| "the hangar".into())
                    ));
                }
                Ok(())
            }
        }
    }

    /// What a freshly created record starts as. A file with the shape already
    /// in it is the difference between an editor somebody uses and a blank
    /// box they close again.
    fn starter(&self) -> String {
        match self {
            Target::Profile => "enabled = false\n# display_name = \"Your Name\"\n\
                                # tagline = \"One line about you\"\n"
                .into(),
            Target::Hangar => {
                "# heading = \"Your Name\"\n# intro = \"Everything in one place.\"\n".into()
            }
            Target::Board(slug) => format!(
                "slug = \"{slug}\"\nheading = \"{slug}\"\n\
                 # intro = \"One line about this page.\"\n\n\
                 # Each line points at something that already exists. The server\n\
                 # resolves it, so a line at something missing is left off the\n\
                 # page rather than served as a dead button.\n\
                 #\n\
                 # [[entry]]\n\
                 # kind = \"booking\"   # booking | form | bundle | poll | link\n\
                 # id = \"office-hours\"\n\
                 # label = \"Book a meeting\"\n\
                 # blurb = \"20 or 45 minutes.\"\n"
            ),
        }
    }
}

/// The dark lines of whatever was just saved, as sentences.
fn dark_notes(app: &Shared, user: &UserRow, target: &Target, source: &str) -> Vec<String> {
    if matches!(target, Target::Profile) {
        return Vec::new();
    }
    let Ok(board) = mecha_manifest::Board::from_toml(source) else {
        return Vec::new();
    };
    let inv = Inventory::read(&app.db, user);
    inv.resolve_all(&board)
        .into_iter()
        .filter_map(|line| match line {
            Line::Dark { label, why } => Some(format!("{label} — {why}")),
            Line::Lit { .. } => None,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn editor_page(
    app: &Shared,
    user: &UserRow,
    csrf: &str,
    target: &Target,
    source: &str,
    error: Option<&str>,
    saved: bool,
    notes: &[String],
) -> Response {
    let mut body = format!("<h1>{}</h1>", esc(&target.title()));

    if let Some(error) = error {
        // Above the textarea, because the text below it is what needs fixing.
        body.push_str(&format!(
            "<p class=\"signal\"><strong>Not saved.</strong> {}</p>",
            esc(error)
        ));
    } else if saved {
        body.push_str("<p><strong>Saved.</strong></p>");
    }

    if !notes.is_empty() {
        body.push_str(
            "<p><strong>Some lines point at nothing</strong>, so they are left off the page \
             rather than served as dead buttons. This is saved either way — create the target, \
             or take the line out.</p><ul>",
        );
        for note in notes {
            body.push_str(&format!("<li>{}</li>", esc(note)));
        }
        body.push_str("</ul>");
    }

    body.push_str(&format!(
        "<form method=\"post\" action=\"/account/edit\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">{fields}\
         <label for=\"source\">TOML</label>\
         <textarea id=\"source\" name=\"source\" rows=\"24\" spellcheck=\"false\">{source}</textarea>\
         <button type=\"submit\">Save</button></form>\
         <p><a href=\"/account\">Back to your account</a></p>",
        fields = target.form_fields(),
        // The submitted text, not the stored one: a rejected save comes back
        // with what the person typed.
        source = esc(source),
    ));

    let chrome = Chrome::Account {
        handle: user.handle.clone(),
        email: user.email.clone(),
        csrf: csrf.to_string(),
        docs_url: app.config.docs_url.clone(),
    };
    let status = if error.is_some() {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::OK
    };
    page(
        status,
        shell_with(&target.title(), &body, "/account/a/", &chrome),
    )
}

/// `GET /account/edit/{what}` and `/account/edit/board/{slug}`.
async fn show(app: Shared, headers: HeaderMap, origin: Origin, target: Target) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return account::nothing_here();
    }
    let Some((token, user)) = account::session(&app, &headers) else {
        return account::signin_form();
    };
    let stored = app
        .db
        .record_get(&user.id, target.kind(), target.slug())
        .ok()
        .flatten();
    // A record nobody has written yet opens as its starter rather than as an
    // empty box.
    let source = match &stored {
        Some(row) if !row.effective.trim().is_empty() => row.effective.clone(),
        _ => target.starter(),
    };
    let notes = dark_notes(&app, &user, &target, &source);
    editor_page(
        &app,
        &user,
        &account::csrf(&token),
        &target,
        &source,
        None,
        false,
        &notes,
    )
}

pub async fn edit_record(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    Path(what): Path<String>,
) -> Response {
    let Some(target) = Target::parse(&what, None) else {
        return account::nothing_here();
    };
    show(app, headers, origin, target).await
}

pub async fn edit_board(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    Path((what, slug)): Path<(String, String)>,
) -> Response {
    let Some(target) = Target::parse(&what, Some(slug)) else {
        return account::nothing_here();
    };
    show(app, headers, origin, target).await
}

/// `POST /account/edit` — save.
pub async fn save(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (token, user, values) = match account::mutating(&app, &origin, &headers, &body) {
        Ok(parts) => parts,
        Err(refusal) => return *refusal,
    };
    let field = |name: &str| {
        values
            .get(name)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let what = field("what");
    let slug = field("slug");
    let Some(target) = Target::parse(&what, (!slug.is_empty()).then_some(slug)) else {
        return account::nothing_here();
    };
    let source = field("source");
    let csrf = account::csrf(&token);

    if let Err(message) = target.check(&source) {
        // The submitted text goes back into the textarea. Losing it here is
        // how this editor would stop being used.
        return editor_page(
            &app,
            &user,
            &csrf,
            &target,
            &source,
            Some(&message),
            false,
            &[],
        );
    }
    if let Err(e) = app.db.record_edit(
        &user.id,
        target.kind(),
        target.slug(),
        &source,
        &crate::db::now(),
    ) {
        tracing::error!(error = %e, "saving a record from the cockpit");
        return editor_page(
            &app,
            &user,
            &csrf,
            &target,
            &source,
            Some("that could not be stored"),
            false,
            &[],
        );
    }
    // Warned, not refused: a board written before the form it points at is an
    // ordinary order to work in.
    let notes = dark_notes(&app, &user, &target, &source);
    editor_page(&app, &user, &csrf, &target, &source, None, true, &notes)
}

/// `POST /account/boards` — claim a slug and start a switchboard.
pub async fn create(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let (_, user, values) = match account::mutating(&app, &origin, &headers, &body) {
        Ok(parts) => parts,
        Err(refusal) => return *refusal,
    };
    let slug = values
        .get("slug")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    let confirmed = values.get("confirm").and_then(|v| v.as_str()).is_some();

    let refuse = |message: &str| {
        page(
            StatusCode::UNPROCESSABLE_ENTITY,
            shell_with(
                "Not created",
                &format!(
                    "<h1>Not created</h1><p>{}</p><p><a href=\"/account#boards\">Back</a></p>",
                    esc(message)
                ),
                "/account/a/",
                &Chrome::Public {
                    docs_url: app.config.docs_url.clone(),
                    sign_in: false,
                },
            ),
        )
    };

    // The confirmation is not decoration. A name claimed here can never be
    // reissued, so it must not be spent as a side effect of typing one.
    if !confirmed {
        return refuse(
            "A page's name is permanent — it goes in links other people keep, so it can never \
             be given to a different page later. Tick the box to confirm.",
        );
    }
    if let Err(e) = mecha_manifest::Board::from_toml(&format!("slug = {slug:?}\n")) {
        return refuse(&e.to_string());
    }
    match app.db.record_get(&user.id, crate::db::RECORD_BOARD, &slug) {
        Ok(Some(_)) => return refuse("you already have a page with that name"),
        Err(e) => {
            tracing::error!(error = %e, "checking a slug");
            return refuse("that could not be checked");
        }
        Ok(None) => {}
    }

    let target = Target::Board(slug);
    if let Err(e) = app.db.record_edit(
        &user.id,
        target.kind(),
        target.slug(),
        &target.starter(),
        &crate::db::now(),
    ) {
        tracing::error!(error = %e, "creating a board");
        return refuse("that could not be stored");
    }
    // Straight into the editor: the page exists now and is empty, which is
    // the moment somebody wants to write it.
    (
        StatusCode::SEE_OTHER,
        [(axum::http::header::LOCATION, target.href())],
        "",
    )
        .into_response()
}
