//! The poll page: `/p/{handle}/{poll_id}/{token}` on the gate.
//!
//! The capability is the identity — no account, no name typing, no way to
//! typo yourself into a duplicate respondent. Two shapes share the route:
//! the seeded times grid (rows without a spec — the legacy shape and still
//! the pipeline's), and the general poll, whose questions live in the
//! stored spec. Either way what comes back is vocabulary the poll itself
//! declared — with the one deliberate exception of a `text` question's
//! prose, which is capped by the spec, escaped at render, and quarantined
//! at the drain. Everything else on these pages never feeds the
//! quarantine at all.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use mecha_manifest::{
    ballot_from_form, build_results, poll_page, survey_page, validate_ballot, Ballot, Identity,
    PageMode, PollAnswer, PollCandidate, PollPageOptions, PollSpec, QuestionResults, Show,
    SurveyPageOptions,
};

use super::{v1, Failure, Shared};
use crate::config::Origin;
use crate::db::PollRow;

fn nothing_here() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Not found</title></head><body><main><h1>Not found</h1>\
         <p>There is no poll here. The link may be from an older email.</p>\
         </main></body></html>",
    )
        .into_response()
}

/// A candidate as the poll row stores it.
#[derive(serde::Deserialize)]
struct StoredCandidate {
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    duration_minutes: u32,
}

/// Resolve a token against the path it arrived on. The token names one
/// participant of one poll; a URL whose handle or poll id disagrees with
/// the row is somebody splicing capabilities onto other pages.
fn resolve(app: &Shared, handle: &str, poll_id: &str, token: &str) -> Option<(PollRow, String)> {
    let user = match app.db.user_by_handle(handle) {
        Ok(Some(user)) if user.active() => user,
        _ => return None,
    };
    let (poll, name) = app
        .db
        .poll_by_token(&crate::intake::hash_token(token))
        .ok()
        .flatten()?;
    (poll.user_id == user.id && poll.id == poll_id).then_some((poll, name))
}

/// Whether answers may still change: the state says open and the deadline,
/// if any, has not passed. The deadline is enforced here rather than by a
/// sweeper — a poll nobody touches after its deadline needs nothing done
/// to refuse correctly.
pub(crate) fn still_open(poll: &PollRow, now: chrono::DateTime<chrono::Utc>) -> bool {
    poll.state == "open"
        && poll
            .deadline
            .as_deref()
            .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
            .map(|d| now < d.with_timezone(&chrono::Utc))
            .unwrap_or(true)
}

fn render(app: &Shared, poll: &PollRow, participant: &str, notice: Option<String>) -> Response {
    let Ok(tz) = poll.timezone.parse::<chrono_tz::Tz>() else {
        tracing::error!(poll = %poll.id, tz = %poll.timezone, "a poll with a bad zone");
        return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
    };
    let stored: Vec<StoredCandidate> = serde_json::from_str(&poll.candidates).unwrap_or_default();
    let others = app
        .db
        .poll_participants(&poll.user_id, &poll.id)
        .unwrap_or_default();
    let mine: std::collections::HashMap<String, PollAnswer> = others
        .iter()
        .find(|p| p.name == participant)
        .and_then(|p| p.answers.as_deref())
        .and_then(|a| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(a).ok())
        .map(|map| {
            map.into_iter()
                .filter_map(|(k, v)| v.as_str().and_then(PollAnswer::parse).map(|a| (k, a)))
                .collect()
        })
        .unwrap_or_default();
    let mut yes_counts: std::collections::HashMap<String, usize> = Default::default();
    for p in &others {
        if let Some(answers) = p.answers.as_deref() {
            if let Ok(map) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(answers)
            {
                for (key, value) in map {
                    if value.as_str() == Some("yes") {
                        *yes_counts.entry(key).or_default() += 1;
                    }
                }
            }
        }
    }

    let candidates: Vec<PollCandidate> = stored
        .iter()
        .map(|c| {
            let key = format!(
                "{}|{}",
                c.start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                c.duration_minutes
            );
            PollCandidate {
                start: c.start,
                end: c.end,
                duration_minutes: c.duration_minutes,
                mine: mine.get(&key).copied(),
                yes_count: yes_counts.get(&key).copied().unwrap_or(0),
            }
        })
        .collect();

    let now = chrono::Utc::now();
    let options = PollPageOptions {
        title: poll.title.clone(),
        participant: participant.to_string(),
        timezone: tz,
        action: String::new(), // POST back to this same URL
        assets: "/p/a/".into(),
        theme: mecha_manifest::Theme::by_name(&app.config.theme),
        deadline_local: poll.deadline.as_deref().map(|d| {
            chrono::DateTime::parse_from_rfc3339(d)
                .map(|t| {
                    t.with_timezone(&tz)
                        .format("%a %b %-d, %-I:%M %p %Z")
                        .to_string()
                })
                .unwrap_or_else(|_| d.to_string())
        }),
        responded: others.iter().filter(|p| p.responded_at.is_some()).count(),
        total: others.len(),
        open: still_open(poll, now),
        notice,
    };
    let page = poll_page(&candidates, &options);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page.html,
    )
        .into_response()
}

