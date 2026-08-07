//! The manifest as a JSON Schema — the wire contract, and the thing a drained
//! record is checked against by anyone who is not us.
//!
//! Emitted rather than hand-written for the obvious reason: two descriptions of
//! one shape drift, and this one is the description a *third party* reads. The
//! same file becomes the MCP tool's `inputSchema` and the A2A skill's parameter
//! declaration, which is what makes "a human with a browser, an agent with MCP,
//! and an agent doing discovery all arrive at the same typed object" true by
//! construction instead of by discipline.
//!
//! **What is deliberately not expressed here: conditional requirement.** JSON
//! Schema can encode it — `if`/`then`, `dependentRequired` — and doing so would
//! be a second implementation of [`RequestType::visible_fields`] that has to
//! agree with the Rust one forever. So the schema states what is *always* true
//! (the fields, their types, their caps, their enums, the unconditionally
//! required ones) and [`RequestType::validate`] remains the authority on the
//! rest. A schema that is a subset of the validator is safe; one that
//! contradicts it is a bug that surfaces as a rejection nobody can explain.
//!
//! Draft 2020-12, and `additionalProperties: false` — an undeclared field is an
//! error at both ends, never a silent drop.

use serde_json::{json, Map, Value};

use crate::{Field, FieldKind, RequestType};

impl RequestType {
    /// The JSON Schema for a submission of this type.
    pub fn json_schema(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();

        for field in &self.fields {
            properties.insert(field.name.clone(), field_schema(field));
            // Only the unconditionally required ones. A field behind a
            // `show_when` is required by the validator when it is visible, and
            // saying so here would mean maintaining that logic twice.
            if field.required && field.show_when.is_none() && !self.hidden_by_a_step(&field.name) {
                required.push(Value::String(field.name.clone()));
            }
        }

        for ack in &self.acknowledgments {
            properties.insert(
                ack.id.clone(),
                json!({
                    "type": "boolean",
                    "const": true,
                    "title": ack.label,
                    "description": ack.description,
                }),
            );
            required.push(Value::String(ack.id.clone()));
        }

        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": format!("mecha-factory:request/{}/v{}", self.id, self.version),
            "title": self.title,
            "description": self.description,
            "type": "object",
            "properties": Value::Object(properties),
            "required": Value::Array(required),
            "additionalProperties": false,
        })
    }

    fn hidden_by_a_step(&self, name: &str) -> bool {
        self.steps
            .iter()
            .any(|s| s.show_when.is_some() && s.fields.iter().any(|f| f == name))
    }
}

fn field_schema(field: &Field) -> Value {
    let mut schema = match &field.kind {
        FieldKind::Text {
            max_length,
            pattern,
        } => {
            let mut s = json!({ "type": "string", "maxLength": max_length });
            // Carried into the schema even though the validator does not run it
            // — a consumer with a regex engine may, and the *declaration* is
            // part of the contract. See `validate.rs` for why we do not.
            if let Some(pattern) = pattern {
                s["pattern"] = json!(pattern);
            }
            s
        }
        FieldKind::LongText { max_length } => json!({ "type": "string", "maxLength": max_length }),
        FieldKind::Email { max_length } => {
            json!({ "type": "string", "format": "email", "maxLength": max_length })
        }
        FieldKind::Url { max_length } => {
            json!({ "type": "string", "format": "uri", "maxLength": max_length })
        }
        FieldKind::Date { min, max } => {
            let mut s = json!({ "type": "string", "format": "date" });
            // ISO dates are fixed-width, so a lexicographic bound is exactly the
            // same bound the validator applies.
            if let Some(min) = min {
                s["formatMinimum"] = json!(min);
            }
            if let Some(max) = max {
                s["formatMaximum"] = json!(max);
            }
            s
        }
        FieldKind::Integer { min, max } => {
            let mut s = json!({ "type": "integer" });
            if let Some(min) = min {
                s["minimum"] = json!(min);
            }
            if let Some(max) = max {
                s["maximum"] = json!(max);
            }
            s
        }
        FieldKind::Select { options } => json!({
            "type": "string",
            "enum": options.iter().map(|o| o.value.clone()).collect::<Vec<_>>(),
        }),
        FieldKind::MultiSelect {
            options,
            max_choices,
        } => {
            let mut s = json!({
                "type": "array",
                "uniqueItems": true,
                "items": {
                    "type": "string",
                    "enum": options.iter().map(|o| o.value.clone()).collect::<Vec<_>>(),
                },
            });
            if let Some(max) = max_choices {
                s["maxItems"] = json!(max);
            }
            s
        }
        FieldKind::Bool => json!({ "type": "boolean" }),
        // The value is the box's measurements about the bytes, not the bytes
        // — see `FileMeta` in validate.rs, which this mirrors.
        FieldKind::File { max_bytes, accept } => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["filename", "size", "sha256", "content_type"],
            "properties": {
                "filename": { "type": "string", "maxLength": 255 },
                "size": { "type": "integer", "minimum": 1, "maximum": max_bytes },
                "sha256": { "type": "string", "pattern": "^sha256:[0-9a-f]{64}$" },
                "content_type": {
                    "type": "string",
                    "enum": accept.iter().map(|t| t.mime()).collect::<Vec<_>>(),
                },
                "attachment_id": { "type": "string" },
            },
        }),
    };

    schema["title"] = json!(field.label);
    if let Some(help) = &field.help {
        schema["description"] = json!(help);
    }
    schema
}

