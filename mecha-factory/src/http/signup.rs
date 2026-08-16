//! Asking for an account, and claiming a handle from the link that answers.
//!
//! ```text
//!   GET  /signup                    the address form: anyone may ask
//!   POST /signup                    budget allowing, a link is mailed
//!   GET  /signup/<token>            the claim form, if the invite is live
//!   POST /signup/<token>            handle validated → the account exists
//!   GET  /signup/<token>/form.css   the same stylesheet the forms use
//! ```
//!
//! **Signup is open, and the thing that bounds it is certificates.** The two
//! halves below are the operator-minted invite flow with the operator taken
//! out of the middle: asking mints the same row `factory invite create` mints,
//! mails the same link, and lands on the same claim form. What replaces the
//! operator's judgement is not a smaller check — it is an arithmetic one.
//!
//! Every account costs a certificate. Let's Encrypt issues 50 **new**
//! certificates per registered domain per week, refilling at one per 202
//! minutes, and each active user's two hostnames are one of them
//! (`certificates::user_group`). Renewals are exempt, so the ceiling is on
//! *growth* and never on the deployment already running. Uncapped signup does
//! not fail by letting too many people in; it fails by handing person 51 a
//! permanent handle whose hostnames cannot be issued, so their URLs die in the
//! TLS handshake, before the application, for days — and nothing in the flow
//! they walked through would have said so.
//!
//! So the budget is the gate, and it is spent where the certificate is spent:
//! accounts created this week, plus invites live enough to still become one.
//! When it is gone the page says signups are paused and names the hour a slot
//! frees. That is the failure this shape chooses — visible, temporary, and
//! nobody's URLs broken.
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

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;

use super::intake::{form_values, page, shell};
use super::{v1, Shared};
use crate::config::{Origin, Role};
use crate::db::Claim;

/// New accounts a rolling week may create, out of Let's Encrypt's 50 new
/// certificates per registered domain per week.
///
/// Ten short of the real ceiling on purpose. The margin is not timidity — it
/// is what the operator spends: an invite minted by hand, a re-order after a
/// failed validation, the base group the first time a redirect host is added.
/// A budget set *at* the limit makes every one of those the thing that breaks
/// somebody else's signup, and the person who finds out is the one whose
/// handle has no certificate.
const SIGNUPS_PER_WEEK: usize = 40;

/// Days a spent slot stays spent — Let's Encrypt's window, not ours.
const BUDGET_DAYS: i64 = 7;

/// Asks one address may make in a day.
///
/// The week's budget is the resource worth defending, and the shape of the
/// attack on it is one host asking forty times with forty addresses it
/// controls: signups are then paused for a week and every real person is
/// turned away by a page that says nothing is wrong. Three is room for a typo,
/// a second household member, and a retry — and it makes spending the week
/// take fourteen hosts rather than one.
///
/// It is not a defence against somebody with a subnet, and nothing in process
/// can be. What it buys is that the cheap version of the attack stops working,
/// which is the honest scope of every limit on this box (`ratelimit.rs`).
const ASKS_PER_ADDRESS_PER_DAY: i64 = 3;

/// What the week has already committed, and when the oldest of it frees.
struct Budget {
    spent: usize,
    /// The earliest moment a slot comes back. `None` when nothing is spent.
    frees_at: Option<String>,
}

impl Budget {
    fn exhausted(&self) -> bool {
        self.spent >= SIGNUPS_PER_WEEK
    }
}