/// Render whichever shape this poll is. The dispatch is the row's `spec`
/// column: present is a general poll, absent is the seeded times grid, and
/// unreadable fails closed — a page rendered from a guessed schema would
/// collect answers to questions nobody asked.
fn render_any(app: &Shared, poll: &PollRow, participant: &str, notice: Option<String>) -> Response {
    match poll.general_spec() {
        Ok(None) => render(app, poll, participant, notice),
        Ok(Some(spec)) => render_general(
            app,
            poll,
            &spec,
            Some(participant),
            Some(participant.to_string()),
            notice,
        ),
        Err(e) => {
            tracing::error!(poll = %poll.id, error = %e, "unreadable poll spec");
            Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// The general poll's page: the stored spec's questions, this participant's
/// saved ballot, and results exactly as the policy allows this viewer —
/// decided here, where the bytes are emitted, never in the client.
/// One viewer's standing in a general poll — computed once, used by the
/// page and by `results.json`, so the two can never disagree about what
/// this viewer may see.
struct SurveyView {
    ballots: Vec<(String, Ballot)>,
    mine: Ballot,
    open: bool,
    identity: Identity,
    /// Whether the policy shows this viewer results *now*.
    visible: bool,
    responded: usize,
    total: usize,
}

fn survey_view(app: &Shared, poll: &PollRow, spec: &PollSpec, viewer: Option<&str>) -> SurveyView {
    let others = app
        .db
        .poll_participants(&poll.user_id, &poll.id)
        .unwrap_or_default();
    let ballots: Vec<(String, Ballot)> = others
        .iter()
        .filter_map(|p| {
            let ballot = serde_json::from_str::<Ballot>(p.answers.as_deref()?).ok()?;
            Some((p.name.clone(), ballot))
        })
        .collect();
    let mine = viewer
        .and_then(|viewer| {
            ballots
                .iter()
                .find(|(name, _)| name == viewer)
                .map(|(_, ballot)| ballot.clone())
        })
        .unwrap_or_default();
    let open = still_open(poll, chrono::Utc::now());
    let visible = match spec.results.show {
        Show::Live => true,
        Show::AfterVote => !mine.is_empty(),
        Show::AfterClose => !open,
        Show::Creator => false,
    };
    SurveyView {
        mine,
        open,
        identity: spec.results.identity(spec.audience.kind),
        visible,
        responded: others.iter().filter(|p| p.responded_at.is_some()).count(),
        total: others.len(),
        ballots,
    }
}

fn render_general(
    app: &Shared,
    poll: &PollRow,
    spec: &PollSpec,
    viewer: Option<&str>,
    greeting: Option<String>,
    notice: Option<String>,
) -> Response {
    let link = spec.audience.kind == mecha_manifest::AudienceKind::Link;
    let view = survey_view(app, poll, spec, viewer);
    let results = view
        .visible
        .then(|| build_results(spec, &view.ballots, view.identity, view.open, false));
    let options = SurveyPageOptions {
        participant: greeting,
        action: String::new(), // POST back to this same URL
        assets: "/p/a/".into(),
        theme: mecha_manifest::Theme::by_name(&app.config.theme),
        // The deadline renders in its own written offset: a general poll
        // has no host timezone, and the offset the organizer wrote is the
        // clock they meant.
        deadline_local: poll.deadline.as_deref().map(|d| {
            chrono::DateTime::parse_from_rfc3339(d)
                .map(|t| t.format("%a %b %-d, %-I:%M %p %Z").to_string())
                .unwrap_or_else(|_| d.to_string())
        }),
        responded: view.responded,
        // A link poll has no roster, so a denominator would be a guess.
        total: (!link).then_some(view.total),
        open: view.open,
        notice,
        show: spec.results.show,
        identity: view.identity,
        mode: PageMode::Served,
        resolution: poll.resolution.clone(),
    };
    let page = survey_page(spec, &view.mine, results.as_deref(), &options);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page.html,
    )
        .into_response()
}

/// `GET /p/{handle}/{poll_id}/{token}/results.json` — what this viewer may
/// see, right now: the intro line and, when the policy allows, each
/// question's results as a server-rendered fragment. The same rendering
/// the page embeds, so the live swap and a reload always agree. `no-store`
/// because the answer is per-viewer and per-moment.
pub async fn results(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, poll_id, token)): Path<(String, String, String)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((poll, name)) = resolve(&app, &handle, &poll_id, &token) else {
        return nothing_here();
    };
    let spec = match poll.general_spec() {
        Ok(Some(spec)) => spec,
        // A times poll has no results endpoint; its heat rides the page.
        Ok(None) => return nothing_here(),
        Err(e) => {
            tracing::error!(poll = %poll.id, error = %e, "unreadable poll spec");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    };
    results_payload(&app, &poll, &spec, Some(&name), Some(&name))
}

/// The results answer for one viewer — shared by the token route and the
/// link route, because "what may this viewer see" must have one
/// implementation however they arrived.
fn results_payload(
    app: &Shared,
    poll: &PollRow,
    spec: &PollSpec,
    viewer: Option<&str>,
    greeting: Option<&str>,
) -> Response {
    let link = spec.audience.kind == mecha_manifest::AudienceKind::Link;
    let view = survey_view(app, poll, spec, viewer);
    let fragments = view.visible.then(|| {
        let results = build_results(spec, &view.ballots, view.identity, view.open, false);
        let map: serde_json::Map<String, serde_json::Value> =
            mecha_manifest::results_fragments(spec, &results)
                .into_iter()
                .map(|(qid, html)| (qid, html.into()))
                .collect();
        serde_json::Value::Object(map)
    });
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        serde_json::json!({
            "open": view.open,
            "intro": mecha_manifest::intro_line(
                greeting,
                view.responded,
                (!link).then_some(view.total),
            ),
            "results": fragments,
        })
        .to_string(),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The link audience: one shared URL, a cookie ballot capability minted at
// first save, dedup on the honor system and the page says so. Identity is
// `anonymous` by construction (the spec checker refuses anything else), and
// the cap was priced at creation: a full poll is a state, not an error.
// ---------------------------------------------------------------------------

fn ballot_cookie_name(handle: &str, poll_id: &str) -> String {
    // Per-poll, because one browser may hold ballots on many polls; the
    // parts are slug-validated, so the name is cookie-safe.
    format!("factory-ballot-{handle}-{poll_id}")
}

fn ballot_cookie(headers: &axum::http::HeaderMap, handle: &str, poll_id: &str) -> Option<String> {
    let wanted = ballot_cookie_name(handle, poll_id);
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == wanted)
        .map(|(_, value)| value.to_string())
}

/// Resolve the shared URL: an active handle, a general poll, a link
/// audience. Anything else — including a roster poll, whose only doors are
/// its participants' own tokens — is nothing.
fn resolve_link(app: &Shared, handle: &str, poll_id: &str) -> Option<(PollRow, PollSpec)> {
    let user = app
        .db
        .user_by_handle(handle)
        .ok()
        .flatten()
        .filter(|u| u.active())?;
    let poll = app.db.poll_get(&user.id, poll_id).ok().flatten()?;
    let spec = poll.general_spec().ok().flatten()?;
    (spec.audience.kind == mecha_manifest::AudienceKind::Link).then_some((poll, spec))
}

/// The ballot this browser's cookie names, verified against the store — a
/// forged cookie is a token that hashes to nothing.
fn link_viewer(
    app: &Shared,
    headers: &axum::http::HeaderMap,
    handle: &str,
    poll: &PollRow,
) -> Option<String> {
    let token = ballot_cookie(headers, handle, &poll.id)?;
    let (row, name) = app
        .db
        .poll_by_token(&crate::intake::hash_token(&token))
        .ok()
        .flatten()?;
    (row.user_id == poll.user_id && row.id == poll.id).then_some(name)
}

/// `GET /p/{handle}/{poll_id}` — the shared page. No greeting: nobody
/// typed a name, and asking for one is the typo'd-duplicate bug returning
/// as a feature.
pub async fn link_page(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, poll_id)): Path<(String, String)>,
    Query(query): Query<SavedQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((poll, spec)) = resolve_link(&app, &handle, &poll_id) else {
        return nothing_here();
    };
    let viewer = link_viewer(&app, &headers, &handle, &poll);
    let notice = query
        .saved
        .map(|_| "Saved. You can change your answers any time until the poll closes.".to_string());
    render_general(&app, &poll, &spec, viewer.as_deref(), None, notice)
}

