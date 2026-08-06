//! The `notebook` template: a marimo notebook as a `compute`-class bundle.
//!
//! **`marimo export html-wasm`, never islands.** The two are not
//! interchangeable and the difference is dependencies: the islands runtime
//! resolves packages through `loadPackagesFromImports` (an AST scan for
//! Pyodide-bundled packages) plus a `micropip.install` list **baked into the JS
//! bundle**, and nothing in it reads PEP 723. So a pure-Python package on PyPI
//! that Pyodide does not bundle silently fails to import, with no hook on the
//! host page to fix it. `export html-wasm` reads PEP 723. That settles it, and
//! it is also what `marimo-book`'s own source concludes after shipping the
//! islands path in production.
//!
//! This is the one template that **executes code we did not write** — the
//! export runs the notebook to capture its state — which is why §2.3 splits
//! rendering from publishing at all. Two consequences are load-bearing:
//!
//! - **The render is bounded by a timeout.** A hung notebook must not stall a
//!   scheduled run for as long as the run's own ceiling. Inherited from
//!   marimo-book, which pays for it: `MarimoIslandGenerator.build()` executes
//!   in-process and gets no timeout for free.
//! - **Confinement is this crate's job, and it is not done yet.** The design
//!   is explicit that a renderer executing arbitrary Python must not also hold
//!   the publish key or reach the network, and equally explicit that an
//!   unenforced claim is decoration. So it is *stated* rather than implied:
//!   today the export runs as you, exactly as `shell` does under mecha's
//!   default `[sandbox] kind = "none"`. See `confinement` in the crate README
//!   before wiring this to anything unattended.
//!
//! ### What the export actually produces, measured rather than assumed
//!
//! It is `marimo/_static/` copied wholesale — eleven files, byte-identical —
//! with `index.html` rewritten and `.nojekyll` added, plus a ~700-file `assets/`
//! tree. Three things follow, and each is handled here:
//!
//! - **Collateral gets published unless removed.** A 367-line `CLAUDE.md`
//!   telling an agent how to *edit* marimo notebooks rides along beside a page
//!   exported `--mode run` that nobody can edit. Nothing references it. The
//!   objection is not tidiness: it is instruction-shaped text on an origin we
//!   hand to correspondents.
//! - **The prune must follow references, not scan the entry point.** Measured:
//!   `logo.png` is unreferenced by `index.html` and used by two assets; the
//!   `android-chrome-*` icons are unreferenced by `index.html` and named in
//!   `manifest.json`. So the keep-list is *declared*, and anything not on it
//!   goes — a published bundle contains what we meant to publish.
//! - **`manifest.json` says "A Marimo App".** Installed to a home screen a
//!   published notebook would carry that name; it is rewritten to the bundle's
//!   own title.
//!
//! ### The check the general gate cannot make
//!
//! `assets/` is pinned as a reviewed third-party tree, so the vendoring gate
//! does not walk it — which means the gate cannot see that Pyodide is loaded
//! from `cdn.jsdelivr.net` by two minified workers inside it. A notebook that
//! passes the gate and cannot boot is precisely the failure the gate exists to
//! prevent, so [`check_runtime_vendored`] is a separate, specific check that
//! runs regardless.

use anyhow::{bail, Context, Result};
use mecha_manifest::ContentClass;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::render::Rendered;
use crate::vendor::Vendored;

/// Files the export leaves at the top level that a published bundle keeps.
///
/// A declared list rather than a reference scan, because a reference scan of
/// minified assets is exactly what gets this wrong: two of these are reachable
/// only from inside `assets/` or from `manifest.json`.
const KEEP: [&str; 9] = [
    "index.html",
    "manifest.json",
    "favicon.ico",
    "favicon-16x16.png",
    "favicon-32x32.png",
    "apple-touch-icon.png",
    "android-chrome-192x192.png",
    "android-chrome-512x512.png",
    // Unreferenced by index.html, referenced by two files in assets/.
    "logo.png",
];

/// Removed, with the reason, so nobody restores one by accident.
const DROP: [(&str, &str); 3] = [
    (
        "CLAUDE.md",
        "a prompt for agents editing marimo notebooks — instruction-shaped text \
         on an origin we hand to correspondents, beside a page nobody can edit",
    ),
    (".nojekyll", "a GitHub Pages marker, meaningless here"),
    (
        "site.webmanifest",
        "superseded by manifest.json, which is the one index.html links",
    ),
];

