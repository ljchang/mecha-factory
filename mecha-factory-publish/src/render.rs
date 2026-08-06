//! Rendering: a template plus a source, in, a directory out.
//!
//! **Render is a separate step from publish, and the split is two arguments
//! landing on one line.** The trust one: a `notebook` renderer *executes* the
//! notebook, so it must not be the process holding the publish key. The
//! ergonomic one: rendering is cheap and publishing is expensive, because
//! publishing costs a human review — without a render step, every iteration
//! (render, look, fix a chart, render again) would be a staged outbox item
//! somebody has to reject. That the security split and the workflow split land
//! on the same line is a good sign the line is real.
//!
//! Four of the six planned templates render **in Rust with nothing to execute**
//! — a report, a dashboard, a booking page and a form need no Python at all.
//! Only the marimo ones run code, and only they need the sandbox. `report` is
//! here; the rest follow.
//!
//! **`class = "static"` means nothing executes, and the publisher enforces
//! that rather than adjusting to it.** Declaring `static` and emitting a
//! `<script>` must fail the publish, not silently upgrade the class — the class
//! decides the CSP and therefore which origin may serve it, and a policy that
//! rewrites itself to match what happened is not a policy. The full vendoring
//! gate (every external reference, named with its file and line) is the next
//! build step; what is enforced here is the narrower rule that a `static`
//! bundle contains no script.

use anyhow::{bail, Context, Result};
use mecha_manifest::ContentClass;
use std::path::{Path, PathBuf};

/// One rendered bundle, on disk, not yet published.
#[derive(Debug)]
pub struct Rendered {
    pub dir: PathBuf,
    pub class: ContentClass,
    pub template: String,
    pub title: String,
    /// What it was rendered from. Travels into `bundle.json` as `sources`, and
    /// is what stops `mecha work clean` removing the input of a published
    /// report.
    pub sources: Vec<PathBuf>,
}

/// Render a markdown file as a `report`.
///
/// The output is one directory: `index.html`, `report.css`, and a copy of the
/// markdown as `source.md` so a later run can read back what produced the page
/// rather than trying to reverse it out of HTML.
pub fn report(source: &Path, out: &Path, title: Option<&str>) -> Result<Rendered> {
    let markdown =
        std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;

    // A title from the first `# heading` when one was not given, because a
    // report whose tab says "index" is a report nobody can find again in a
    // browser with nine tabs open.
    let title = title
        .map(str::to_string)
        .or_else(|| first_heading(&markdown))
        .unwrap_or_else(|| {
            source
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Report".into())
        });

    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    options.insert(pulldown_cmark::Options::ENABLE_FOOTNOTES);
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    options.insert(pulldown_cmark::Options::ENABLE_TASKLISTS);
    // Deliberately **not** ENABLE_SMART_PUNCTUATION: a briefing quoting a
    // command or a path must not have its quotes and hyphens rewritten.

    let parser = pulldown_cmark::Parser::new_ext(&markdown, options);
    let mut body = String::new();
    pulldown_cmark::html::push_html(&mut body, parser);

    // pulldown-cmark passes raw HTML through by default. A briefing is written
    // by a model from mail bodies and web pages, so its markdown is not
    // trusted input — an `<img src=x onerror=…>` in a subject line would
    // otherwise become script in a page the user opens.
    //
    // This is why the report template renders in-process with nothing to
    // execute *and* still refuses script: "nothing runs it" is a property of
    // the renderer, not of the bytes it produced.
    // `body` is HTML by construction and goes in raw; `title` is a string
    // lifted out of the markdown — from a heading a model wrote out of mail
    // bodies — so it is escaped here. minijinja does not auto-escape an unnamed
    // template, which is what we want for the body and exactly wrong for the
    // title, so the asymmetry is made explicit rather than left to a default.
    let env = minijinja::Environment::new();
    let html = env
        .render_str(
            PAGE,
            minijinja::context! { title => escape(&title), body => &body },
        )
        .context("rendering the report page")?;

    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    std::fs::write(out.join("index.html"), &html)?;
    std::fs::write(out.join("report.css"), REPORT_CSS)?;
    std::fs::write(out.join("source.md"), &markdown)?;

    let rendered = Rendered {
        dir: out.to_path_buf(),
        class: ContentClass::Static,
        template: "report".into(),
        title,
        sources: vec![source
            .canonicalize()
            .unwrap_or_else(|_| source.to_path_buf())],
    };
    check_class(&rendered)?;
    Ok(rendered)
}