/// `POST /p/{handle}/{poll_id}` — a ballot. First save mints the browser
/// its capability (inside the cap, atomically) and sets the cookie; later
/// saves replace that ballot, exactly as a roster participant's do.
pub async fn link_answer(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, poll_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((poll, spec)) = resolve_link(&app, &handle, &poll_id) else {
        return nothing_here();
    };
    let viewer = link_viewer(&app, &headers, &handle, &poll);
    if !still_open(&poll, chrono::Utc::now()) {
        if wants_json {
            return StatusCode::CONFLICT.into_response();
        }
        return render_general(&app, &poll, &spec, viewer.as_deref(), None, None);
    }
    let raw = super::intake::form_values(&body);
    let (ballot, errors) = validate_ballot(&spec, &ballot_from_form(&spec, &raw));
    if !errors.is_empty() {
        if wants_json {
            return StatusCode::UNPROCESSABLE_ENTITY.into_response();
        }
        let what: Vec<String> = errors
            .iter()
            .map(|(question, why)| format!("{question}: {why}"))
            .collect();
        return render_general(
            &app,
            &poll,
            &spec,
            viewer.as_deref(),
            None,
            Some(format!("Nothing was saved — {}.", what.join("; "))),
        );
    }
    let payload = serde_json::to_string(&ballot).unwrap_or_else(|_| "{}".into());
    let now = crate::db::now();

    // A returning browser updates its own ballot through its cookie.
    if let Some(token) = ballot_cookie(&headers, &handle, &poll.id) {
        let hash = crate::intake::hash_token(&token);
        if app.db.poll_by_token(&hash).ok().flatten().is_some() {
            return match app.db.poll_answer(&hash, &payload, &now) {
                Ok(true) if wants_json => StatusCode::NO_CONTENT.into_response(),
                Ok(true) => see_saved(&handle, &poll_id, None),
                Ok(false) if wants_json => StatusCode::CONFLICT.into_response(),
                Ok(false) => render_general(&app, &poll, &spec, viewer.as_deref(), None, None),
                Err(e) => {
                    tracing::error!(poll = %poll_id, error = %e, "storing answers");
                    Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
                }
            };
        }
        // A cookie that names nothing (a wiped store, a forgery) falls
        // through to minting a fresh ballot rather than erroring: the
        // browser is simply new here.
    }

    let token = crate::intake::mint_token();
    let hash = crate::intake::hash_token(&token);
    let name = format!("b-{}", &hash[..12]);
    let max = spec.audience.max_ballots.unwrap_or(0);
    match app
        .db
        .poll_mint_ballot(&poll.user_id, &poll.id, &name, &hash, max)
    {
        Ok(true) => {}
        Ok(false) => {
            // Full is a state the page explains, and the cap was the
            // point: a bot run costs the poll its capacity, never the box
            // its disk.
            if wants_json {
                return StatusCode::CONFLICT.into_response();
            }
            return render_general(
                &app,
                &poll,
                &spec,
                None,
                None,
                Some("This poll has reached its response limit.".into()),
            );
        }
        Err(e) => {
            tracing::error!(poll = %poll_id, error = %e, "minting a ballot");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    }
    let cookie = super::session_cookie(
        &ballot_cookie_name(&handle, &poll_id),
        &token,
        // A semester, roughly: long enough to edit next week, no tenure.
        180 * 24 * 3600,
    );
    match app.db.poll_answer(&hash, &payload, &now) {
        Ok(true) if wants_json => {
            (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
        }
        Ok(true) => see_saved(&handle, &poll_id, Some(cookie)),
        Ok(_) => render_general(&app, &poll, &spec, Some(&name), None, None),
        Err(e) => {
            tracing::error!(poll = %poll_id, error = %e, "storing answers");
            Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

fn see_saved(handle: &str, poll_id: &str, cookie: Option<String>) -> Response {
    let location = format!("/p/{handle}/{poll_id}?saved=1");
    match cookie {
        Some(cookie) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, location), (header::SET_COOKIE, cookie)],
        )
            .into_response(),
        None => (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response(),
    }
}

// ---------------------------------------------------------------------------
// The projector. The screen capability is the creator's authority, so the
// page shows aggregates whatever `show` says — the reveal is whether this
// page is on the wall. What the wall gets is the stricter cut: typed
// tallies and the word cloud, never anyone's sentence, and the anonymous
// suppression floor still applies, because a projector is an audience.
// ---------------------------------------------------------------------------

fn resolve_screen(
    app: &Shared,
    handle: &str,
    poll_id: &str,
    token: &str,
) -> Option<(PollRow, PollSpec)> {
    let user = app
        .db
        .user_by_handle(handle)
        .ok()
        .flatten()
        .filter(|u| u.active())?;
    let poll = app.db.poll_get(&user.id, poll_id).ok().flatten()?;
    let stored = poll.screen_token_hash.clone()?;
    if stored != crate::intake::hash_token(token) {
        return None;
    }
    let spec = poll.general_spec().ok().flatten()?;
    Some((poll, spec))
}

fn screen_state(
    app: &Shared,
    poll: &PollRow,
    spec: &PollSpec,
) -> (Vec<QuestionResults>, usize, bool) {
    let view = survey_view(app, poll, spec, None);
    let results = build_results(spec, &view.ballots, view.identity, view.open, true);
    (results, view.responded, view.open)
}

/// `GET /p/{handle}/{poll_id}/screen/{token}` — the wall.
pub async fn screen_page(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, poll_id, token)): Path<(String, String, String)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((poll, spec)) = resolve_screen(&app, &handle, &poll_id, &token) else {
        return nothing_here();
    };
    let (results, responded, open) = screen_state(&app, &poll, &spec);
    let join_url = (spec.audience.kind == mecha_manifest::AudienceKind::Link).then(|| {
        format!(
            "{}/p/{handle}/{poll_id}",
            app.config.base_url(crate::config::Role::Gate)
        )
    });
    let page = mecha_manifest::screen_page(
        &spec,
        &results,
        &mecha_manifest::ScreenPageOptions {
            join_url,
            responded,
            open,
            resolution: poll.resolution.clone(),
            theme: mecha_manifest::Theme::by_name(&app.config.theme),
            assets: "/p/a/".into(),
            live: true,
        },
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page.html,
    )
        .into_response()
}

/// `GET /p/{handle}/{poll_id}/screen/{token}/data.json` — the 2s truth.
pub async fn screen_data(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, poll_id, token)): Path<(String, String, String)>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((poll, spec)) = resolve_screen(&app, &handle, &poll_id, &token) else {
        return nothing_here();
    };
    let (results, responded, open) = screen_state(&app, &poll, &spec);
    let map: serde_json::Map<String, serde_json::Value> =
        mecha_manifest::results_fragments(&spec, &results)
            .into_iter()
            .map(|(qid, html)| (qid, html.into()))
            .collect();
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        serde_json::json!({
            "open": open,
            "count": mecha_manifest::screen_count_line(responded, open),
            "results": map,
        })
        .to_string(),
    )
        .into_response()
}