#[cfg(test)]
mod tests {
    use crate::RequestType;

    fn t() -> RequestType {
        RequestType::from_toml(
            r#"
id = "letter"
version = 2
title = "Request a letter"

[[fields]]
name = "requester_email"
label = "Your email"
kind = "email"
required = true

[[fields]]
name = "deadline"
label = "Deadline"
kind = "date"
min = "2026-01-01"
required = true

[[fields]]
name = "programs"
label = "Programs"
kind = "multi_select"
max_choices = 5
options = [
  { value = "phd", label = "PhD" },
  { value = "masters", label = "Master's" },
]

[[fields]]
name = "why_me"
label = "Why me"
kind = "long_text"
max_length = 1500
required = true
show_when = { field = "programs", op = "present" }

[[acknowledgments]]
id = "consent"
label = "I understand the deadline is firm"
"#,
        )
        .unwrap()
    }

    #[test]
    fn the_schema_declares_every_field_and_forbids_the_undeclared() {
        let s = t().json_schema();
        let props = s["properties"].as_object().unwrap();
        assert_eq!(props.len(), 5, "four fields plus the acknowledgment");
        assert_eq!(s["additionalProperties"], serde_json::json!(false));
        assert_eq!(props["requester_email"]["format"], "email");
        assert_eq!(props["deadline"]["formatMinimum"], "2026-01-01");
        assert_eq!(props["why_me"]["maxLength"], 1500);
        assert_eq!(props["programs"]["maxItems"], 5);
        assert_eq!(props["programs"]["items"]["enum"][0], "phd");
        assert_eq!(props["consent"]["const"], serde_json::json!(true));
        assert_eq!(s["$id"], "mecha-factory:request/letter/v2");
    }

    /// The rule that keeps the schema from becoming a second, disagreeing copy
    /// of the visibility logic: it states only what is *always* true.
    #[test]
    fn a_conditionally_required_field_is_not_required_in_the_schema() {
        let s = t().json_schema();
        let required: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, ["requester_email", "deadline", "consent"]);
        assert!(
            !required.contains(&"why_me"),
            "why_me is required only when it is visible, which the validator owns"
        );
    }

    /// A schema that is a subset of the validator is safe; one that contradicts
    /// it is a rejection nobody can explain. Check the direction holds on a
    /// submission the validator accepts.
    #[test]
    fn everything_the_schema_requires_the_validator_also_requires() {
        let t = t();
        let schema = t.json_schema();
        let body = serde_json::json!({
            "requester_email": "a@b.edu",
            "deadline": "2026-06-01",
            "consent": true,
        });
        let accepted = t.validate(body.as_object().unwrap()).unwrap();
        for name in schema["required"].as_array().unwrap() {
            assert!(
                accepted.values.contains_key(name.as_str().unwrap()),
                "{name} is required by the schema and absent from a validated submission"
            );
        }
    }
}