/// What this week has spent, counted where the certificate is spent.
///
/// Two things commit a certificate and they must not be double-counted: an
/// account created in the window (its order went out when the row committed),
/// and an invite still live enough to become one. A *claimed* invite is
/// already counted as the user it became; an expired or revoked one never cost
/// anything, which is what makes revoking a pending invite in `/admin` return
/// its slot immediately rather than a week later.
fn budget(app: &Shared, now: &str) -> anyhow::Result<Budget> {
    let since = (chrono::Utc::now() - chrono::Duration::days(BUDGET_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Every timestamp is written by `db::now()` in the same RFC 3339 shape, to
    // the same precision, always in UTC — so lexicographic order is
    // chronological order, and this compares strings rather than parsing.
    let mut spent: Vec<String> = app
        .db
        .users()?
        .into_iter()
        .map(|user| user.created_at)
        .filter(|at| at.as_str() > since.as_str())
        .collect();
    spent.extend(
        app.db
            .invites()?
            .into_iter()
            .filter(|row| row.status(now) == "pending")
            .map(|row| row.created_at)
            .filter(|at| at.as_str() > since.as_str()),
    );

    let frees_at = spent
        .iter()
        .min()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| {
            (at + chrono::Duration::days(BUDGET_DAYS))
                .to_utc()
                .format("%-d %B at %H:%M UTC")
                .to_string()
        });
    Ok(Budget {
        spent: spent.len(),
        frees_at,
    })
}

/// The page anyone who asks gets, whatever came of it.
///
/// **One set of bytes for every address**, exactly like the dead-invite page
/// and for the same reason: whether an address already has an account, has
/// already asked today, or has never been seen here is not something a form
/// anybody can post to gets to answer. The mail is the only channel that
/// distinguishes them, and it reaches the only person entitled to know.
fn asked() -> Response {
    page(
        StatusCode::OK,
        shell(
            "Check your inbox",
            "<h1>Check your inbox</h1>\
             <p>If that address can have an account, a link to pick your \
             handle is on its way. It works once, and it expires.</p>\
             <p>Nothing else happens until you open it — and if no mail \
             arrives, that address already has an account or has already \
             asked today.</p>",
            "/account/a/",
        ),
    )
}

/// The ask form. `error` is this page's whole state, as on the claim form.
fn ask_form(attempted: &str, error: Option<&str>) -> Response {
    let error_html = match error {
        Some(text) => format!(
            "<p role=\"alert\"><strong>{}</strong></p>",
            mecha_manifest::escape_text(text)
        ),
        None => String::new(),
    };
    let body = format!(
        "<h1>Create an account</h1>\
         <p>Give an address and the box mails you a link. Open it, pick a \
         handle, and your pages live at <code>&lt;handle&gt;.art.…</code> — \
         with a certificate ordered the moment you claim it.</p>\
         <p>The address is how you sign in afterwards, so use one you keep. \
         There is no password to lose.</p>\
         {error_html}\
         <form method=\"post\" action=\"/signup\">\
         <label for=\"email\">Email</label>\
         <input id=\"email\" name=\"email\" type=\"email\" value=\"{attempted}\" \
         required autocomplete=\"email\">\
         <button type=\"submit\">Send me a link</button>\
         </form>",
        attempted = mecha_manifest::escape_text(attempted),
    );
    page(
        if error.is_some() {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        },
        shell("Create an account", &body, "/account/a/"),
    )
}

/// Signups are paused, and the page says so rather than pretending.
///
/// A 503 with `Retry-After` because that is what this is: a temporary refusal
/// by a box that is working. The alternative — accepting the ask and mailing a
/// link to a handle that cannot be issued — moves the failure to the TLS
/// handshake on somebody's permanent hostname, where no page can explain it.
fn paused(budget: &Budget) -> Response {
    let when = match &budget.frees_at {
        Some(at) => format!(
            " The next one frees on {}.",
            mecha_manifest::escape_text(at)
        ),
        None => String::new(),
    };
    let mut response = page(
        StatusCode::SERVICE_UNAVAILABLE,
        shell(
            "Signups are paused",
            &format!(
                "<h1>Signups are paused</h1>\
                 <p>Every account here gets its own certificate, and the \
                 issuing authority allows a fixed number of new ones each \
                 week. This week's are spoken for.{when}</p>\
                 <p>Nothing is wrong, and nothing is lost — come back and \
                 ask again.</p>"
            ),
            "/account/a/",
        ),
    );
    // An hour: the budget is a rolling window, so the honest answer is "later
    // today" and never a precise second. Long enough that a retry loop is not
    // the thing that empties the next slot.
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        HeaderValue::from_static("3600"),
    );
    response
}

/// One address has asked enough for one day.
fn too_many() -> Response {
    let mut response = page(
        StatusCode::TOO_MANY_REQUESTS,
        shell(
            "Too many requests",
            "<h1>Too many requests</h1>\
             <p>This connection has asked for an account several times today, \
             so the rest of today's asks are refused. If a link was sent \
             earlier, it is still good.</p>\
             <p>Try again tomorrow.</p>",
            "/account/a/",
        ),
    );
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        HeaderValue::from_static("3600"),
    );
    response
}