/// The two hosts a marimo export reaches at runtime, both from inside the
/// minified workers. Neither is reachable through configuration:
/// `packageBaseUrl` exists as a parameter and marimo assigns it the CDN literal
/// itself.
const RUNTIME_HOSTS: [&str; 2] = ["cdn.jsdelivr.net/pyodide", "wasm.marimo.app"];

pub struct NotebookOptions {
    /// The marimo executable. A venv's `bin/marimo`, or whatever is on `PATH`.
    pub marimo: PathBuf,
    /// Wall-clock ceiling on the export, which *executes the notebook*.
    pub timeout: Duration,
    pub title: Option<String>,
}

impl Default for NotebookOptions {
    fn default() -> Self {
        NotebookOptions {
            marimo: std::env::var("MECHA_FACTORY_MARIMO")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("marimo")),
            // Long enough for a real notebook that loads data, short enough
            // that a wedged one does not hold a scheduled run to its own
            // ceiling.
            timeout: Duration::from_secs(300),
            title: None,
        }
    }
}

/// A notebook bundle, plus the two things a caller cannot be expected to
/// work out for itself.
pub struct NotebookBundle {
    pub rendered: Rendered,
    /// The third-party tree to pin: `assets/` is ~700 files of vendored
    /// runtime, and scanning it line by line is a workflow nobody performs.
    pub vendored: Vec<Vendored>,
    /// What the prune removed, and why. Reported rather than silent, for the
    /// same reason `mecha work clean` names what it deleted: a sweep that
    /// prints nothing is one nobody trusts enough to leave running.
    pub removed: Vec<(String, &'static str)>,
}

/// Export, prune, and describe a notebook bundle.
pub fn notebook(source: &Path, out: &Path, options: &NotebookOptions) -> Result<NotebookBundle> {
    anyhow::ensure!(source.is_file(), "{} does not exist", source.display());
    // A stale directory would leave files from a previous export that this one
    // did not produce, and they would be published.
    if out.exists() {
        std::fs::remove_dir_all(out).with_context(|| format!("clearing {}", out.display()))?;
    }

    export(source, out, options)?;
    let removed = prune(out)?;

    let title = options.title.clone().unwrap_or_else(|| {
        source
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into()
    });
    rewrite_manifest(out, &title)?;

    check_runtime_vendored(out)?;

    let assets = out.join("assets");
    let mut vendored = Vec::new();
    if assets.is_dir() {
        vendored.push(Vendored {
            path: PathBuf::from("assets"),
            digest: crate::store::digest_tree(&assets)?,
            description: format!(
                "marimo export html-wasm runtime ({})",
                marimo_version(options)
            ),
        });
    }

    Ok(NotebookBundle {
        rendered: Rendered {
            dir: out.to_path_buf(),
            // Pyodide needs `wasm-unsafe-eval`, which is why this class exists
            // and why it is served from its own origin rather than beside the
            // reports.
            class: ContentClass::Compute,
            template: "notebook".into(),
            title,
            sources: vec![source
                .canonicalize()
                .unwrap_or_else(|_| source.to_path_buf())],
        },
        vendored,
        removed,
    })
}

fn export(source: &Path, out: &Path, options: &NotebookOptions) -> Result<()> {
    let mut child = std::process::Command::new(&options.marimo)
        .arg("export")
        .arg("html-wasm")
        .arg(source)
        .arg("-o")
        .arg(out)
        // Never `edit`: a published notebook is read-only, and `--mode edit`
        // would ship an editor for a file nobody can save.
        .args(["--mode", "run"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "running {} — install marimo, or point MECHA_FACTORY_MARIMO at it",
                options.marimo.display()
            )
        })?;

