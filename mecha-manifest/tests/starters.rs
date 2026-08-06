//! Every shipped starter has to load, generate, and round-trip.
//!
//! Request-type starters ship as **manifests, not code** — copy one, edit the
//! fields, and the form, both validators, the schema and the tool declarations
//! regenerate. Which makes them data that can rot silently, so they are checked
//! like anything else: a starter that stopped parsing after a field kind
//! changed would otherwise be discovered by whoever copied it.

use mecha_manifest::{FormOptions, RequestType};
use std::path::{Path, PathBuf};

fn types_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("types")
}

fn starters() -> Vec<(String, RequestType)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(types_dir()).expect("types/ exists") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed =
            RequestType::from_toml(&text).unwrap_or_else(|e| panic!("{name} does not load: {e}"));
        out.push((name, parsed));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!out.is_empty(), "no starters found in {:?}", types_dir());
    out
}

#[test]
fn every_starter_loads_and_names_itself_after_its_file() {
    for (name, t) in starters() {
        assert_eq!(
            format!("{}.toml", t.id),
            name,
            "a starter's id has to match its filename, since both are the URL"
        );
        assert!(!t.fields.is_empty());
    }
}

/// Every starter must be able to reply to whoever sent it. A typed request with
/// no way back is a dead end that the sender experiences as silence, which is
/// the exact failure the whole system exists to fix.
#[test]
fn every_starter_can_reach_the_person_who_submitted_it() {
    for (name, t) in starters() {
        let email = t
            .field("requester_email")
            .unwrap_or_else(|| panic!("{name} has no requester_email"));
        assert!(email.required, "{name}: requester_email must be required");
    }
}

/// A starter with an unconstrained free-text field is a starter that teaches
/// the wrong thing to whoever copies it.
#[test]
fn every_free_text_field_in_every_starter_is_capped() {
    for (name, t) in starters() {
        for field in t.free_text_fields() {
            let cap = field.max_length().unwrap_or(0);
            assert!(cap > 0, "{name}: {} has no cap", field.name);
            assert!(
                cap <= 4000,
                "{name}: {} allows {cap} characters, which is an essay on an \
                 unauthenticated endpoint",
                field.name
            );
        }
    }
}

#[test]
fn every_starter_generates_a_schema_and_a_form() {
    for (name, t) in starters() {
        let schema = t.json_schema();
        assert_eq!(schema["type"], "object", "{name}");
        assert_eq!(
            schema["additionalProperties"],
            serde_json::json!(false),
            "{name}: an undeclared field must be an error, never a silent drop"
        );

        let page = t.form(&FormOptions::default());
        assert!(page.html.starts_with("<!doctype html>"), "{name}");
        assert!(page.html.contains("</form>"), "{name}");
        // Nothing executable inline, so the strictest CSP holds unrelaxed.
        assert!(
            !page.html.contains("onclick=") && !page.html.contains("onchange="),
            "{name}: an inline handler would force script-src 'unsafe-inline'"
        );
    }
}

#[test]
fn every_starter_survives_a_toml_round_trip() {
    for (name, t) in starters() {
        let again = RequestType::from_toml(&t.to_toml())
            .unwrap_or_else(|e| panic!("{name} does not round-trip: {e}"));
        assert_eq!(again.id, t.id);
        assert_eq!(again.fields.len(), t.fields.len(), "{name}");
        assert_eq!(again.steps.len(), t.steps.len(), "{name}");
        assert_eq!(
            again.json_schema(),
            t.json_schema(),
            "{name}: the schema changed across a round trip"
        );
    }
}

/// The conditional-step rule, exercised end to end on a shipped manifest rather
/// than on a fixture invented to pass.
#[test]
fn the_speaking_starter_enforces_its_travel_step_in_both_directions() {
    let t = starters()
        .into_iter()
        .find(|(name, _)| name == "speaking.toml")
        .expect("the speaking starter")
        .1;

    let base = serde_json::json!({
        "requester_name": "A Person",
        "requester_email": "person@example.edu",
        "organisation": "Somewhere",
        "event_name": "A seminar series",
        "event_date": "2026-11-02",
        "talk_kind": "seminar",
        "understands_request": "on",
    });

    // Remote: the travel step is skipped, so its fields are neither required...
    let mut remote = base.clone();
    remote["format"] = serde_json::json!("remote");
    t.validate(remote.as_object().unwrap())
        .expect("a remote invitation needs no travel answers");

    // ...nor accepted, because the browser never showed them.
    let mut sneaky = remote.clone();
    sneaky["travel_covered"] = serde_json::json!(true);
    let errors = t.validate(sneaky.as_object().unwrap()).unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, "travel_covered");

    // In person: the step is shown, so its fields are accepted.
    let mut in_person = base;
    in_person["format"] = serde_json::json!("in_person");
    in_person["travel_covered"] = serde_json::json!(true);
    let accepted = t.validate(in_person.as_object().unwrap()).unwrap();
    assert_eq!(accepted.values["travel_covered"], serde_json::json!(true));
}

/// The handoff to the quarantine layer, on real manifests: exactly the prose
/// fields, and nothing structural.
#[test]
fn free_text_never_includes_a_field_whose_values_are_ours() {
    for (name, t) in starters() {
        for field in t.free_text_fields() {
            assert!(
                !matches!(
                    field.kind,
                    mecha_manifest::FieldKind::Select { .. }
                        | mecha_manifest::FieldKind::MultiSelect { .. }
                        | mecha_manifest::FieldKind::Bool
                        | mecha_manifest::FieldKind::Date { .. }
                        | mecha_manifest::FieldKind::Integer { .. }
                ),
                "{name}: {} is not free text",
                field.name
            );
        }
    }
}
