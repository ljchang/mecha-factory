//! The general poll page: every question kind as a native form control,
//! complete with JavaScript off — enhancement arrives in a later step, and
//! nothing here waits for it.
//!
//! Two contracts live in this file on purpose, side by side, so they cannot
//! drift: [`survey_page`] renders the form's field names, and
//! [`ballot_from_form`] reads them back. The box runs both; a bundle writer
//! runs only the first; nobody else invents a field name.
//!
//! Results are rendered from [`QuestionResults`] the *server* built — the
//! visibility policy is enforced where bytes are emitted, so a page whose
//! viewer may not see results receives `None` and renders the reason. What
//! never happens here: results hidden by markup. Absent is absent.

use crate::poll::{
    Answer, Ballot, ChoiceTally, Identity, LikertTally, PollOption, PollQuestion, PollSpec,
    QuestionKind, RankingTally, Show, VasTally,
};
use crate::{booking::BookingPage, escape_text};

/// What the server supplies to render one viewer's page.
pub struct SurveyPageOptions {
    /// The greeting name — a roster participant's. `None` on a link poll,
    /// where nobody typed a name and the page greets nobody.
    pub participant: Option<String>,
    pub action: String,
    pub assets: String,
    pub theme: crate::Theme,
    pub deadline_local: Option<String>,
    pub responded: usize,
    /// Roster size; `None` on a link poll, where the denominator would be
    /// a guess.
    pub total: Option<usize>,
    pub open: bool,
    pub notice: Option<String>,
    /// The poll's policy, restated on the page: the promise is made before
    /// the vote, in plain words, by [`promise_line`].
    pub show: Show,
    pub identity: Identity,
}

/// One question's results, as the server decided this viewer may see them.
pub struct QuestionResults {
    pub display: QuestionDisplay,
    /// Who answered what, rendered — only under `identity = named`, and
    /// only ever built by the server.
    pub voters: Option<Vec<(String, String)>>,
}

pub enum QuestionDisplay {
    /// Too few respondents to break down anonymously.
    Suppressed { n: usize },
    Choice { tally: ChoiceTally },
    Likert { tally: LikertTally },
    Vas { tally: VasTally },
    /// `complete` when the poll is closed: the IRV rounds render only
    /// then, because rounds on a partial electorate imply a winner one
    /// more ballot can flip. Open polls show first preferences alone.
    Ranking { tally: RankingTally, complete: bool },
    /// (name when named, the prose). Escaped at render like everything.
    Text { entries: Vec<(Option<String>, String)> },
}

/// The identity promise in the voter's language, stated before they answer.
/// One wording, one place — a page that leaves the difference between
/// "anonymous to peers" and "anonymous to the organizer" ambiguous is lying
/// to somebody.
pub fn promise_line(show: Show, identity: Identity) -> String {
    let identity_words = match identity {
        Identity::Named => "Everyone in this poll can see how each person answered.",
        Identity::Creator => {
            "Other participants see only totals; the organizer can see individual answers."
        }
        Identity::Anonymous => {
            "Your name is not shown with results — not to other participants, \
             and not to the organizer. (Written answers still read like their \
             author.)"
        }
    };
    let show_words = match show {
        Show::Live => "Results are visible while the poll runs.",
        Show::AfterVote => "Results appear after you answer.",
        Show::AfterClose => "Results appear when the poll closes.",
        Show::Creator => "Results go to the organizer.",
    };
    format!("{show_words} {identity_words}")
}

