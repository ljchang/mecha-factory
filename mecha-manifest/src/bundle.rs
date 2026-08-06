//! The outbound half of the contract: what a published bundle declares.
//!
//! The manifest that travels with a rendered directory, and the same file
//! `mecha work clean` reads to find out what it must never remove.
//!
//! Two rules here, and both are enforced by the publisher rather than by this
//! type — a manifest describes, it does not police:
//!
//! - **The template declares the content class; the publisher enforces the
//!   policy.** A `static` bundle that emitted a `<script>` must *fail* the
//!   publish, not be silently upgraded to `interactive` — the class decides the
//!   CSP and therefore the origin, and a class that adjusts itself to whatever
//!   was emitted is not a policy.
//! - **`vendor = true` means the publish fails on a surviving external
//!   reference.** Not warns. This is the one enforcement the whole artifact
//!   security model rests on, and a warning is how it silently stops holding.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::request::valid_id;
use crate::{ManifestError, Result};

/// What a bundle contains, which decides its CSP and therefore which origin it
/// may be served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentClass {
    /// Prose and computed figures. Nothing executes.
    #[default]
    Static,
    /// A view over data the reader manipulates: filters, linked charts. Scripts,
    /// but no `eval` and no WebAssembly.
    Interactive,
    /// A notebook. Pyodide needs `wasm-unsafe-eval`, which is why this class
    /// gets its own origin instead of weakening every static report to
    /// accommodate it.
    Compute,
}

impl ContentClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContentClass::Static => "static",
            ContentClass::Interactive => "interactive",
            ContentClass::Compute => "compute",
        }
    }

    /// Whether a bundle of this class may contain anything that executes.
    /// The publisher's gate consults this; the class never adjusts to the
    /// answer.
    pub fn allows_scripts(&self) -> bool {
        !matches!(self, ContentClass::Static)
    }
}

/// Who may read a published bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// A capability URL is required. The default, because the failure mode of
    /// getting this wrong points one way.
    #[default]
    Private,
    /// Anyone with the link, no capability check.
    Public,
}

/// The manifest written into every mirrored bundle version.
///
/// `sources` is a contract with mecha rather than with the server:
/// `mecha work clean` reads it to find the generated files a published bundle
/// depends on, and never removes them — "regenerate last week's report" must
/// not silently lose its input. **If the publisher does not write `sources`,
/// that protection covers nothing**, which is worth knowing before assuming a
/// source is safe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    /// Stable across versions. The share URL is `/b/<id>/`.
    pub id: String,
    /// Content-addressed and immutable. Re-publishing identical bytes returns
    /// the existing version rather than minting a new one, which makes "did
    /// anything actually change?" a comparison rather than a guess.
    pub version: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The template that rendered it.
    pub template: String,
    pub class: ContentClass,
    #[serde(default)]
    pub visibility: Visibility,
    /// The digest the version is addressed by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// What this was rendered from, as absolute paths. Read by
    /// `mecha work clean`; see the type docs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<PathBuf>,
}

impl BundleManifest {
    pub fn from_json(text: &str) -> Result<Self> {
        let parsed: BundleManifest = serde_json::from_str(text)
            .map_err(|e| ManifestError::invalid(format!("parsing bundle.json: {e}")))?;
        parsed.check()?;
        Ok(parsed)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a BundleManifest is always serialisable")
    }

    pub fn check(&self) -> Result<()> {
        valid_id(&self.id, "bundle id")?;
        valid_id(&self.template, "template id")?;
        if self.version == 0 {
            return Err(ManifestError::invalid(
                "bundle versions start at 1; 0 reads as unversioned",
            ));
        }
        // Absolute, because `clean` compares them against canonicalized paths
        // and a relative path would silently match nothing — the shape of
        // failure where a protection appears to be in place and is not.
        for source in &self.sources {
            if !source.is_absolute() {
                return Err(ManifestError::invalid(format!(
                    "source `{}` is relative; retention compares canonical paths, \
                     so a relative one would protect nothing",
                    source.display()
                )));
            }
        }
        Ok(())
    }

    /// The path a reviewer opens. `mecha outbox show` on a staged publish looks
    /// for exactly these names, in this order.
    pub fn entry_point_candidates() -> &'static [&'static str] {
        &["index.html", "index.md", "README.md"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> BundleManifest {
        BundleManifest {
            id: "morning-brief".into(),
            version: 3,
            title: "Morning briefing".into(),
            description: None,
            template: "report".into(),
            class: ContentClass::Static,
            visibility: Visibility::Private,
            digest: Some("sha256:abc".into()),
            published_at: Some("2026-08-06T11:00:00Z".into()),
            sources: vec![PathBuf::from("/home/x/.mecha/work/morning/2026-08-06.md")],
        }
    }

    #[test]
    fn a_bundle_manifest_round_trips_and_defaults_to_private() {
        let json = manifest().to_json();
        let again = BundleManifest::from_json(&json).unwrap();
        assert_eq!(again.version, 3);
        assert_eq!(again.sources.len(), 1);

        // The two fields whose default matters, omitted entirely.
        let minimal = BundleManifest::from_json(
            r#"{"id":"b","version":1,"title":"t","template":"report","class":"static"}"#,
        )
        .unwrap();
        assert_eq!(minimal.visibility, Visibility::Private);
        assert!(minimal.sources.is_empty());
    }

    /// A relative source would silently match nothing in `clean`, which is the
    /// shape of failure where a protection looks present and is not.
    #[test]
    fn a_relative_source_is_refused() {
        let mut m = manifest();
        m.sources = vec![PathBuf::from("2026-08-06.md")];
        let err = m.check().unwrap_err().to_string();
        assert!(err.contains("relative"), "{err}");
    }

    #[test]
    fn a_static_bundle_is_the_one_class_that_may_not_execute() {
        assert!(!ContentClass::Static.allows_scripts());
        assert!(ContentClass::Interactive.allows_scripts());
        assert!(ContentClass::Compute.allows_scripts());
        assert_eq!(ContentClass::default(), ContentClass::Static);
    }

    #[test]
    fn version_zero_is_refused_because_it_reads_as_unversioned() {
        let mut m = manifest();
        m.version = 0;
        assert!(m.check().is_err());
    }
}
