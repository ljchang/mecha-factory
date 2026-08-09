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

/// One CSV line, hardened and quoted like the export's own rows — shared
/// so `links.csv` (name,email,url out) speaks the same dialect ballots do.
pub fn csv_line(cells: &[String]) -> String {
    let mut out = String::new();
    push_row(&mut out, cells);
    out
}

/// `--roster students.csv`: `name,email` per line, a header line tolerated
/// (recognised by its email column not holding an `@`), blanks skipped.
/// Quoted names with commas are supported; nothing fancier — this is the
/// instructor's own file, and a shape it doesn't have is an error worth
/// hearing about, not guessing around.
pub fn parse_roster(text: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut rows = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, email) = split_roster_line(line)
            .ok_or_else(|| anyhow::anyhow!("roster line {}: not name,email", number + 1))?;
        if !email.contains('@') {
            if number == 0 {
                continue; // the header
            }
            anyhow::bail!("roster line {}: `{email}` is not an address", number + 1);
        }
        if name.is_empty() {
            anyhow::bail!("roster line {}: an empty name", number + 1);
        }
        rows.push((name, email));
    }
    anyhow::ensure!(!rows.is_empty(), "the roster holds no name,email rows");
    Ok(rows)
}

fn split_roster_line(line: &str) -> Option<(String, String)> {
    if let Some(rest) = line.strip_prefix('"') {
        let (name, rest) = rest.split_once('"')?;
        let email = rest.trim().strip_prefix(',')?.trim();
        return Some((name.trim().to_string(), email.to_string()));
    }
    let (name, email) = line.split_once(',')?;
    Some((name.trim().to_string(), email.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_roster_tolerates_a_header_quotes_and_blank_lines() {
        let rows = parse_roster(
            "name,email\n\n\"Chang, Luke\",luke@example.edu\nPriya,priya@example.edu\n",
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("Chang, Luke".to_string(), "luke@example.edu".to_string()),
                ("Priya".to_string(), "priya@example.edu".to_string()),
            ]
        );
    }

    #[test]
    fn a_bad_address_names_its_line() {
        let err = parse_roster("name,email\nPriya,not-an-address\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 2"), "{err}");
    }

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
        assert!(
            csv.contains("\"'=HYPERLINK(\"\"http://evil\"\")\""),
            "{csv}"
        );
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
        let rows = vec![(None, ballot(&[("note", Answer::Text("-2+3".into()))]))];
        let csv = ballots_csv(&spec(), &rows);
        assert!(csv.contains("'-2+3"), "{csv}");
    }
}