/// Render the page: the form, this viewer's saved answers, and whatever
/// results the server decided to pass. `results`, when present, is parallel
/// to `spec.questions`.
pub fn survey_page(
    spec: &PollSpec,
    mine: &Ballot,
    results: Option<&[QuestionResults]>,
    options: &SurveyPageOptions,
) -> BookingPage {
    let mut body = String::new();
    body.push_str(&format!("<h1>{}</h1>\n", escape_text(&spec.title)));

    let mut intro = match &options.participant {
        Some(name) => format!("Hi {} — ", escape_text(name)),
        None => String::new(),
    };
    match options.total {
        Some(total) => intro.push_str(&format!(
            "{} of {} have answered so far.",
            options.responded, total
        )),
        None => intro.push_str(&format!("{} answered so far.", options.responded)),
    }
    body.push_str(&format!("<p class=\"intro\">{intro}</p>\n"));

    if let Some(notice) = &options.notice {
        body.push_str(&format!(
            "<p class=\"stale\" role=\"note\">{}</p>\n",
            escape_text(notice)
        ));
    }
    if let Some(deadline) = &options.deadline_local {
        body.push_str(&format!(
            "<p class=\"zone\">Answers close {}.</p>\n",
            escape_text(deadline)
        ));
    }
    body.push_str(&format!(
        "<p class=\"zone promise\">{}</p>\n",
        escape_text(&promise_line(options.show, options.identity))
    ));
    if !options.open {
        body.push_str(
            "<p class=\"empty\">This poll is closed; answers can no longer change.</p>\n",
        );
    }

    body.push_str(&format!(
        "<form method=\"post\" action=\"{}\" class=\"survey\">\n",
        escape_text(&options.action)
    ));
    for (index, question) in spec.questions.iter().enumerate() {
        body.push_str("<section class=\"question\">\n");
        let prompt = question.prompt.as_deref().unwrap_or(&spec.title);
        // The single-question poll's title is its prompt; repeating it as a
        // heading directly under itself would just be an echo.
        if question.prompt.is_some() || spec.questions.len() > 1 {
            body.push_str(&format!(
                "<h2>{}{}</h2>\n",
                escape_text(prompt),
                if question.required {
                    " <span class=\"req\" title=\"required\">*</span>"
                } else {
                    ""
                }
            ));
        }
        widget(&mut body, question, mine.get(&question.id), options.open);
        if let Some(results) = results {
            if let Some(question_results) = results.get(index) {
                render_results(&mut body, question, question_results);
            }
        }
        body.push_str("</section>\n");
    }
    if results.is_none() && options.show != Show::Creator {
        // Owed but not yet due — say when. (`Creator` already said where
        // they go, in the promise line.)
        let when = match options.show {
            Show::AfterVote => "Results will appear here after you answer.",
            _ => "Results will appear here when the poll closes.",
        };
        body.push_str(&format!("<p class=\"zone\">{when}</p>\n"));
    }
    if options.open {
        body.push_str(
            "<p class=\"savestate\" id=\"savestate\" aria-live=\"polite\" hidden></p>\n\
             <button type=\"submit\">Save my answers</button>\n",
        );
    }
    body.push_str("</form>\n");

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n\
         {}\n\
         <link rel=\"stylesheet\" href=\"{}booking.css\">\n\
         </head>\n<body>\n{}<main class=\"booking\">\n{}</main>\n</body>\n</html>\n",
        escape_text(&spec.title),
        crate::brand::FAVICON_LINK,
        escape_text(&options.assets),
        crate::form::site_header(),
        body
    );
    BookingPage {
        html,
        style: format!(
            "{}{}{}{}",
            options.theme.css(),
            crate::form::FORM_STRUCTURE,
            crate::booking::BOOKING_STRUCTURE,
            SURVEY_STRUCTURE
        ),
    }
}