    // The export executes the notebook, so it is bounded. Polling rather than
    // blocking: there is no timeout on `wait`, and a hung notebook must not
    // hold a scheduled run for as long as the run's own ceiling.
    let started = Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_string(&mut stderr);
                }
                bail!(
                    "marimo export failed ({status}):\n{}",
                    stderr
                        .trim()
                        .lines()
                        .rev()
                        .take(20)
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            None if started.elapsed() > options.timeout => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "the export of {} did not finish in {:?} — it executes the \
                     notebook, so a cell that blocks blocks this",
                    source.display(),
                    options.timeout
                );
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Remove everything at the top level that is not on the keep-list, and say
/// what went.
///
/// Top level only: `assets/` is the vendored runtime and is kept whole.
fn prune(out: &Path) -> Result<Vec<(String, &'static str)>> {
    let mut removed = Vec::new();
    for entry in std::fs::read_dir(out)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if KEEP.contains(&name.as_str()) {
            continue;
        }
        std::fs::remove_file(entry.path()).with_context(|| format!("removing {name}"))?;
        // A named reason where we have one, so nobody restores a file by
        // accident; a generic one otherwise, since a new file the exporter
        // starts shipping is exactly the case the keep-list exists to catch.
        let why = DROP
            .iter()
            .find(|(dropped, _)| *dropped == name)
            .map(|(_, why)| *why)
            .unwrap_or(
                "not on the keep-list — a published bundle carries what we meant to publish",
            );
        removed.push((name, why));
    }
    removed.sort();
    Ok(removed)
}

