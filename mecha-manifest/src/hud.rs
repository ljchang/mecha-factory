//! A live dashboard's data, as home pushes it and the box keeps it
//! (`mecha/docs/LIVE-DASHBOARD-DESIGN.md` §6.2).
//!
//! A dashboard bundle (the `hud` template) carries its spec and renderer in an
//! immutable version, plus `datasets.json`: what each dataset's columns are and
//! which loader the owner released. The **rows** are not in the version — a
//! version is content-addressed and never changes — so home pushes them
//! beside it, one dataset at a time, and the page fetches them on load.
//!
//! Shared by both ends so they cannot disagree about the wire:
//! - [`DatasetsManifest`]: `datasets.json`, written by the template, read by
//!   the box to check every push against what was released.
//! - [`DatasetPush`]: the body of `PUT /v1/bundles/{id}/datasets/{name}`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The file a dashboard version declares its datasets in.
pub const DATASETS_FILE: &str = "datasets.json";

/// One column, as the loader declared it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetColumn {
    pub name: String,
    /// `string`, `integer`, `float`, `boolean`, `date` or `timestamp`.
    #[serde(rename = "type")]
    pub kind: String,
}

/// What a released dashboard may be fed: per dataset, its columns and the
/// digest of the loader the owner released. A push that differs in either is
/// refused — what runs unattended is exactly what a human released.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetsManifest {
    pub datasets: BTreeMap<String, DeclaredDataset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredDataset {
    pub columns: Vec<DatasetColumn>,
    pub loader_digest: String,
}

/// One dataset push.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetPush {
    /// The version the alias points at — the release this push was made
    /// under. A push naming any other is stale.
    pub release: u32,
    /// Orders pushes within one release; home numbers them monotonically.
    pub generation: u64,
    /// When home's query ran, RFC 3339 — the freshness the page shows.
    pub generated_at: String,
    pub loader_digest: String,
    pub columns: Vec<DatasetColumn>,
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Whether the rows carry text someone other than the owner wrote.
    #[serde(default)]
    pub external: bool,
}

/// A dataset name: `[a-z][a-z0-9_]{0,63}` — checked before it is joined to
/// anything, on both ends.
pub fn valid_dataset_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dataset_name_is_a_lowercase_identifier() {
        for ok in ["now", "visits_by_day", "a1", &"a".repeat(64)] {
            assert!(valid_dataset_name(ok), "{ok}");
        }
        for bad in ["", "1a", "Visits", "a-b", "../x", "a.json", &"a".repeat(65)] {
            assert!(!valid_dataset_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_push_refuses_a_field_it_does_not_know() {
        let ok = r#"{"release":1,"generation":1,"generated_at":"2031-04-17T09:00:00Z",
            "loader_digest":"d","columns":[{"name":"a","type":"integer"}],"rows":[[1]]}"#;
        assert!(serde_json::from_str::<DatasetPush>(ok).is_ok());
        let extra = ok.replace(r#""rows""#, r#""url":"x","rows""#);
        assert!(serde_json::from_str::<DatasetPush>(&extra).is_err());
    }
}
