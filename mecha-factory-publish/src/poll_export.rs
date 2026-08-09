//! `polls export --csv`: ballots as a spreadsheet, for the
//! instructor-in-a-spreadsheet case.
//!
//! One row per ballot, one column per question, built client-side from the
//! same status reply the tallies come from — no new endpoint, and the
//! identity policy arrives already enforced (an anonymous poll's rows come
//! back nameless, so the export has no name column to print).
//!
//! **Hardened against CSV injection.** A `text` answer is a stranger's
//! prose, and a cell that begins with `=`, `+`, `-`, `@` or a tab/CR is a
//! formula the moment Excel opens it — stranger prose reaching a code path,
//! the exact shape every other boundary in this system exists to stop. Such
//! cells are prefixed with `'`, the standard defusal, before the ordinary
//! RFC 4180 quoting.

use mecha_manifest::{Answer, Ballot, PollSpec};

/// One exported row: the participant's name when the policy shows one, and
/// their ballot.
pub type ExportRow = (Option<String>, Ballot);

/// The CSV, complete with header. Rows without ballots are not here —
/// response *rate* is `status`'s to report; the export is the ballots.
pub fn ballots_csv(spec: &PollSpec, rows: &[ExportRow]) -> String {
    let named = rows.iter().any(|(name, _)| name.is_some());
    let mut out = String::new();

    let mut header: Vec<String> = Vec::new();
    if named {
        header.push("name".into());
    }
    header.extend(spec.questions.iter().map(|q| q.id.clone()));
    push_row(&mut out, &header);

    for (name, ballot) in rows {
        let mut row: Vec<String> = Vec::new();
        if named {
            row.push(name.clone().unwrap_or_default());
        }
        for question in &spec.questions {
            row.push(match ballot.get(&question.id) {
                None => String::new(),
                Some(answer) => answer_cell(answer),
            });
        }
        push_row(&mut out, &row);
    }
    out
}

/// One answer as one cell: multiple choices joined `; `, a ranking joined
/// `>` best-first (the design's spelling), scales as their number, prose
/// verbatim — the hardening happens at the cell boundary, not here.
fn answer_cell(answer: &Answer) -> String {
    match answer {
        Answer::Choice(ids) => ids.join("; "),
        Answer::Ranking(ids) => ids.join(">"),
        Answer::Likert(v) | Answer::Vas(v) => v.to_string(),
        Answer::Text(text) => text.clone(),
    }
}

fn push_row(out: &mut String, cells: &[String]) {
    let encoded: Vec<String> = cells.iter().map(|cell| encode_cell(cell)).collect();
    out.push_str(&encoded.join(","));
    out.push_str("\r\n");
}

/// Defuse, then quote. The `'` prefix goes on before quoting so it is part
/// of the cell's text everywhere a spreadsheet looks.
fn encode_cell(cell: &str) -> String {
    let defused = if cell.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{cell}")
    } else {
        cell.to_string()
    };
    if defused.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", defused.replace('"', "\"\""))
    } else {
        defused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> PollSpec {
        PollSpec::from_toml(
            r#"
            title = "T"
            [[questions]]
            id = "pick"
            kind = "choice"
            max_choices = 2
            [[questions.options]]
            id = "a"
            label = "A"
            [[questions.options]]
            id = "b"
            label = "B"
            [[questions]]
            id = "note"
            kind = "text"
            max_length = 200
        "#,
        )
        .expect("a valid spec")
    }

    fn ballot(entries: &[(&str, Answer)]) -> Ballot {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn a_formula_shaped_answer_is_defused_before_it_is_quoted() {
        let rows = vec![(
            Some("Priya".to_string()),
            ballot(&[
                ("pick", Answer::Choice(vec!["a".into(), "b".into()])),
                ("note", Answer::Text("=HYPERLINK(\"http://evil\")".into())),
            ]),
        )];
        let csv = ballots_csv(&spec(), &rows);
        assert!(csv.starts_with("name,pick,note\r\n"), "{csv}");
        // The prefix lands inside the quotes, ahead of the formula.
        assert!(csv.contains("\"'=HYPERLINK(\"\"http://evil\"\")\""), "{csv}");
        assert!(csv.contains("a; b"), "{csv}");
    }

    #[test]
    fn an_anonymous_export_has_no_name_column_at_all() {
        let rows = vec![
            (None, ballot(&[("pick", Answer::Choice(vec!["a".into()]))])),
            (None, ballot(&[("pick", Answer::Choice(vec!["b".into()]))])),
        ];
        let csv = ballots_csv(&spec(), &rows);
        assert!(csv.starts_with("pick,note\r\n"), "{csv}");
        assert!(!csv.contains("name"), "{csv}");
    }

    #[test]
    fn commas_newlines_and_quotes_survive_the_round_trip_shape() {
        let rows = vec![(
            None,
            ballot(&[("note", Answer::Text("line one\nsays \"hi\", twice".into()))]),
        )];
        let csv = ballots_csv(&spec(), &rows);
        assert!(
            csv.contains("\"line one\nsays \"\"hi\"\", twice\""),
            "{csv}"
        );
    }

    #[test]
    fn a_leading_minus_is_defused_even_unquoted() {
        let rows = vec![(
            None,
            ballot(&[("note", Answer::Text("-2+3".into()))]),
        )];
        let csv = ballots_csv(&spec(), &rows);
        assert!(csv.contains("'-2+3"), "{csv}");
    }
}