/// Refuse a bundle that emitted more than its declared class allows.
///
/// The check runs on the **bytes**, on the far side of the render, rather than
/// on the renderer that produced them — the same instinct that puts the
/// vendoring gate in the publisher rather than in the notebook subprocess.
pub fn check_class(rendered: &Rendered) -> Result<()> {
    if rendered.class.allows_scripts() {
        return Ok(());
    }
    for path in html_files(&rendered.dir)? {
        let text = std::fs::read_to_string(&path)?;
        let lower = text.to_ascii_lowercase();
        for needle in [
            "<script",
            "javascript:",
            " onerror=",
            " onload=",
            " onclick=",
        ] {
            if let Some(at) = lower.find(needle) {
                let line = text[..at].lines().count();
                bail!(
                    "{}:{line} contains `{}`, but this bundle is `static`, which means \
                     nothing executes. A static bundle that emitted script must fail the \
                     publish rather than be quietly reclassified — the class decides the \
                     CSP and therefore which origin may serve it.",
                    path.display(),
                    needle.trim()
                );
            }
        }
    }
    Ok(())
}

fn html_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            out.extend(html_files(&path)?);
        } else if path.extension().and_then(|e| e.to_str()) == Some("html") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn first_heading(markdown: &str) -> Option<String> {
    markdown.lines().find_map(|line| {
        line.strip_prefix("# ")
            .map(|heading| heading.trim().to_string())
    })
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{ title }}</title>
<link rel="stylesheet" href="report.css">
</head>
<body>
<main>
{{ body }}
</main>
</body>
</html>
"#;

const REPORT_CSS: &str = r#"/* Generated by mecha-factory-publish. System fonts and no imports — an
   external font is an external reference, and a publish fails on one. */