/// `"A Marimo App"` is what a published notebook would otherwise be called on
/// a home screen.
fn rewrite_manifest(out: &Path, title: &str) -> Result<()> {
    let path = out.join("manifest.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let mut value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    value["name"] = serde_json::Value::String(title.to_string());
    // Short name has a real length budget on a home screen; the full title is
    // wrong there even when it is right in the manifest.
    let short: String = title.chars().take(30).collect();
    value["short_name"] = serde_json::Value::String(short);
    std::fs::write(&path, serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

/// Refuse a notebook whose Python runtime still comes from a CDN.
///
/// **The general gate cannot make this check**, and that is the whole reason it
/// exists separately: `assets/` is pinned as a reviewed third-party tree and is
/// therefore not walked, so the two minified workers that hardcode
/// `cdn.jsdelivr.net/pyodide/…` and `wasm.marimo.app/pyodide-lock.json` are
/// invisible to it. Under the compute origin's `script-src 'self'` /
/// `connect-src 'self'` the notebook does not boot — and a bundle that passes
/// the gate and cannot boot is exactly what the gate is for.
///
/// Fails rather than warns, for the same reason every other check here does.
pub fn check_runtime_vendored(bundle: &Path) -> Result<()> {
    let mut found: Vec<(PathBuf, &str)> = Vec::new();
    for path in js_files(&bundle.join("assets"))? {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for host in RUNTIME_HOSTS {
            if text.contains(host) {
                found.push((
                    path.strip_prefix(bundle).unwrap_or(&path).to_path_buf(),
                    host,
                ));
            }
        }
    }
    if found.is_empty() {
        return Ok(());
    }
    let mut message = String::from(
        "this notebook's Python runtime is still loaded from a CDN, so under the \
         compute origin's `script-src 'self'` / `connect-src 'self'` it will not \
         boot:\n",
    );
    for (file, host) in &found {
        message.push_str(&format!("  {}  → {host}\n", file.display()));
    }
    message.push_str(
        "\nmarimo has no setting for this: `packageBaseUrl` exists as a parameter \
         and marimo assigns it the CDN literal itself. Vendoring means shipping \
         the pinned Pyodide distribution into the bundle and substituting those \
         literals for same-origin paths.\n\
         The vendoring gate cannot catch this, because `assets/` is pinned as a \
         reviewed tree and is not walked — which is why this check is separate.",
    );
    bail!(message)
}

fn js_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            out.extend(js_files(&path)?);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("js") | Some("mjs")
        ) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn marimo_version(options: &NotebookOptions) -> String {
    std::process::Command::new(&options.marimo)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown version".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// marimo is a heavyweight optional dependency, so its tests skip when it
    /// is absent — and `MECHA_TEST_REQUIRE_BACKENDS=1` turns every skip into a
    /// failure, because in CI a silently skipped test reads exactly like a
    /// passing one.
    fn marimo_or_skip() -> Option<NotebookOptions> {
        let options = NotebookOptions::default();
        let available = std::process::Command::new(&options.marimo)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if available {
            return Some(options);
        }
        if std::env::var("MECHA_TEST_REQUIRE_BACKENDS").as_deref() == Ok("1") {
            panic!(
                "marimo is not available at {} and MECHA_TEST_REQUIRE_BACKENDS=1",
                options.marimo.display()
            );
        }
        eprintln!("skipping: marimo is not installed (set MECHA_FACTORY_MARIMO)");
        None
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "factory-notebook-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const NB: &str = r##"import marimo

app = marimo.App()


@app.cell
def _():
    import marimo as mo
    return (mo,)


@app.cell
def _(mo):
    mo.md("# A probe\n\nHello.")
    return


if __name__ == "__main__":
    app.run()
"##;

    /// The prune list, checked against a real export rather than a fixture —
    /// the whole point is that the exporter's output is what it is.
    #[test]
    fn a_real_export_is_pruned_to_the_declared_list() {
        let Some(options) = marimo_or_skip() else {
            return;
        };
        let s = Scratch::new("prune");
        let source = s.0.join("nb.py");
        std::fs::write(&source, NB).unwrap();

        // The runtime is not vendored yet, so the whole call refuses — which is
        // itself the behaviour under test further down. Drive the steps.
        let out = s.0.join("out");
        export(&source, &out, &options).unwrap();
        assert!(out.join("CLAUDE.md").is_file(), "the export ships it");
        let removed = prune(&out).unwrap();
        assert!(
            removed
                .iter()
                .any(|(name, why)| name == "CLAUDE.md" && why.contains("instruction-shaped")),
            "the prune says what it removed and why: {removed:?}"
        );

        for (name, _why) in DROP {
            assert!(
                !out.join(name).exists(),
                "{name} survived the prune, and it is published if it does"
            );
        }
        for name in KEEP {
            assert!(
                out.join(name).is_file(),
                "{name} was pruned and should not be"
            );
        }
        assert!(
            out.join("assets").is_dir(),
            "the runtime tree is kept whole"
        );
    }

    #[test]
    fn the_manifest_stops_calling_it_a_marimo_app() {
        let Some(options) = marimo_or_skip() else {
            return;
        };
        let s = Scratch::new("manifest");
        let source = s.0.join("nb.py");
        std::fs::write(&source, NB).unwrap();
        let out = s.0.join("out");
        export(&source, &out, &options).unwrap();

        let before: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(before["name"], "A Marimo App");

        rewrite_manifest(&out, "Weekly figures").unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(after["name"], "Weekly figures");
        assert_eq!(after["short_name"], "Weekly figures");
        // The icons the manifest names must still be there, or the prune broke
        // something no reference scan of index.html would have noticed.
        for icon in after["icons"].as_array().unwrap() {
            let src = icon["src"].as_str().unwrap();
            assert!(
                KEEP.contains(&src),
                "manifest.json names {src}, which the prune would remove"
            );
        }
    }

    /// The check the general gate structurally cannot make.
    #[test]
    fn a_notebook_whose_runtime_is_not_vendored_is_refused() {
        let s = Scratch::new("runtime");
        std::fs::create_dir_all(s.0.join("assets")).unwrap();
        std::fs::write(
            s.0.join("assets/worker-abc.js"),
            "let n=`https://cdn.jsdelivr.net/pyodide/${v}/full/`;",
        )
        .unwrap();

        let err = check_runtime_vendored(&s.0).unwrap_err().to_string();
        assert!(err.contains("will not boot"), "{err}");
        assert!(err.contains("assets/worker-abc.js"), "{err}");
        assert!(
            err.contains("pinned as a reviewed tree"),
            "it says why the gate missed it: {err}"
        );

        // And a vendored one passes.
        std::fs::write(s.0.join("assets/worker-abc.js"), "let n=`./pyodide/`;").unwrap();
        check_runtime_vendored(&s.0).unwrap();
    }

    /// Measured on the real export: this is the whole reason the keep-list is
    /// declared rather than derived from `index.html`.
    #[test]
    fn the_keep_list_holds_files_no_scan_of_index_html_would_find() {
        // logo.png is referenced only from inside assets/, and the
        // android-chrome icons only from manifest.json.
        for name in [
            "logo.png",
            "android-chrome-192x192.png",
            "android-chrome-512x512.png",
        ] {
            assert!(KEEP.contains(&name));
        }
    }
}
