//! The shipped starter manifests, by name: what a new form begins as.
//!
//! Each is a whole manifest a person copies and edits — comments included,
//! because the comments are the guidance (`types/*.toml` say what each
//! section is for). Embedded here rather than read from a path, so a
//! published client carries them; the files stay the one source.

use crate::{valid_id, ManifestError, RequestType, Result};

/// `(name, manifest)` for every starter, in a stable order.
pub const STARTERS: &[(&str, &str)] = &[
    ("book", include_str!("../types/book.toml")),
    (
        "lab-application",
        include_str!("../types/lab-application.toml"),
    ),
    ("letter", include_str!("../types/letter.toml")),
    ("meeting", include_str!("../types/meeting.toml")),
    ("speaking", include_str!("../types/speaking.toml")),
];

/// The starter `name`, re-identified as `id`: the top-level `id = "…"` line
/// replaced in place, so the starter's comments survive, and the result
/// checked whole — a starter that does not parse under its new id is a bug
/// in this function, not a manifest to hand anyone.
pub fn starter_as(name: &str, id: &str) -> Result<String> {
    let Some((_, text)) = STARTERS.iter().find(|(n, _)| *n == name) else {
        return Err(ManifestError::invalid(format!(
            "no starter called `{name}`; there are: {}",
            STARTERS
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    valid_id(id, "request type id")?;
    let mut replaced = false;
    let out: Vec<String> = text
        .lines()
        .map(|line| {
            if !replaced && line.starts_with("id = ") {
                replaced = true;
                format!("id = \"{id}\"")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !replaced {
        return Err(ManifestError::invalid(format!(
            "starter `{name}` has no top-level id line"
        )));
    }
    let out = out.join("\n") + "\n";
    let parsed = RequestType::from_toml(&out)?;
    // Checked in the build that ships, not only in debug: replacing "the
    // first `id = ` line" is a promise about the starter's layout, and a
    // starter whose first such line sat inside a string would keep its id.
    if parsed.id != id {
        return Err(ManifestError::invalid(format!(
            "starter `{name}` kept its id `{}` instead of taking `{id}`",
            parsed.id
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_starter_parses_and_takes_a_new_id_with_its_comments() {
        for (name, text) in STARTERS {
            let parsed = RequestType::from_toml(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(parsed.id, *name, "a starter's file name is its id");
            let renamed = starter_as(name, "office-hours").unwrap();
            assert_eq!(RequestType::from_toml(&renamed).unwrap().id, "office-hours");
            // The comments are the guidance; they come along.
            let comments = |t: &str| {
                t.lines()
                    .filter(|l| l.trim_start().starts_with('#'))
                    .count()
            };
            assert_eq!(comments(&renamed), comments(text), "{name}");
        }
    }

    #[test]
    fn an_unknown_starter_or_a_bad_id_is_refused_by_name() {
        let err = starter_as("nope", "x").unwrap_err().to_string();
        assert!(err.contains("there are: book"), "{err}");
        assert!(starter_as("meeting", "Not An Id").is_err());
    }
}