fn widget(body: &mut String, question: &PollQuestion, mine: Option<&Answer>, open: bool) {
    let disabled = if open { "" } else { " disabled" };
    let qid = escape_text(&question.id);
    match &question.kind {
        QuestionKind::Choice {
            min_choices,
            max_choices,
            options,
        } => {
            let picked: &[String] = match mine {
                Some(Answer::Choice(ids)) => ids,
                _ => &[],
            };
            if *max_choices > 1 {
                let cap = if min_choices == max_choices {
                    format!("Pick {max_choices}.")
                } else {
                    format!("Pick {min_choices}–{max_choices}.")
                };
                body.push_str(&format!("<p class=\"cap\">{cap}</p>\n"));
            }
            for option in options {
                let oid = escape_text(&option.id);
                let checked = if picked.contains(&option.id) {
                    " checked"
                } else {
                    ""
                };
                let control = if *max_choices == 1 {
                    format!(
                        "<input type=\"radio\" name=\"q_{qid}\" value=\"{oid}\"{checked}{disabled}>"
                    )
                } else {
                    format!(
                        "<input type=\"checkbox\" name=\"q_{qid}_o_{oid}\" \
                         value=\"1\"{checked}{disabled}>"
                    )
                };
                body.push_str(&format!(
                    "<label class=\"opt\">{control} <span class=\"optlabel\">{}</span>{}{}</label>\n",
                    escape_text(&option.label),
                    option
                        .detail
                        .as_deref()
                        .map(|d| format!(" <span class=\"detail\">{}</span>", escape_text(d)))
                        .unwrap_or_default(),
                    option
                        .link
                        .as_deref()
                        .map(|l| format!(
                            " <a href=\"{}\" rel=\"noopener\">link</a>",
                            escape_text(l)
                        ))
                        .unwrap_or_default(),
                ));
            }
        }
        QuestionKind::Ranking { options } => {
            body.push_str(
                "<p class=\"cap\">Rank your preferences — 1 is best. \
                 Ranking only some is fine.</p>\n",
            );
            let ranked: &[String] = match mine {
                Some(Answer::Ranking(ids)) => ids,
                _ => &[],
            };
            for option in options {
                let oid = escape_text(&option.id);
                let position = ranked.iter().position(|id| id == &option.id);
                body.push_str(&format!(
                    "<label class=\"opt rank\"><select name=\"q_{qid}_o_{oid}\"{disabled}>\
                     <option value=\"\">—</option>"
                ));
                for rank in 1..=options.len() {
                    let selected = if position == Some(rank - 1) {
                        " selected"
                    } else {
                        ""
                    };
                    body.push_str(&format!("<option value=\"{rank}\"{selected}>{rank}</option>"));
                }
                body.push_str(&format!(
                    "</select> <span class=\"optlabel\">{}</span></label>\n",
                    escape_text(&option.label)
                ));
            }
        }
        QuestionKind::Likert {
            points,
            labels,
            label_min,
            label_max,
        } => {
            let mine = match mine {
                Some(Answer::Likert(v)) => Some(*v),
                _ => None,
            };
            body.push_str("<div class=\"scale\" role=\"radiogroup\">\n");
            for point in 1..=*points {
                let label = match labels {
                    Some(labels) => labels[usize::from(point) - 1].clone(),
                    None => {
                        let end = if point == 1 {
                            label_min.as_deref()
                        } else if point == *points {
                            label_max.as_deref()
                        } else {
                            None
                        };
                        match end {
                            Some(words) => format!("{point} — {words}"),
                            None => point.to_string(),
                        }
                    }
                };
                let checked = if mine == Some(point) { " checked" } else { "" };
                body.push_str(&format!(
                    "<label class=\"point\"><input type=\"radio\" name=\"q_{qid}\" \
                     value=\"{point}\"{checked}{disabled}> {}</label>\n",
                    escape_text(&label)
                ));
            }
            body.push_str("</div>\n");
        }
        QuestionKind::Vas {
            anchor_min,
            anchor_max,
        } => {
            let value = match mine {
                Some(Answer::Vas(v)) => v.to_string(),
                _ => String::new(),
            };
            // A number input, not a range: a native slider cannot express
            // "untouched", and an untouched control submitting its resting
            // place would invent a midpoint. The enhanced thumbless track
            // is a later step; the meaning ships first.
            body.push_str(&format!(
                "<p class=\"vas\"><span class=\"anchor\">0 — {}</span> \
                 <input type=\"number\" name=\"q_{qid}\" min=\"0\" max=\"100\" \
                 inputmode=\"numeric\" value=\"{}\"{disabled}> \
                 <span class=\"anchor\">100 — {}</span></p>\n",
                escape_text(anchor_min),
                escape_text(&value),
                escape_text(anchor_max),
            ));
        }
        QuestionKind::Text { max_length } => {
            let value = match mine {
                Some(Answer::Text(text)) => text.as_str(),
                _ => "",
            };
            body.push_str(&format!(
                "<textarea name=\"q_{qid}\" maxlength=\"{max_length}\" rows=\"4\"{disabled}>{}\
                 </textarea>\n<p class=\"cap\">Up to {max_length} characters.</p>\n",
                escape_text(value)
            ));
        }
        QuestionKind::Times { .. } => {
            // The seeded grid has its own page (`poll_page`); a general
            // render of a times question would be a second implementation
            // of it. The spec checker keeps them apart.
        }
    }
}

