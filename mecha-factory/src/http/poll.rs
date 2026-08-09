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
    ballot_from_form, poll_page, survey_page, tally_choice, tally_likert, tally_ranking,
    tally_vas, validate_ballot, Answer, Ballot, Identity, PollAnswer, PollCandidate,
    PollPageOptions, PollSpec, QuestionDisplay, QuestionKind, QuestionResults, Show,
    SurveyPageOptions, DEFAULT_SUPPRESSION_FLOOR,
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
fn still_open(poll: &PollRow, now: chrono::DateTime<chrono::Utc>) -> bool {
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
        Ok(Some(spec)) => render_general(app, poll, &spec, participant, notice),
        Err(e) => {
            tracing::error!(poll = %poll.id, error = %e, "unreadable poll spec");
            Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable").into_response()
        }
    }
}

/// The general poll's page: the stored spec's questions, this participant's
/// saved ballot, and results exactly as the policy allows this viewer —
/// decided here, where the bytes are emitted, never in the client.
fn render_general(
    app: &Shared,
    poll: &PollRow,
    spec: &PollSpec,
    participant: &str,
    notice: Option<String>,
) -> Response {
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
    let mine = ballots
        .iter()
        .find(|(name, _)| name == participant)
        .map(|(_, ballot)| ballot.clone())
        .unwrap_or_default();
    let open = still_open(poll, chrono::Utc::now());
    let identity = spec.results.identity(spec.audience.kind);
    let visible = match spec.results.show {
        Show::Live => true,
        Show::AfterVote => !mine.is_empty(),
        Show::AfterClose => !open,
        Show::Creator => false,
    };
    let results = visible.then(|| build_results(spec, &ballots, identity, open));
    let options = SurveyPageOptions {
        participant: Some(participant.to_string()),
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
        responded: others.iter().filter(|p| p.responded_at.is_some()).count(),
        total: Some(others.len()),
        open,
        notice,
        show: spec.results.show,
        identity,
    };
    let page = survey_page(spec, &mine, results.as_deref(), &options);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page.html,
    )
        .into_response()
}

/// Per-question results under the identity policy: tallies always, the
/// small-n suppression on anonymous polls, names only under `named`.
fn build_results(
    spec: &PollSpec,
    ballots: &[(String, Ballot)],
    identity: Identity,
    open: bool,
) -> Vec<QuestionResults> {
    spec.questions
        .iter()
        .map(|question| {
            let named: Vec<(&str, &Answer)> = ballots
                .iter()
                .filter_map(|(name, ballot)| {
                    ballot.get(&question.id).map(|a| (name.as_str(), a))
                })
                .collect();
            let answers: Vec<Answer> = named.iter().map(|(_, a)| (*a).clone()).collect();
            let n = answers.len();
            let display = if identity == Identity::Anonymous
                && n > 0
                && n < DEFAULT_SUPPRESSION_FLOOR
            {
                QuestionDisplay::Suppressed { n }
            } else {
                match &question.kind {
                    QuestionKind::Choice { options, .. } => QuestionDisplay::Choice {
                        tally: tally_choice(options, &answers),
                    },
                    QuestionKind::Ranking { options } => QuestionDisplay::Ranking {
                        tally: tally_ranking(options, &answers),
                        complete: !open,
                    },
                    QuestionKind::Likert { points, .. } => QuestionDisplay::Likert {
                        tally: tally_likert(*points, &answers),
                    },
                    QuestionKind::Vas { .. } => QuestionDisplay::Vas {
                        tally: tally_vas(&answers),
                    },
                    QuestionKind::Text { .. } => QuestionDisplay::Text {
                        entries: named
                            .iter()
                            .filter_map(|(name, answer)| match answer {
                                Answer::Text(text) => Some((
                                    (identity == Identity::Named)
                                        .then(|| (*name).to_string()),
                                    text.clone(),
                                )),
                                _ => None,
                            })
                            .collect(),
                    },
                    // A times question never reaches the general path:
                    // `put_poll` refuses it in a spec, and legacy rows have
                    // no spec at all. Nothing to draw is the honest render
                    // if one ever does.
                    QuestionKind::Times { .. } => QuestionDisplay::Text {
                        entries: Vec::new(),
                    },
                }
            };
            let voters = (identity == Identity::Named
                && !matches!(question.kind, QuestionKind::Text { .. })
                && !matches!(display, QuestionDisplay::Suppressed { .. }))
            .then(|| {
                named
                    .iter()
                    .map(|(name, answer)| {
                        ((*name).to_string(), answer_words(question, answer))
                    })
                    .collect()
            });
            QuestionResults { display, voters }
        })
        .collect()
}

/// One stored answer, in the option's own words where it has any.
fn answer_words(question: &mecha_manifest::PollQuestion, answer: &Answer) -> String {
    let label = |id: &String| {
        question
            .options()
            .iter()
            .find(|o| &o.id == id)
            .map(|o| o.label.clone())
            .unwrap_or_else(|| id.clone())
    };
    match answer {
        Answer::Choice(ids) => ids.iter().map(label).collect::<Vec<_>>().join(", "),
        Answer::Ranking(ids) => ids.iter().map(label).collect::<Vec<_>>().join(" › "),
        Answer::Likert(v) | Answer::Vas(v) => v.to_string(),
        Answer::Text(_) => String::new(), // rendered inline, never here
    }
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
                    &name,
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
                Ok(false) => render_general(&app, &poll, &spec, &name, None),
                Err(e) => {
                    tracing::error!(poll = %poll_id, error = %e, "storing answers");
                    Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                        .into_response()
                }
            };
        }
        Err(e) => {
            tracing::error!(poll = %poll.id, error = %e, "unreadable poll spec");
            return Failure::text(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                .into_response();
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