:root { color-scheme: light dark; --fg: #1a1a1a; --bg: #fff; --muted: #5a5a5a;
        --line: #d8d8d8; --accent: #1f5fa9; --code-bg: #f4f4f6; }
@media (prefers-color-scheme: dark) {
  :root { --fg: #e8e8e8; --bg: #16181c; --muted: #9aa0a6; --line: #383c42;
          --accent: #7cb2ff; --code-bg: #22262c; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); line-height: 1.6;
       font-family: system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 46rem; margin: 0 auto; padding: 2.5rem 1.25rem 5rem; }
h1, h2, h3 { line-height: 1.25; margin: 2rem 0 .6rem; }
h1 { font-size: 1.75rem; margin-top: 0; }
h2 { font-size: 1.3rem; border-bottom: 1px solid var(--line); padding-bottom: .3rem; }
h3 { font-size: 1.05rem; }
p, ul, ol, blockquote, table { margin: .8rem 0; }
a { color: var(--accent); }
blockquote { margin-left: 0; padding-left: 1rem; border-left: 3px solid var(--line);
             color: var(--muted); }
code { background: var(--code-bg); padding: .1rem .3rem; border-radius: 4px;
       font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .9em; }
pre { background: var(--code-bg); padding: .8rem 1rem; border-radius: 6px;
      overflow-x: auto; }
pre code { background: none; padding: 0; }
table { border-collapse: collapse; width: 100%; display: block; overflow-x: auto; }
th, td { border: 1px solid var(--line); padding: .4rem .6rem; text-align: left; }
th { background: var(--code-bg); }
hr { border: 0; border-top: 1px solid var(--line); margin: 2rem 0; }
img { max-width: 100%; height: auto; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "factory-render-{name}-{}-{:?}",
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

    fn render(scratch: &Scratch, markdown: &str) -> Result<Rendered> {
        let source = scratch.0.join("brief.md");
        std::fs::write(&source, markdown).unwrap();
        report(&source, &scratch.0.join("out"), None)
    }

    #[test]
    fn a_markdown_report_becomes_a_page_that_references_only_its_own_files() {
        let scratch = Scratch::new("basic");
        let rendered = render(
            &scratch,
            "# Monday\n\nTwo meetings.\n\n| Time | What |\n|---|---|\n| 9am | Standup |\n",
        )
        .unwrap();

        assert_eq!(rendered.title, "Monday", "the title comes from the heading");
        assert_eq!(rendered.class, ContentClass::Static);
        let html = std::fs::read_to_string(rendered.dir.join("index.html")).unwrap();
        assert!(html.contains("<h1>Monday</h1>"));
        assert!(html.contains("<table>"), "tables are enabled");
        assert!(html.contains("<title>Monday</title>"));
        assert!(html.contains(r#"<link rel="stylesheet" href="report.css">"#));
        assert!(!html.contains("http://") && !html.contains("https://"));

        // The source travels with the bundle, so a later run reads back what
        // produced the page rather than reversing it out of HTML.
        assert!(rendered.dir.join("source.md").is_file());
        assert!(rendered.dir.join("report.css").is_file());
    }

    /// A briefing is written by a model from mail bodies and web pages, so its
    /// markdown is not trusted input — and pulldown-cmark passes raw HTML
    /// through. "Nothing runs it" is a property of the renderer, not of the
    /// bytes, so the check is on the bytes.
    #[test]
    fn a_static_bundle_that_emitted_script_fails_rather_than_being_reclassified() {
        let scratch = Scratch::new("script");
        for hostile in [
            "# Brief\n\n<script>alert(1)</script>\n",
            "# Brief\n\n<img src=x onerror=alert(1)>\n",
            "# Brief\n\n[click](javascript:alert(1))\n",
        ] {
            let err = render(&scratch, hostile)
                .expect_err(&format!("should have refused: {hostile}"))
                .to_string();
            assert!(
                err.contains("`static`"),
                "unexpected message for {hostile:?}: {err}"
            );
            assert!(err.contains("index.html"), "the file is named: {err}");
        }
    }

    /// A briefing quoting a command must not have its quotes and hyphens
    /// rewritten into typography that no longer runs.
    #[test]
    fn smart_punctuation_is_off_so_a_quoted_command_survives() {
        let scratch = Scratch::new("punct");
        let rendered = render(&scratch, "# X\n\nRun `mecha run --tool fs_read \"a\"`.\n").unwrap();
        let html = std::fs::read_to_string(rendered.dir.join("index.html")).unwrap();
        // Verbatim, including the straight quotes: pulldown-cmark leaves `"` as
        // itself in text content, which is valid and is what we want here.
        assert!(
            html.contains(r#"<code>mecha run --tool fs_read "a"</code>"#),
            "smart punctuation would have rewritten the flag or the quotes"
        );
    }

    /// The body is HTML by construction and the title is not — it is a string
    /// lifted out of markdown a model wrote from mail bodies.
    #[test]
    fn the_title_is_escaped_even_though_the_body_is_not() {
        let scratch = Scratch::new("titleesc");
        let rendered = render(&scratch, "# A </title> & <b>bold</b> day\n\nText.\n").unwrap();
        let html = std::fs::read_to_string(rendered.dir.join("index.html")).unwrap();
        assert!(
            html.contains("<title>A &lt;/title&gt; &amp; &lt;b&gt;bold&lt;/b&gt; day</title>"),
            "the title element was broken out of:\n{html}"
        );
        // ...while in the body, raw HTML is still passed through, which is
        // markdown's documented behaviour and the reason `check_class` scans
        // the emitted bytes rather than trusting the renderer.
        assert!(html.contains("<h1>A </title> &amp; <b>bold</b> day</h1>"));
    }

    #[test]
    fn a_report_with_no_heading_falls_back_to_the_filename() {
        let scratch = Scratch::new("noheading");
        let rendered = render(&scratch, "Just a paragraph.\n").unwrap();
        assert_eq!(rendered.title, "brief");
    }
}
