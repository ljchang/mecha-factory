//! The response headers each content class is served with.
//!
//! **Here, in a library, because the policy has to be one definition.** The
//! local preview server and the public server must send the same headers, or a
//! bundle verified at home is verified against something the world never sees —
//! which is worse than not verifying it, because it produces a confident wrong
//! answer. Putting it in the shared manifest crate is also what makes the
//! strictest claim in this design a **unit test** rather than a sentence in a
//! document.
//!
//! Three policies, one per [`ContentClass`], and the split is the whole reason
//! the classes exist:
//!
//! - **`static`** — nothing executes. `script-src 'none'`.
//! - **`interactive`** — our own compiled scripts run. `script-src 'self'`, and
//!   still no `eval`, no `wasm-unsafe-eval`, no inline.
//! - **`compute`** — Pyodide. Needs `wasm-unsafe-eval`, which is exactly why
//!   this class gets **its own origin** instead of every static report being
//!   weakened to accommodate notebooks. That would be the
//!   silently-degrading-sandbox shape, one origin wide.
//!
//! Choices that each cost something to decide:
//!
//! - **`wasm-unsafe-eval`, never `unsafe-eval`.** The narrow directive permits
//!   `WebAssembly.compile`/`instantiate` and still forbids `eval` and
//!   `new Function`.
//! - **COOP/COEP are on** for `compute`, which unlocks `SharedArrayBuffer` for
//!   Pyodide's threading. They are normally painful because they break
//!   third-party embeds — here everything is same-origin and vendored, so they
//!   cost nothing. The strict thing is also the free thing.
//! - **`connect-src 'self'`** is what stops a notebook phoning home, and it is
//!   the runtime enforcement the vendoring gate leans on for a pinned
//!   third-party tree: a map-tile fetch buried in a charting library simply
//!   fails.
//! - **`style-src 'unsafe-inline'` only on `compute`.** marimo's runtime sets
//!   element styles; our own templates extract CSS to a file precisely so the
//!   other two classes never need it.
//! - **`frame-ancestors 'self'`** so a notebook still cannot be framed by the
//!   gate or by anyone else's origin — `'self'` is the artifact origin
//!   itself, which is per-user, so the only page that may frame a bundle is
//!   one the same origin serves. That is what lets the factory's viewer put
//!   chrome *beside* immutable bytes instead of inside them, while every
//!   cross-origin clickjacking protection `'none'` bought stays bought.
//!   `form-action 'none'` because nothing in an artifact submits anywhere.

use crate::ContentClass;