/// `GET /signup` — the way in for somebody who has no link.
pub async fn ask_page(State(app): State<Shared>, Extension(origin): Extension<Origin>) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    // The budget is read here too, so a paused week is visible before anyone
    // types an address rather than after.
    match budget(&app, &crate::db::now()) {
        Ok(budget) if budget.exhausted() => paused(&budget),
        Ok(_) => ask_form("", None),
        Err(e) => {
            tracing::error!(error = %e, "reading the signup budget");
            super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `POST /signup` — mint this address its own invite, budget allowing.
pub async fn ask(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
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
    // Their own typing, so this one *is* told back to them — it says nothing
    // about any address but the one they just wrote.
    if email.is_empty() || !email.contains('@') {
        return ask_form(&email, Some("That does not look like an email address"));
    }

    let now = crate::db::now();
    let budget = match budget(&app, &now) {
        Ok(budget) => budget,
        Err(e) => {
            tracing::error!(error = %e, "reading the signup budget");
            return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    };
    if budget.exhausted() {
        tracing::info!(spent = budget.spent, "a signup arrived on a spent budget");
        return paused(&budget);
    }

    // The week's budget is checked before this one so that a paused week is
    // reported as paused and costs the visitor nothing — being turned away by
    // an exhausted box should not also spend their own day's allowance.
    //
    // Hashed like the upload budget's, and for the same reason: the count is
    // what bounds abuse, and keeping the address itself would be a log of who
    // considered signing up.
    let ip_hash = crate::intake::hash_token(&peer.ip().to_string());
    let today = crate::db::today();
    match app.db.signup_asks_today(&ip_hash, &today) {
        // Told plainly rather than hidden behind the "check your inbox" page.
        // This says something about the visitor's own address, which they
        // already know — the rule that keeps the page silent is about *other*
        // people's addresses, and a person on a shared connection deserves to
        // know why no mail is coming.
        Ok(asks) if asks >= ASKS_PER_ADDRESS_PER_DAY => {
            tracing::info!(asks, "signup asks from one address hit the daily cap");
            return too_many();
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "reading the per-address signup budget");
            return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    }
    if let Err(e) = app.db.signup_ask_add(&ip_hash, &today) {
        // Counted before the outcome is known, and a failure to count is a
        // refusal rather than a free ask: an uncountable budget is not a
        // budget, and the visitor can try again.
        tracing::error!(error = %e, "counting a signup ask");
        return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
            .into_response();
    }

    // Everything past here answers with the same page, so the order of these
    // checks is invisible from outside — which is the point. Both are refusals
    // about *this address*, and an address is exactly what a stranger posting
    // this form is trying to learn about.
    let taken = match app.db.users_by_email(&email) {
        Ok(users) => !users.is_empty(),
        Err(e) => {
            tracing::error!(error = %e, "reading accounts for a signup");
            return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    };
    let asked_today = match app.db.invites() {
        Ok(invites) => {
            let today = crate::db::today();
            invites.iter().any(|row| {
                row.email.eq_ignore_ascii_case(&email) && row.created_at.starts_with(&today)
            })
        }
        Err(e) => {
            tracing::error!(error = %e, "reading invites for a signup");
            return super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
        }
    };
    if taken || asked_today {
        // Logged, because the page cannot say it: an address hammering this
        // form is worth seeing in the journal.
        tracing::info!(taken, asked_today, "a signup asked for nothing new");
        return asked();
    }

    match v1::mint_invite(&app, &email, "self-serve") {
        Ok(_) => asked(),
        // Refused for shape after we already checked it, or the ledger is
        // unavailable. Neither is the visitor's problem to solve, and the
        // second must not be reported as success.
        Err(v1::InviteRefused::BadAddress) => {
            ask_form(&email, Some("That does not look like an email address"))
        }
        Err(v1::InviteRefused::Unavailable) => {
            super::Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

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

            // The one moment we know a person is at a browser having just
            // proved themselves is the moment to hand them the pairing
            // command — signup that ends with "now find the operator" would
            // put the SSH session right back in the flow. Failure to mint is
            // a warning, not a dead signup: the account exists either way,
            // and `factory pair create` can mint another code any time.
            let pairing = crate::keys::mint_pairing(&app.db, &user.id)
                .map_err(|e| tracing::warn!(error = %e, "minting a pairing code at signup"))
                .ok();
            let connect = match &pairing {
                Some(code) => format!(
                    "<h2>Connect your machine</h2>\
                     <p>On the machine that will publish for you, run:</p>\
                     <pre><code>factory-publish connect --gate {gate} \
--handle {handle} {code}</code></pre>\
                     <p>The code works once and expires in \
                     {expiry}&nbsp;minutes. It installs this machine's own \
                     keys — pair each machine separately, and any of them \
                     can be revoked on its own.</p>",
                    gate = mecha_manifest::escape_text(&app.config.base_url(Role::Gate)),
                    handle = mecha_manifest::escape_text(&user.handle),
                    code = mecha_manifest::escape_text(code),
                    expiry = crate::keys::PAIR_EXPIRY_MINUTES,
                ),
                None => "<p>Connecting a machine needs a pairing code; ask \
                         the operator for one.</p>"
                    .to_string(),
            };
            let body = format!(
                "<h1>You are <code>{handle}</code></h1>\
                 <p>Your pages will live at \
                 <a href=\"{art}\">{art}</a> and notebooks at \
                 <a href=\"{compute}\">{compute}</a>.</p>\
                 <p>A certificate for those names is being ordered now — they \
                 start answering within a minute or two.</p>\
                 {connect}",
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