fn render_results(body: &mut String, question: &PollQuestion, results: &QuestionResults) {
    body.push_str("<div class=\"results\">\n");
    match &results.display {
        QuestionDisplay::Suppressed { n } => {
            body.push_str(&format!(
                "<p class=\"cap\">{n} so far — too few to break down \
                 anonymously.</p>\n"
            ));
        }
        QuestionDisplay::Choice { tally } => {
            let labels: Vec<(&str, &str)> = question
                .options()
                .iter()
                .map(|o| (o.id.as_str(), o.label.as_str()))
                .collect();
            for (id, count) in &tally.counts {
                let label = labels
                    .iter()
                    .find(|(oid, _)| oid == id)
                    .map(|(_, l)| *l)
                    .unwrap_or(id.as_str());
                bar(body, label, *count, tally.n);
            }
        }
        QuestionDisplay::Likert { tally } => {
            for (index, count) in tally.counts.iter().enumerate() {
                bar(body, &format!("{}", index + 1), *count, tally.n);
            }
            if let (Some(median), Some(mean)) = (tally.median, tally.mean) {
                body.push_str(&format!(
                    "<p class=\"cap\">{} answers · median {} · mean {:.1}</p>\n",
                    tally.n, median, mean
                ));
            }
        }
        QuestionDisplay::Vas { tally } => {
            if let (Some(median), Some(mean)) = (tally.median, tally.mean) {
                body.push_str(&format!(
                    "<p class=\"cap\">{} answers · median {} · mean {:.1}</p>\n",
                    tally.n, median, mean
                ));
            }
            for (index, count) in tally.deciles.iter().enumerate() {
                let label = if index == 9 {
                    "90–100".to_string()
                } else {
                    format!("{}–{}", index * 10, index * 10 + 9)
                };
                bar(body, &label, *count, tally.n);
            }
        }
        QuestionDisplay::Ranking { tally, complete } => {
            let labels: Vec<(&str, &str)> = question
                .options()
                .iter()
                .map(|o| (o.id.as_str(), o.label.as_str()))
                .collect();
            let name = |id: &str| {
                labels
                    .iter()
                    .find(|(oid, _)| *oid == id)
                    .map(|(_, l)| (*l).to_string())
                    .unwrap_or_else(|| id.to_string())
            };
            body.push_str("<p class=\"cap\">First preferences:</p>\n");
            for (id, count) in &tally.first_preferences {
                bar(body, &name(id), *count, tally.n);
            }
            if *complete {
                if let Some(winner) = &tally.winner {
                    body.push_str(&format!(
                        "<p class=\"cap\"><strong>Winner by instant runoff: {}</strong></p>\n",
                        escape_text(&name(winner))
                    ));
                }
                for (number, round) in tally.rounds.iter().enumerate() {
                    let standing: Vec<String> = round
                        .counts
                        .iter()
                        .map(|(id, count)| format!("{} {count}", escape_text(&name(id))))
                        .collect();
                    let eliminated = match &round.eliminated {
                        Some(id) => format!(" — {} eliminated", escape_text(&name(id))),
                        None => String::new(),
                    };
                    body.push_str(&format!(
                        "<p class=\"cap\">Round {}: {}{}</p>\n",
                        number + 1,
                        standing.join(" · "),
                        eliminated
                    ));
                }
            }
        }
        QuestionDisplay::Text { entries } => {
            body.push_str("<ul class=\"prose-answers\">\n");
            for (author, text) in entries {
                let byline = author
                    .as_deref()
                    .map(|name| format!(" <span class=\"detail\">— {}</span>", escape_text(name)))
                    .unwrap_or_default();
                body.push_str(&format!("<li>{}{byline}</li>\n", escape_text(text)));
            }
            body.push_str("</ul>\n");
        }
    }
    if let Some(voters) = &results.voters {
        body.push_str("<details class=\"voters\"><summary>Who answered what</summary><ul>\n");
        for (name, answer) in voters {
            body.push_str(&format!(
                "<li>{}: {}</li>\n",
                escape_text(name),
                escape_text(answer)
            ));
        }
        body.push_str("</ul></details>\n");
    }
    body.push_str("</div>\n");
}

/// A count as a native `<meter>` beside its words — no script, no styling
/// tricks under a CSP that forbids inline style, and the number always
/// rides in text where a screen reader (or colour-blind reader) lives.
fn bar(body: &mut String, label: &str, count: usize, n: usize) {
    body.push_str(&format!(
        "<p class=\"tallybar\"><span class=\"optlabel\">{}</span> \
         <meter max=\"{}\" value=\"{count}\"></meter> \
         <span class=\"count\">{count}</span></p>\n",
        escape_text(label),
        n.max(1),
    ));
}