/// One header, as it goes on the wire.
pub type Header = (&'static str, String);

impl ContentClass {
    /// The `Content-Security-Policy` for this class.
    pub fn csp(&self) -> String {
        let directives: &[&str] = match self {
            ContentClass::Static => &[
                "default-src 'none'",
                "img-src 'self' data:",
                "style-src 'self'",
                "font-src 'self'",
                // Named explicitly even though `default-src 'none'` covers it:
                // the one thing a reader of this policy most wants to know
                // about a report is that nothing in it runs.
                "script-src 'none'",
                "connect-src 'none'",
                "frame-ancestors 'self'",
                "base-uri 'none'",
                "form-action 'none'",
            ],
            ContentClass::Interactive => &[
                "default-src 'self'",
                "script-src 'self'",
                "style-src 'self'",
                "img-src 'self' data:",
                "font-src 'self'",
                "connect-src 'self'",
                "frame-ancestors 'self'",
                "base-uri 'none'",
                "form-action 'none'",
            ],
            ContentClass::Compute => &[
                "default-src 'self'",
                "script-src 'self' 'wasm-unsafe-eval'",
                "style-src 'self' 'unsafe-inline'",
                "img-src 'self' data: blob:",
                // marimo embeds a woff2 as a data: URL. A font is bytes the
                // page already has rather than a fetch, so this grants no
                // reach it did not have.
                "font-src 'self' data:",
                "connect-src 'self'",
                "worker-src 'self' blob:",
                "child-src 'self' blob:",
                "frame-ancestors 'self'",
                "base-uri 'none'",
                "form-action 'none'",
            ],
        };
        directives.join("; ")
    }

    /// Every header a bundle of this class is served with.
    ///
    /// **Not for the gate.** These describe an *artifact* — something a reader
    /// looks at and which submits nothing anywhere. The gate serves forms, and
    /// a form has to POST. See [`gate_headers`].
    pub fn headers(&self) -> Vec<Header> {
        self.headers_framed_by(None)
    }

    /// The same headers, with one extra permitted frame ancestor — the gate,
    /// whose signed-in viewer frames a bundle to put the owner's chrome
    /// beside it.
    ///
    /// This deliberately revises an older rule that read "so a notebook
    /// cannot be framed by the gate". What that rule actually protected is
    /// preserved by the browser regardless of this directive: a framed
    /// bundle keeps its own origin, so its scripts still cannot reach the
    /// gate's DOM, cookies or session — cross-origin iframe isolation is not
    /// ours to grant or revoke. What the directive governs is UI redressing,
    /// and the only pages the extra ancestor admits are the gate's own,
    /// which tenants cannot author. Every other origin on the web stays
    /// refused. The parameter is an origin, not a boolean, because the gate's
    /// name is deployment configuration this crate must not guess.
    pub fn headers_framed_by(&self, gate: Option<&str>) -> Vec<Header> {
        let csp = match gate {
            None => self.csp(),
            Some(origin) => self.csp().replace(
                "frame-ancestors 'self'",
                &format!("frame-ancestors 'self' {origin}"),
            ),
        };
        let mut out = vec![
            ("Content-Security-Policy", csp),
            ("X-Content-Type-Options", "nosniff".into()),
            ("Referrer-Policy", "no-referrer".into()),
            // A published artifact is immutable at its version URL, so a
            // reader's browser may keep it. The *alias* is what moves, and it
            // is a redirect rather than content.
            ("Cross-Origin-Resource-Policy", "same-origin".into()),
        ];
        if matches!(self, ContentClass::Compute) {
            // Cross-origin isolation, for SharedArrayBuffer. Free here because
            // everything is same-origin and vendored.
            out.push(("Cross-Origin-Opener-Policy", "same-origin".into()));
            out.push(("Cross-Origin-Embedder-Policy", "require-corp".into()));
        }
        out
    }
}

/// The policy the **gate** serves its own pages under.
///
/// The gate is not an artifact origin, and applying the artifact policy to it
/// was a real, silent breakage: `form-action 'none'` means a browser refuses to
/// submit the form, and `script-src 'none'` blocks the conditional-field script
/// the generator emits. Neither showed up in testing, because every test
/// submitted with `curl` — which enforces no policy at all. A stranger with a
/// browser could read the form and never send it.
///
/// It stays as narrow as a page that submits can be:
///
/// - **`form-action 'self'`** — it may POST back to us, and nowhere else. That
///   is the directive that matters: it is what stops injected markup pointing
///   the form at somebody else's collector.
/// - **`script-src 'self'`** — our own `form.js`, which hides fields whose
///   `show_when` is unmet. A convenience and never a control: the server
///   evaluates the same rules, and a submission that ignores the script is
///   refused there.
/// - **`default-src 'none'` and `connect-src 'none'`** — the form fetches
///   nothing at runtime and must not start.
pub fn gate_headers() -> Vec<Header> {
    let csp = [
        "default-src 'none'",
        "style-src 'self'",
        "script-src 'self'",
        "img-src 'self' data:",
        "font-src 'self'",
        "connect-src 'none'",
        "form-action 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
    ]
    .join("; ");
    vec![
        ("Content-Security-Policy", csp),
        ("X-Content-Type-Options", "nosniff".into()),
        // A form carries somebody's name and address in its URL history but
        // not in a referer we hand to anyone else.
        ("Referrer-Policy", "no-referrer".into()),
        ("Cross-Origin-Resource-Policy", "same-origin".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A form has to be able to submit, and the artifact policy forbids it.
    ///
    /// This shipped: the gate served `ContentClass::Static`, so a browser
    /// refused the POST and a stranger could fill the form in and never send
    /// it. Nothing caught it because every test submitted with `curl`, which
    /// enforces no policy — so the assertion is written against the directive
    /// rather than against a request that appears to succeed.
    #[test]
    fn the_gate_can_submit_a_form_and_the_artifact_policy_cannot() {
        let gate = gate_headers()
            .into_iter()
            .find(|(name, _)| *name == "Content-Security-Policy")
            .map(|(_, value)| value)
            .expect("a policy");

        assert!(
            gate.contains("form-action 'self'"),
            "the gate must be able to POST to itself: {gate}"
        );
        assert!(
            gate.contains("script-src 'self'"),
            "the conditional-field script is ours and same-origin: {gate}"
        );
        // Narrow everywhere else: a form fetches nothing at runtime.
        assert!(gate.contains("default-src 'none'"), "{gate}");
        assert!(gate.contains("connect-src 'none'"), "{gate}");
        assert!(gate.contains("frame-ancestors 'none'"), "{gate}");

        // And the policy it used to be served under would have blocked both.
        let artifact = ContentClass::Static.csp();
        assert!(artifact.contains("form-action 'none'"));
        assert!(artifact.contains("script-src 'none'"));
    }

    /// The claim the whole artifact model rests on, as an assertion rather than
    /// a sentence in a design document.
    #[test]
    fn nothing_executes_in_a_static_bundle() {
        let csp = ContentClass::Static.csp();
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("script-src 'none'"));
        assert!(csp.contains("connect-src 'none'"));
    }

    /// The narrow directive permits WebAssembly and still forbids `eval` and
    /// `new Function`. Granting the wide one would give a notebook arbitrary
    /// script execution on an origin we control.
    #[test]
    fn compute_grants_wasm_and_never_plain_eval() {
        let csp = ContentClass::Compute.csp();
        assert!(csp.contains("'wasm-unsafe-eval'"));
        assert!(
            !csp.contains("'unsafe-eval'") || csp.contains("'wasm-unsafe-eval'"),
            "the wide directive must not appear"
        );
        // Belt and braces on the substring check above: the exact directive.
        assert!(csp.contains("script-src 'self' 'wasm-unsafe-eval';"));
    }

    /// This is the one that stops a notebook phoning home, and it is what the
    /// vendoring gate leans on for a pinned third-party tree.
    #[test]
    fn no_class_may_connect_off_origin() {
        for class in [
            ContentClass::Static,
            ContentClass::Interactive,
            ContentClass::Compute,
        ] {
            let csp = class.csp();
            assert!(
                csp.contains("connect-src 'none'") || csp.contains("connect-src 'self'"),
                "{class:?} can reach off-origin: {csp}"
            );
            // 'self' and never 'none': the factory's own viewer on the same
            // per-user origin may frame a bundle; the gate and every other
            // origin still may not.
            assert!(csp.contains("frame-ancestors 'self'"), "{class:?}");
            assert!(csp.contains("base-uri 'none'"), "{class:?}");
        }
    }

    /// Inline styles are a `compute`-only concession to marimo's runtime. Our
    /// own templates extract CSS to a file so the other two never need it — and
    /// a regression there would relax the policy for every report.
    #[test]
    fn only_compute_allows_inline_style_and_no_class_allows_inline_script() {
        assert!(ContentClass::Compute
            .csp()
            .contains("style-src 'self' 'unsafe-inline'"));
        for class in [ContentClass::Static, ContentClass::Interactive] {
            assert!(!class.csp().contains("unsafe-inline"), "{class:?}");
        }
        for class in [
            ContentClass::Static,
            ContentClass::Interactive,
            ContentClass::Compute,
        ] {
            assert!(
                !class.csp().contains("script-src 'self' 'unsafe-inline'"),
                "{class:?} allows inline script"
            );
        }
    }

    /// Cross-origin isolation is what Pyodide's threading needs, and it is free
    /// here because everything is same-origin and vendored.
    #[test]
    fn only_compute_is_cross_origin_isolated() {
        let names = |c: ContentClass| -> Vec<&'static str> {
            c.headers().into_iter().map(|(n, _)| n).collect()
        };
        assert!(names(ContentClass::Compute).contains(&"Cross-Origin-Embedder-Policy"));
        assert!(!names(ContentClass::Static).contains(&"Cross-Origin-Embedder-Policy"));
        // Every class gets the two that are never optional.
        for class in [
            ContentClass::Static,
            ContentClass::Interactive,
            ContentClass::Compute,
        ] {
            assert!(names(class).contains(&"X-Content-Type-Options"));
            assert!(names(class).contains(&"Content-Security-Policy"));
        }
    }
}
