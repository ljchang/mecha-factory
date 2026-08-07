//! The contract between mecha and `mecha-factory`, and it is **data**.
//!
//! One request type, written once as TOML, emits everything the boundary needs:
//! a JSON Schema, an HTML form, and the validation both ends run. That is what
//! makes "a human with a browser, an agent with a browser, an agent with MCP,
//! and an agent doing discovery all arrive at the same typed object" a property
//! of one file rather than of four implementations agreeing.
//!
//! ```text
//!                        ┌──▶ JSON Schema   (schema.rs)  — the wire contract
//!   types/meeting.toml ──┼──▶ HTML form     (form.rs)    — the default rendering
//!                        ├──▶ validation    (validate.rs)— run at BOTH ends
//!                        └──▶ tool + skill declarations   — later, same source
//! ```
//!
//! Five rules carry the design, and each is a bug if undone.
//!
//! **1. Nothing here may depend on `mecha-core`.** The shared contract is TOML
//! plus a generated JSON Schema, not a struct two crates happen to agree on.
//! That is a feature: it forces the contract to be inspectable, and it is what
//! lets validation happen independently at the edge and at home.
//!
//! **2. Declarative conditions only.** The form system this borrows from lets a
//! step's `showWhen` be either a declarative condition or an arbitrary
//! TypeScript closure. A closure cannot cross to a Rust server, and **the server
//! must evaluate exactly the rules the browser did** — so [`Condition`] takes
//! `field`/`operator`/`value` and there is no function form. One arm of a
//! two-arm union, kept deliberately.
//!
//! **3. Validation is one function, run twice.** [`RequestType::validate`] is
//! what the factory runs at the edge *and* what mecha runs on every drained
//! record before it enters a conversation. Not belt-and-braces: it is the whole
//! containment story for a compromised public box. A hostile server cannot
//! invent a field, change a request's type, or exceed a cap — because the same
//! code that accepted the submission re-checks it at home. What it *can* do is
//! put hostile prose in a field already known to be free text, which is exactly
//! what the quarantine layer is for.
//!
//! **4. Free text is not a declaration.** [`Field::is_free_text`] is derived
//! from the field's kind, never read from the manifest. A `select` of our own
//! enum values cannot carry a stranger's prose; a `text` field always can. A
//! knob that let someone mark a text field trusted is precisely the switch that
//! must not exist — same reasoning as the learning system's provenance gate
//! having no override.
//!
//! **5. An uncapped text field is a manifest error.** These forms sit on an
//! unauthenticated endpoint. A `text` or `textarea` with no `max_length` is an
//! unbounded write, so [`RequestType::check`] refuses it at load rather than
//! discovering it under load.
//!
//! And one consequence of rule 2 worth stating on its own, because it is the
//! rule most likely to be got wrong by an implementation that validates fields
//! independently: **a field is required only when it is visible.** A required
//! field inside a step whose `show_when` is false must not be demanded — the
//! browser did not show it, so the server cannot insist on it. See
//! [`RequestType::visible_fields`].

pub mod availability;
mod booking;
pub mod brand;
mod bundle;
mod condition;
mod csp;
mod digest;
mod form;
mod media;
mod request;
mod schema;
pub mod theme;
mod validate;

pub use booking::{booking_assets, week_of, BookingOptions, BookingPage};
pub use brand::{FAVICON_DATA_URI, FAVICON_LINK, LOGO_MONO_SVG};
pub use bundle::{BundleManifest, ContentClass, Visibility};
pub use condition::{Condition, Operator};
pub use csp::{gate_headers, Header};
pub use digest::{digest_files, MANIFEST_FILE};
pub use form::{site_header, FormOptions, FormPage};
pub use media::{content_type, sniff, FileType};
pub use request::{
    reminder_minutes, valid_id, Acknowledgment, BookingPolicy, Field, FieldKind, RequestKind,
    RequestType, Step, MAX_FILE_BYTES_PER_FIELD, MAX_FILE_BYTES_PER_TYPE,
};
pub use theme::{Palette, Theme, BUILT_IN as BUILT_IN_THEMES};
pub use validate::{FileMeta, Phase, Submission, ValidationError};

/// Anything wrong with a manifest itself, as opposed to with a submission
/// against it. Separate types on purpose: a bad manifest is our mistake and
/// stops the program, a bad submission is a stranger's and gets a form back
/// with the errors on it.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("{0}")]
    Invalid(String),
    #[error("parsing TOML: {0}")]
    Toml(#[from] toml::de::Error),
}

impl ManifestError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        ManifestError::Invalid(message.into())
    }
}

pub type Result<T> = std::result::Result<T, ManifestError>;

/// Escape text for interpolation into HTML, including attribute values.
///
/// Public because the server interpolates the same class of text — a
/// stranger's own answers, handed back to them on a confirmation page — and a
/// second escaper would be a second thing to get wrong.
///
/// Hand-rolled and applied at **every** interpolation site rather than at the
/// ones that look risky. A manifest is ours, so this is not the injection
/// boundary that matters — but a form's `value` attribute is re-rendered from a
/// stranger's rejected submission when errors are shown, and that one is.
pub fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_both_quote_styles_and_the_ampersand_first() {
        assert_eq!(
            escape_text(r#"<a href="x" onclick='y'>&"#),
            "&lt;a href=&quot;x&quot; onclick=&#39;y&#39;&gt;&amp;"
        );
        // The ampersand must not be double-escaped by a later replacement,
        // which is what a naive sequence of `replace` calls gets wrong.
        assert_eq!(escape_text("&lt;"), "&amp;lt;");
    }
}