/// `GET /p/{handle}/{poll_id}/results.json` — the link page's live truth,
/// the browser's cookie standing in for the path token.
pub async fn link_results(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, poll_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((poll, spec)) = resolve_link(&app, &handle, &poll_id) else {
        return nothing_here();
    };
    let viewer = link_viewer(&app, &headers, &handle, &poll);
    results_payload(&app, &poll, &spec, viewer.as_deref(), None)
}

#[derive(serde::Deserialize)]
pub struct SavedQuery {
    #[serde(default)]
    saved: Option<u8>,
}

/// `GET /p/{handle}/{poll_id}/{token}` — the seeded grid, this
/// participant's answers, and everyone's heat. Mutates nothing.
pub async fn page(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, poll_id, token)): Path<(String, String, String)>,
    Query(query): Query<SavedQuery>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((poll, name)) = resolve(&app, &handle, &poll_id, &token) else {
        return nothing_here();
    };
    let notice = query
        .saved
        .map(|_| "Saved. You can change your answers any time until the poll closes.".to_string());
    render_any(&app, &poll, &name, notice)
}

/// `POST /p/{handle}/{poll_id}/{token}` — this participant's answers,
/// wholesale. The newest submission replaces the last: changing your mind
/// is ordinary. Redirects back to the GET (a reload must not resubmit) —
/// unless the caller asked for JSON, which is poll.js autosaving: a bare
/// 204 says saved, anything else tells the script to hand back the button.
pub async fn answer(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path((handle, poll_id, token)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let Some((poll, name)) = resolve(&app, &handle, &poll_id, &token) else {
        return nothing_here();
    };
    if !still_open(&poll, chrono::Utc::now()) {
        // A stale tab after the close: the read-only page is the answer,
        // and an autosave is told plainly rather than shown a page.
        if wants_json {
            return StatusCode::CONFLICT.into_response();
        }
        return render_any(&app, &poll, &name, None);
    }

    let raw = super::intake::form_values(&body);
    match poll.general_spec() {
        Ok(None) => {}
        Ok(Some(spec)) => {
            // The general ballot: decoded by the same crate that rendered
            // the field names, validated against the spec's own vocabulary,
            // refused wholesale on anything illegal — our own form never
            // produces an illegal value, so an error is a hand-built POST
            // and gets no partial store to probe with.
            let (ballot, errors) = validate_ballot(&spec, &ballot_from_form(&spec, &raw));
            if !errors.is_empty() {
                if wants_json {
                    return StatusCode::UNPROCESSABLE_ENTITY.into_response();
                }
                let what: Vec<String> = errors
                    .iter()
                    .map(|(question, why)| format!("{question}: {why}"))
                    .collect();
                return render_general(
                    &app,
                    &poll,
                    &spec,
                    Some(&name),
                    Some(name.clone()),
                    Some(format!("Nothing was saved — {}.", what.join("; "))),
                );
            }
            let payload = serde_json::to_string(&ballot).unwrap_or_else(|_| "{}".into());
            return match app.db.poll_answer(
                &crate::intake::hash_token(&token),
                &payload,
                &crate::db::now(),
            ) {
                Ok(true) if wants_json => StatusCode::NO_CONTENT.into_response(),
                Ok(true) => (
                    StatusCode::SEE_OTHER,
                    [(
                        header::LOCATION,
                        format!("/p/{handle}/{poll_id}/{token}?saved=1"),
                    )],
                )
                    .into_response(),
                Ok(false) if wants_json => StatusCode::CONFLICT.into_response(),
                Ok(false) => {
                    render_general(&app, &poll, &spec, Some(&name), Some(name.clone()), None)
                }
                Err(e) => {
                    tracing::error!(poll = %poll_id, error = %e, "storing answers");
                    Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
                }
            };
        }
        Err(e) => {
            tracing::error!(poll = %poll.id, error = %e, "unreadable poll spec");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response();
        }
    }
    let stored: Vec<StoredCandidate> = serde_json::from_str(&poll.candidates).unwrap_or_default();
    let mut answers = serde_json::Map::new();
    for candidate in &stored {
        let key = format!(
            "{}|{}",
            candidate
                .start
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            candidate.duration_minutes
        );
        // Only the three words, only for candidates the poll declared —
        // anything else in the body is ignored, not stored. Absent means
        // "no": silence is unavailability, when2meet's own rule.
        let answer = raw
            .get(&format!("a_{key}"))
            .and_then(|v| v.as_str())
            .and_then(PollAnswer::parse)
            .unwrap_or(PollAnswer::No);
        answers.insert(key, answer.as_str().into());
    }
    let payload = serde_json::Value::Object(answers).to_string();
    match app.db.poll_answer(
        &crate::intake::hash_token(&token),
        &payload,
        &crate::db::now(),
    ) {
        Ok(true) if wants_json => StatusCode::NO_CONTENT.into_response(),
        Ok(true) => (
            StatusCode::SEE_OTHER,
            [(
                header::LOCATION,
                format!("/p/{handle}/{poll_id}/{token}?saved=1"),
            )],
        )
            .into_response(),
        Ok(false) if wants_json => StatusCode::CONFLICT.into_response(),
        Ok(false) => render(&app, &poll, &name, None),
        Err(e) => {
            tracing::error!(poll = %poll_id, error = %e, "storing answers");
            Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// `GET /p/a/{name}` — the shared page assets, theme-only like `/s/`'s.
pub async fn asset(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
    Path(name): Path<String>,
) -> Response {
    if v1::not_on_gate(&origin).is_some() {
        return nothing_here();
    }
    let theme = mecha_manifest::Theme::by_name(&app.config.theme);
    for (asset_name, body) in mecha_manifest::booking_assets(&theme) {
        if asset_name == name {
            return (
                StatusCode::OK,
                [(
                    header::CONTENT_TYPE,
                    mecha_manifest::content_type(asset_name),
                )],
                body,
            )
                .into_response();
        }
    }
    nothing_here()
}