/// Read a survey POST back into the raw ballot [`crate::poll::validate_ballot`]
/// takes. Lives beside the renderer so the field names have one home; the
/// box's job is only to hand the form pairs over.
pub fn ballot_from_form(
    spec: &PollSpec,
    form: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let text_of = |key: &str| -> Option<String> {
        form.get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    let mut raw = serde_json::Map::new();
    for question in &spec.questions {
        let qid = &question.id;
        match &question.kind {
            QuestionKind::Choice {
                max_choices,
                options,
                ..
            } => {
                let picked: Vec<serde_json::Value> = if *max_choices == 1 {
                    text_of(&format!("q_{qid}")).into_iter().map(Into::into).collect()
                } else {
                    options
                        .iter()
                        .filter(|o| form.contains_key(&format!("q_{qid}_o_{}", o.id)))
                        .map(|o| o.id.clone().into())
                        .collect()
                };
                if !picked.is_empty() {
                    raw.insert(qid.clone(), picked.into());
                }
            }
            QuestionKind::Ranking { options } => {
                let mut ranked: Vec<(usize, usize, &PollOption)> = options
                    .iter()
                    .enumerate()
                    .filter_map(|(declared, option)| {
                        let rank: usize =
                            text_of(&format!("q_{qid}_o_{}", option.id))?.parse().ok()?;
                        Some((rank, declared, option))
                    })
                    .collect();
                // Two options given the same rank keep their declared order —
                // arbitrary, visible on the re-rendered page, and correctable.
                ranked.sort_by_key(|(rank, declared, _)| (*rank, *declared));
                if !ranked.is_empty() {
                    raw.insert(
                        qid.clone(),
                        ranked
                            .into_iter()
                            .map(|(_, _, o)| serde_json::Value::from(o.id.clone()))
                            .collect::<Vec<_>>()
                            .into(),
                    );
                }
            }
            QuestionKind::Likert { .. } | QuestionKind::Vas { .. } | QuestionKind::Text { .. } => {
                if let Some(value) = text_of(&format!("q_{qid}")) {
                    raw.insert(qid.clone(), value.into());
                }
            }
            QuestionKind::Times { .. } => {}
        }
    }
    raw
}

/// The survey page's own structure, appended to the booking stylesheet.
pub(crate) const SURVEY_STRUCTURE: &str = r#"
/* --- the survey ---------------------------------------------------------- */
.survey .question { margin:1.5rem 0; padding:1rem 0; border-top:1px solid var(--line, #8884); }
.survey .question h2 { font-size:1.05rem; margin:0 0 .5rem; }
.survey .req { color:var(--accent, inherit); }
.survey .opt { display:block; margin:.35rem 0; }
.survey .opt .detail, .survey .cap { opacity:.75; font-size:.9rem; }
.survey .cap { margin:.25rem 0; }
.survey .scale { display:flex; flex-wrap:wrap; gap:.75rem; }
.survey .scale .point { display:flex; align-items:center; gap:.3rem; }
.survey .vas { display:flex; align-items:center; gap:.75rem; flex-wrap:wrap; }
.survey .vas input { width:5.5rem; }
.survey textarea { width:100%; max-width:36rem; }
.survey .results { margin:.75rem 0 0; }
.survey .tallybar { display:flex; align-items:center; gap:.5rem; margin:.2rem 0; }
.survey .tallybar .optlabel { min-width:9rem; }
.survey .tallybar meter { flex:1; max-width:18rem; }
.survey .prose-answers li { margin:.4rem 0; }
.survey .voters { margin:.5rem 0; font-size:.9rem; }
.survey .promise { font-style:italic; }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::AudienceKind;

    fn two_question_spec() -> PollSpec {
        PollSpec::from_toml(
            r#"
            title = "Feedback"
            [[questions]]
            id = "pace"
            prompt = "The pace is right."
            kind = "likert"
            points = 5
            [[questions]]
            id = "keep"
            prompt = "What should we keep?"
            kind = "text"
            max_length = 200
        "#,
        )
        .expect("a valid spec")
    }

    fn options() -> SurveyPageOptions {
        SurveyPageOptions {
            participant: Some("Priya".into()),
            action: String::new(),
            assets: "/p/a/".into(),
            theme: crate::theme::NOCTURNE,
            deadline_local: None,
            responded: 1,
            total: Some(3),
            open: true,
            notice: None,
            show: Show::AfterVote,
            identity: Identity::Anonymous,
        }
    }

    #[test]
    fn the_form_round_trips_through_ballot_from_form() {
        let spec = PollSpec::from_toml(
            r#"
            title = "T"
            [[questions]]
            id = "pick"
            kind = "choice"
            min_choices = 1
            max_choices = 2
            [[questions.options]]
            id = "a"
            label = "A"
            [[questions.options]]
            id = "b"
            label = "B"
            [[questions]]
            id = "order"
            kind = "ranking"
            [[questions.options]]
            id = "x"
            label = "X"
            [[questions.options]]
            id = "y"
            label = "Y"
            [[questions]]
            id = "mood"
            kind = "vas"
            anchor_min = "low"
            anchor_max = "high"
        "#,
        )
        .expect("valid");
        let form = serde_json::json!({
            "q_pick_o_b": "1",
            "q_order_o_y": "1",
            "q_order_o_x": "2",
            "q_mood": "62",
            "q_stray": "ignored",
        });
        let raw = ballot_from_form(&spec, form.as_object().unwrap());
        let (ballot, errors) = crate::poll::validate_ballot(&spec, &raw);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(ballot["pick"], Answer::Choice(vec!["b".into()]));
        assert_eq!(
            ballot["order"],
            Answer::Ranking(vec!["y".into(), "x".into()])
        );
        assert_eq!(ballot["mood"], Answer::Vas(62));
        assert!(!ballot.contains_key("q_stray"));
    }

    #[test]
    fn the_page_carries_the_promise_and_no_results_before_the_vote() {
        let spec = two_question_spec();
        let page = survey_page(&spec, &Ballot::new(), None, &options());
        assert!(page.html.contains("Results appear after you answer."));
        assert!(page.html.contains("not to the organizer"));
        assert!(page.html.contains("Results will appear here after you answer."));
        assert!(!page.html.contains("tallybar"));
        // Both widgets render, JS-off complete.
        assert!(page.html.contains("name=\"q_pace\""));
        assert!(page.html.contains("name=\"q_keep\""));
        assert!(page.html.contains("maxlength=\"200\""));
    }

    #[test]
    fn a_closed_page_disables_every_control_and_says_so() {
        let spec = two_question_spec();
        let mut opts = options();
        opts.open = false;
        let page = survey_page(&spec, &Ballot::new(), None, &opts);
        assert!(page.html.contains("closed"));
        assert!(!page.html.contains("<button type=\"submit\""));
        assert!(page.html.matches(" disabled").count() >= 6);
    }

    #[test]
    fn results_render_as_meters_with_counts_in_text() {
        let spec = two_question_spec();
        let answers = [3u8, 4, 4].map(Answer::Likert);
        let results = vec![
            QuestionResults {
                display: QuestionDisplay::Likert {
                    tally: crate::poll::tally_likert(5, &answers),
                },
                voters: None,
            },
            QuestionResults {
                display: QuestionDisplay::Text {
                    entries: vec![(None, "More coffee <3".into())],
                },
                voters: None,
            },
        ];
        let page = survey_page(&spec, &Ballot::new(), Some(&results), &options());
        assert!(page.html.contains("<meter max=\"3\" value=\"2\">"));
        assert!(page.html.contains("median 4"));
        // The prose is escaped, not interpolated.
        assert!(page.html.contains("More coffee &lt;3"));
    }

    #[test]
    fn saved_answers_re_render_checked_and_filled() {
        let spec = two_question_spec();
        let mut mine = Ballot::new();
        mine.insert("pace".into(), Answer::Likert(4));
        mine.insert("keep".into(), Answer::Text("the katas".into()));
        let page = survey_page(&spec, &mine, None, &options());
        assert!(page.html.contains("value=\"4\" checked"));
        assert!(page.html.contains(">the katas</textarea>"));
    }

    #[test]
    fn the_promise_distinguishes_creator_from_anonymous() {
        let creator = promise_line(Show::Live, Identity::Creator);
        let anonymous = promise_line(Show::Live, Identity::Anonymous);
        assert!(creator.contains("organizer can see"));
        assert!(anonymous.contains("not to the organizer"));
        assert_ne!(creator, anonymous);
        // And the audience resolver stays consistent with the wording.
        assert_eq!(
            crate::poll::ResultsPolicy::default().identity(AudienceKind::Link),
            Identity::Anonymous
        );
    }
}
