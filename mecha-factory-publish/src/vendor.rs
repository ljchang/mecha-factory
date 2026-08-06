//! The external-reference gate: the one enforcement the artifact model rests on.
//!
//! **A publish fails on a surviving external reference. Not warns.** A warning
//! is how this silently stops holding, and four separate things depend on it:
//!
//! - **The CSP is `default-src 'self'`.** A page that loads something off-origin
//!   does not degrade, it breaks — and it breaks in the reader's browser, after
//!   publication, where nobody is watching.
//! - **The reader's IP is not ours to give away.** A report that pulls a font or
//!   an image from a third party tells that third party who read it and when.
//! - **Permanence.** A version is immutable and addressable forever; a version
//!   that loads a CDN asset is immutable until someone else's bucket changes.
//! - **It is the prerequisite for the notebook path**, where the references are
//!   real and enumerated: the islands bundle, Pyodide, MathJax, Plotly.
//!
//! **The distinction that makes the gate usable rather than absurd: a link is
//! not a resource.** `<a href="https://…">` is a human clicking through, and a
//! briefing that could not link to the pull request it is about would be worse
//! than useless. `<img src="https://…">` is the *page* reaching out, without
//! asking, on load. Only the second is a finding. The blunt version of this
//! check — grep the bundle for `https?://` — cannot tell them apart, which is
//! why it lives here instead.
//!
//! **Every finding names the file, the line, the URL and what made it a
//! resource.** "Vendoring failed" is unactionable; recovery means knowing which
//! reference survived and where. Same reasoning as every other expected failure
//! in this project: reserve the unrecoverable error for what the caller cannot
//! route around.
//!
//! ### What this deliberately does not do yet
//!
//! **It does not fetch.** Rewriting an external reference to a vendored copy
//! means making an outbound request, and the URLs in a report arrive in markdown
//! a model wrote out of mail bodies and web pages — an address chosen by content
//! we do not trust. Fetching those needs the same guard mecha puts in front of
//! `http_fetch`, and a gate that quietly acquired an unguarded fetcher would be
//! a worse trade than the one it prevents. When vendoring does land it will pull
//! from a **pinned allowlist** — marimo's six references are known and
//! version-pinned — never from whatever the content happens to name.
//!
//! **It detects `data:` script URLs but does not rewrite them.** Under marimo's
//! script runtime every anywidget module is emitted as
//! `data:text/javascript;base64,…`, which collides with `script-src` on the
//! compute origin. Turning those into ordinary same-origin files is the right
//! fix and it needs no network at all — but it wants a real marimo bundle to be
//! written against, not a fixture invented here to make a test pass. Until then
//! it fails the publish and says exactly that, which is the enforcement either
//! way.

use anyhow::Result;
use std::fmt;
use std::path::{Path, PathBuf};

/// URLs that are **identifiers, not addresses**.
///
/// `xmlns="http://www.w3.org/2000/svg"` is how an SVG element declares what it
/// is; no browser ever fetches it. A real marimo bundle contains 234 of these,
/// and reporting them was simply a bug — it buried the ~30 references that are
/// genuinely fetchable at runtime under a pile nobody would read to the end of.
///
/// Matched exactly rather than by host prefix, deliberately: `w3.org` also
/// serves real scripts, and a prefix rule would wave those through.
const NAMESPACE_URIS: [&str; 6] = [
    "http://www.w3.org/2000/svg",
    "http://www.w3.org/1999/xhtml",
    "http://www.w3.org/1999/xlink",
    "http://www.w3.org/XML/1998/namespace",
    "http://www.w3.org/2000/xmlns/",
    "http://www.w3.org/1998/Math/MathML",
];

/// A third-party tree that is reviewed as a unit rather than line by line.
///
/// **Why a whole mode exists for this.** A real `marimo export html-wasm` is 710
/// files and 27 MB of minified vendor JavaScript containing 224 distinct URLs —
/// namespace identifiers, documentation links, attribution strings, and a
/// genuine handful of runtime CDN loaders. Per-line review of that is a
/// workflow nobody performs, and a check nobody performs is a check that is not
/// there.
///
/// So the unit of review becomes the tree: you review it once at the version
/// you pin, record its digest, and the **CSP is the runtime enforcement** — a
/// `connect-src 'self'` origin means a map-tile fetch buried in a charting
/// library simply fails. That is what §7.1 was always for; the gate just was
/// not scoped to match it.
///
/// **Fail-closed in both directions.** A subtree that is not declared is
/// scanned strictly, so nothing becomes vendored by being forgotten. And a
/// declared subtree whose digest no longer matches is a finding, not a pass —
/// otherwise "reviewed once" would silently mean "reviewed once, then never
/// again".
#[derive(Debug, Clone)]
pub struct Vendored {
    /// Relative to the bundle root.
    pub path: PathBuf,
    /// The digest recorded when this tree was reviewed.
    pub digest: String,
    /// What it is and at which version, for whoever reads the manifest later.
    pub description: String,
}

/// What made a reference a *resource* rather than a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// An attribute the browser fetches on load: `<img src>`, `<link href>`,
    /// `<script src>`, and friends. Carries the tag and attribute so the
    /// message can say what to change.
    Attribute { tag: String, attribute: String },
    /// `url(...)` in a stylesheet.
    CssUrl,
    /// `@import` in a stylesheet.
    CssImport,
    /// A URL in a script. Scripts are scanned bluntly, because a string in a
    /// program is indistinguishable from an address it will fetch.
    InScript,
    /// A `data:` URL in a script position. See the module docs — §7.3 of the
    /// design.
    DataUrlScript,
    /// A tree declared as reviewed-and-pinned no longer digests to what was
    /// recorded. Reported rather than rescanned: what changed is the question,
    /// and answering it with a pile of per-line findings would hide it.
    VendoredTreeChanged { expected: String, actual: String },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Attribute { tag, attribute } => write!(f, "<{tag} {attribute}=…>"),
            Reason::CssUrl => write!(f, "url(…) in CSS"),
            Reason::CssImport => write!(f, "@import in CSS"),
            Reason::InScript => write!(f, "a URL inside a script"),
            Reason::DataUrlScript => write!(f, "a data: URL in a script position"),
            Reason::VendoredTreeChanged { expected, actual } => write!(
                f,
                "a pinned third-party tree changed since it was reviewed \
                 (recorded {expected}, found {actual})"
            ),
        }
    }
}

/// One external reference that would survive publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Relative to the bundle root, so the message is the same wherever the
    /// bundle was staged.
    pub file: PathBuf,
    /// 1-indexed.
    pub line: usize,
    pub reference: String,
    pub reason: Reason,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}  {}  ({})",
            self.file.display(),
            self.line,
            self.reference,
            self.reason
        )
    }
}

/// Scan a rendered bundle and return every external reference in it.
///
/// Empty means the bundle is self-contained.
pub fn scan(bundle: &Path) -> Result<Vec<Finding>> {
    scan_with(bundle, &[])
}

/// Scan, treating the declared trees as reviewed-and-pinned units.
pub fn scan_with(bundle: &Path, vendored: &[Vendored]) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for tree in vendored {
        let dir = bundle.join(&tree.path);
        if !dir.is_dir() {
            // A declaration pointing at nothing is a manifest that has drifted
            // from the bundle. Silently ignoring it would mean a later rename
            // quietly turns strict scanning back on — or off.
            findings.push(Finding {
                file: tree.path.clone(),
                line: 1,
                reference: tree.description.clone(),
                reason: Reason::VendoredTreeChanged {
                    expected: tree.digest.clone(),
                    actual: "the directory does not exist".into(),
                },
            });
            continue;
        }
        let actual = crate::store::digest_tree(&dir)?;
        if actual != tree.digest {
            findings.push(Finding {
                file: tree.path.clone(),
                line: 1,
                reference: tree.description.clone(),
                reason: Reason::VendoredTreeChanged {
                    expected: tree.digest.clone(),
                    actual,
                },
            });
        }
    }
    scan_dir(bundle, bundle, vendored, &mut findings)?;
    findings.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.reference.cmp(&b.reference))
    });
    Ok(findings)
}

/// The gate. `Ok(())` iff the bundle is self-contained.
///
/// The error lists every finding, because a caller who fixes one and republishes
/// to be told about the next is a caller who stops using the gate.
pub fn gate(bundle: &Path) -> Result<()> {
    gate_with(bundle, &[])
}

/// The gate, with declared third-party trees reviewed as pinned units.
pub fn gate_with(bundle: &Path, vendored: &[Vendored]) -> Result<()> {
    let findings = scan_with(bundle, vendored)?;
    if findings.is_empty() {
        return Ok(());
    }
    let mut message = format!(
        "{} external reference(s) would survive this publish, and a published \
         bundle must load nothing off its own origin:\n",
        findings.len()
    );
    for finding in &findings {
        message.push_str(&format!("  {finding}\n"));
    }
    message.push_str(
        "\nA link a reader clicks (<a href=…>) is fine and is not listed here — \
         what is listed is the page fetching something on load. Either inline \
         the resource, drop it, or copy it into the bundle and point at the \
         local file.",
    );
    anyhow::bail!(message)
}

/// The gate, plus a pointer back to the file a person would actually edit.
///
/// A finding names `index.html:13` — a generated file, which is the wrong thing
/// to send someone to change. For a rendered bundle the editable artifact is the
/// source it came from, and the offending URL is verbatim in it, so it can be
/// located exactly. Without this the message is precise about a file nobody
/// should touch and silent about the one they should.
pub fn gate_rendered(bundle: &Path, source: &Path) -> Result<()> {
    let Err(e) = gate(bundle) else {
        return Ok(());
    };
    let Ok(text) = std::fs::read_to_string(source) else {
        return Err(e);
    };
    let mut message = e.to_string();
    message.push_str(&format!("\n\nIn the source ({}):", source.display()));
    for finding in scan(bundle)? {
        match locate(&text, &finding.reference) {
            Some(line) => message.push_str(&format!(
                "\n  {}:{line}  {}",
                source.display(),
                finding.reference
            )),
            // Emitted by the template rather than written by hand — say so
            // instead of leaving a gap the reader has to interpret.
            None => message.push_str(&format!(
                "\n  (not in the source: {} comes from the template)",
                finding.reference
            )),
        }
    }
    anyhow::bail!(message)
}

/// The 1-indexed line a URL appears on, if it does.
fn locate(text: &str, needle: &str) -> Option<usize> {
    text.lines()
        .position(|line| line.contains(needle))
        .map(|i| i + 1)
}

fn scan_dir(root: &Path, dir: &Path, vendored: &[Vendored], out: &mut Vec<Finding>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        // A pinned tree was reviewed as a unit; its digest is checked above and
        // its contents are not walked. Everything not declared is scanned
        // strictly, so nothing becomes vendored by being forgotten.
        if vendored.iter().any(|v| relative.starts_with(&v.path)) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            scan_dir(root, &path, vendored, out)?;
            continue;
        }
        // Binary files cannot be scanned and do not need to be: an image is
        // bytes, not a reference. A file that is not valid UTF-8 is treated as
        // binary rather than as an error.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match path.extension().and_then(|e| e.to_str()) {
            Some("html") | Some("htm") => scan_html(&relative, &text, out),
            Some("css") => scan_css(&relative, &text, out),
            Some("js") | Some("mjs") => scan_script(&relative, &text, out),
            // Everything else — markdown sources, JSON data, the manifest — is
            // not loaded by a browser, so a URL in it is data rather than a
            // reference. `source.md` travels with every report specifically so
            // a later run can read it; flagging the links inside it would make
            // that impossible.
            _ => {}
        }
    }
    Ok(())
}

/// Attributes a browser fetches. `href` is deliberately absent: it is a link on
/// `<a>` and a resource on `<link>`, and that is decided per tag below.
const RESOURCE_ATTRIBUTES: [&str; 6] = ["src", "srcset", "poster", "data", "action", "formaction"];

/// Tags whose `href` the browser fetches rather than offering to the reader.
const HREF_LOADS: [&str; 2] = ["link", "base"];

fn scan_html(file: &Path, text: &str, out: &mut Vec<Finding>) {
    for tag in tags(text) {
        for (name, value) in &tag.attributes {
            let loads = RESOURCE_ATTRIBUTES.contains(&name.as_str())
                || (name == "href" && HREF_LOADS.contains(&tag.name.as_str()));
            if !loads {
                continue;
            }
            // `srcset` is a comma-separated list of candidates, each with an
            // optional descriptor. Splitting it matters: a bundle can be
            // self-contained in the 1x image and reach a CDN for the 2x one.
            for candidate in value.split(',') {
                let url = candidate.split_whitespace().next().unwrap_or("").trim();
                if url.is_empty() {
                    continue;
                }
                if is_external(url) {
                    out.push(Finding {
                        file: file.to_path_buf(),
                        line: line_of(text, tag.at),
                        reference: url.to_string(),
                        reason: Reason::Attribute {
                            tag: tag.name.clone(),
                            attribute: name.clone(),
                        },
                    });
                } else if name == "src" && tag.name == "script" && url.starts_with("data:") {
                    out.push(Finding {
                        file: file.to_path_buf(),
                        line: line_of(text, tag.at),
                        reference: truncate(url),
                        reason: Reason::DataUrlScript,
                    });
                }
            }
        }
    }
    // Inline <style> is CSS wherever it lives, and an @import inside one
    // reaches out exactly as a stylesheet's would.
    for (offset, css) in inline_blocks(text, "style") {
        let mut inner = Vec::new();
        scan_css(file, css, &mut inner);
        for mut finding in inner {
            finding.line += line_of(text, offset) - 1;
            out.push(finding);
        }
    }
    for (offset, js) in inline_blocks(text, "script") {
        let mut inner = Vec::new();
        scan_script(file, js, &mut inner);
        for mut finding in inner {
            finding.line += line_of(text, offset) - 1;
            out.push(finding);
        }
    }
}

fn scan_css(file: &Path, text: &str, out: &mut Vec<Finding>) {
    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("@import") {
            for url in urls_in(rest) {
                out.push(Finding {
                    file: file.to_path_buf(),
                    line: number + 1,
                    reference: url,
                    reason: Reason::CssImport,
                });
            }
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find("url(") {
            let after = &rest[at + 4..];
            let end = after.find(')').unwrap_or(after.len());
            let raw = after[..end].trim().trim_matches(['"', '\'']);
            if is_external(raw) {
                out.push(Finding {
                    file: file.to_path_buf(),
                    line: number + 1,
                    reference: raw.to_string(),
                    reason: Reason::CssUrl,
                });
            }
            rest = &after[end.min(after.len())..];
        }
    }
}

/// Scripts are scanned bluntly, and that is not laziness.
///
/// A string literal in a program is indistinguishable from an address the
/// program will fetch — there is no static analysis that separates them without
/// running the thing. So every absolute URL in a script is a finding, and the
/// answer to a false positive is to not put a URL in a script that ships in a
/// bundle.
fn scan_script(file: &Path, text: &str, out: &mut Vec<Finding>) {
    for (number, line) in text.lines().enumerate() {
        for url in urls_in(line) {
            out.push(Finding {
                file: file.to_path_buf(),
                line: number + 1,
                reference: url,
                reason: Reason::InScript,
            });
        }
    }
}

/// Absolute URLs in a fragment of text, minus the ones that are identifiers.
fn urls_in(text: &str) -> Vec<String> {
    urls_in_raw(text)
        .into_iter()
        .filter(|url| !NAMESPACE_URIS.contains(&url.as_str()))
        .collect()
}

fn urls_in_raw(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        // Walk back over the scheme.
        let before = &rest[..at];
        let start = before
            .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '-' && c != '.')
            .map(|i| i + 1)
            .unwrap_or(0);
        let scheme = &before[start..];
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            let after = &rest[at + 3..];
            let end = after
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ')' | '>' | ',' | ';')
                })
                .unwrap_or(after.len());
            out.push(format!("{scheme}://{}", &after[..end]));
        }
        rest = &rest[at + 3..];
    }
    out
}

/// Protocol-relative (`//host/x`) counts: the browser resolves it to the page's
/// scheme and fetches it off-origin exactly as an absolute URL would. Missing
/// that is the classic way a check like this is bypassed by accident.
fn is_external(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("//")
}

fn truncate(url: &str) -> String {
    if url.len() <= 60 {
        return url.to_string();
    }
    format!("{}… ({} bytes)", &url[..60], url.len())
}

/// The 1-indexed line an offset falls on.
///
/// Counting newlines rather than `lines().count()`: the latter does not count a
/// trailing one, so everything after the first line was reported one line early
/// — which sends a reader to the wrong place, in the one message whose entire
/// job is to say where to look.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1
}

struct Tag {
    name: String,
    attributes: Vec<(String, String)>,
    /// Byte offset of the `<`.
    at: usize,
}

/// A deliberately small tag scanner.
///
/// Not an HTML parser, and it does not need to be: it answers one question —
/// which attribute values would a browser fetch — and errs toward *reporting*.
/// A construct it misreads produces a finding on something harmless, which
/// costs someone a look; the opposite would let a reference through.
fn tags(text: &str) -> Vec<Tag> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let at = i;
        i += 1;
        // Comments and closing tags carry nothing to fetch.
        if text[i..].starts_with("!--") {
            i = text[i..]
                .find("-->")
                .map(|e| i + e + 3)
                .unwrap_or(bytes.len());
            continue;
        }
        if i < bytes.len() && (bytes[i] == b'/' || bytes[i] == b'!' || bytes[i] == b'?') {
            i = text[i..]
                .find('>')
                .map(|e| i + e + 1)
                .unwrap_or(bytes.len());
            continue;
        }
        let name_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
            i += 1;
        }
        if i == name_start {
            continue;
        }
        let name = text[name_start..i].to_ascii_lowercase();

        let mut attributes = Vec::new();
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] == b'>' {
                i = (i + 1).min(bytes.len());
                break;
            }
            if bytes[i] == b'/' {
                i += 1;
                continue;
            }
            let key_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'='
                && bytes[i] != b'>'
            {
                i += 1;
            }
            let key = text[key_start..i].to_ascii_lowercase();
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'=' {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                let value = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                    let quote = bytes[i];
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != quote {
                        i += 1;
                    }
                    let v = text[start..i.min(bytes.len())].to_string();
                    i = (i + 1).min(bytes.len());
                    v
                } else {
                    let start = i;
                    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                        i += 1;
                    }
                    text[start..i].to_string()
                };
                if !key.is_empty() {
                    attributes.push((key, value));
                }
            } else if !key.is_empty() {
                attributes.push((key, String::new()));
            }
        }
        out.push(Tag {
            name,
            attributes,
            at,
        });
    }
    out
}

/// The contents of every `<tag>…</tag>` of one name, with the byte offset the
/// content starts at.
fn inline_blocks<'a>(text: &'a str, tag: &str) -> Vec<(usize, &'a str)> {
    let lower = text.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find(&open) {
        let start = from + at;
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let content_start = start + gt + 1;
        let Some(end) = lower[content_start..].find(&close) else {
            break;
        };
        out.push((content_start, &text[content_start..content_start + end]));
        from = content_start + end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "factory-vendor-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn write(&self, name: &str, contents: &str) -> &Self {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
            self
        }
        fn scan(&self) -> Vec<Finding> {
            scan(&self.0).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The distinction the whole gate turns on. A briefing that could not link
    /// to the pull request it is about would be worse than useless.
    #[test]
    fn a_link_is_not_a_resource() {
        let s = Scratch::new("link");
        s.write(
            "index.html",
            r#"<p>See <a href="https://github.com/ljchang/mecha/pull/1">the PR</a>.</p>
<p>And <a href="https://example.edu">this</a>.</p>"#,
        );
        assert!(s.scan().is_empty(), "{:?}", s.scan());
    }

    #[test]
    fn everything_the_page_fetches_on_load_is_a_finding() {
        let s = Scratch::new("resources");
        s.write(
            "index.html",
            r#"<link rel="stylesheet" href="https://cdn.example/a.css">
<script src="https://cdn.example/b.js"></script>
<img src="https://cdn.example/c.png">
<img srcset="local.png 1x, https://cdn.example/d@2x.png 2x">
<video poster="https://cdn.example/e.jpg"></video>
<form action="https://evil.example/collect"></form>"#,
        );
        let findings = s.scan();
        let refs: Vec<&str> = findings.iter().map(|f| f.reference.as_str()).collect();
        assert_eq!(
            refs,
            [
                "https://cdn.example/a.css",
                "https://cdn.example/b.js",
                "https://cdn.example/c.png",
                "https://cdn.example/d@2x.png",
                "https://cdn.example/e.jpg",
                "https://evil.example/collect",
            ],
            "{findings:#?}"
        );
        // The message has to say what to change, not just that something is
        // wrong.
        assert!(matches!(
            &findings[0].reason,
            Reason::Attribute { tag, attribute } if tag == "link" && attribute == "href"
        ));
    }

    /// A `srcset` bundle can be self-contained in the 1x image and reach a CDN
    /// for the 2x one — a split the naive read misses entirely.
    #[test]
    fn srcset_is_split_so_one_bad_candidate_is_still_caught() {
        let s = Scratch::new("srcset");
        s.write(
            "index.html",
            r#"<img srcset="a.png 1x, //cdn.example/b.png 2x">"#,
        );
        let findings = s.scan();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reference, "//cdn.example/b.png");
    }

    /// The classic accidental bypass: the browser resolves `//host/x` against
    /// the page's scheme and fetches it off-origin just the same.
    #[test]
    fn a_protocol_relative_url_is_external() {
        assert!(is_external("//cdn.example/x.js"));
        assert!(is_external("HTTPS://cdn.example/x.js"));
        assert!(!is_external("/local/x.js"));
        assert!(!is_external("./x.js"));
        assert!(!is_external("x.js"));
        assert!(!is_external("data:text/css,body{}"));
    }

    #[test]
    fn css_reaches_out_through_import_and_url_wherever_it_lives() {
        let s = Scratch::new("css");
        s.write(
            "report.css",
            "@import url(\"https://fonts.example/f.css\");\nbody { background: url('https://cdn.example/bg.png'); }\n.local { background: url(./bg.png); }\n",
        );
        s.write(
            "index.html",
            "<style>\n@import \"https://fonts.example/g.css\";\n</style>",
        );
        let findings = s.scan();
        assert_eq!(findings.len(), 3, "{findings:#?}");
        assert_eq!(findings[0].file, PathBuf::from("index.html"));
        assert_eq!(findings[0].line, 2, "the line is relative to the html file");
        assert_eq!(findings[1].reason, Reason::CssImport);
        assert_eq!(findings[2].reason, Reason::CssUrl);
    }

    /// A string in a program is indistinguishable from an address it will
    /// fetch, so scripts are scanned bluntly and on purpose.
    #[test]
    fn a_url_anywhere_in_a_script_is_a_finding() {
        let s = Scratch::new("script");
        s.write(
            "app.js",
            "const cdn = \"https://cdn.example/lib.js\";\nfetch(cdn);\n",
        );
        s.write(
            "index.html",
            "<script>\nnew Worker('https://cdn.example/w.js');\n</script>",
        );
        let findings = s.scan();
        assert_eq!(findings.len(), 2, "{findings:#?}");
        assert!(findings.iter().all(|f| f.reason == Reason::InScript));
        assert_eq!(findings[1].line, 2);
    }

    /// §7.3: marimo emits every anywidget module as a `data:` script URL, which
    /// collides with `script-src` on the compute origin. Detected and refused
    /// rather than rewritten, until there is a real bundle to write the rewrite
    /// against.
    #[test]
    fn a_data_url_in_a_script_position_is_refused_with_its_own_reason() {
        let s = Scratch::new("dataurl");
        s.write(
            "index.html",
            "<script src=\"data:text/javascript;base64,YWxlcnQoMSk=\"></script>\n\
             <img src=\"data:image/png;base64,iVBORw0KGgo=\">",
        );
        let findings = s.scan();
        assert_eq!(findings.len(), 1, "an inline image is fine: {findings:#?}");
        assert_eq!(findings[0].reason, Reason::DataUrlScript);
    }

    #[test]
    fn markdown_and_data_files_are_not_scanned() {
        let s = Scratch::new("sources");
        // Every report ships its own source.md so a later run can read it. A
        // link inside it is data, not a reference — flagging it would make the
        // read-back impossible.
        s.write("source.md", "See <https://example.edu/paper>.\n");
        s.write("bundle.json", r#"{"sources":["https://example.edu"]}"#);
        assert!(s.scan().is_empty());
    }

    #[test]
    fn a_comment_carries_nothing_to_fetch() {
        let s = Scratch::new("comment");
        s.write(
            "index.html",
            "<!-- <img src=\"https://cdn.example/old.png\"> -->\n<p>ok</p>",
        );
        assert!(s.scan().is_empty(), "{:?}", s.scan());
    }

    #[test]
    fn the_gate_lists_every_finding_rather_than_the_first() {
        let s = Scratch::new("gate");
        s.write(
            "index.html",
            "<img src=\"https://a.example/1.png\">\n<img src=\"https://b.example/2.png\">",
        );
        let err = gate(&s.0).unwrap_err().to_string();
        assert!(err.contains("2 external reference"), "{err}");
        assert!(err.contains("https://a.example/1.png"), "{err}");
        assert!(err.contains("https://b.example/2.png"), "{err}");
        // One per line, and the line numbers are the ones a reader would find
        // them on — an off-by-one here sends them to the wrong place.
        assert!(
            err.contains("index.html:1  https://a.example/1.png"),
            "{err}"
        );
        assert!(
            err.contains("index.html:2  https://b.example/2.png"),
            "{err}"
        );
        // And it says what a reader is allowed to do, or the first response to
        // it is to delete the link they were entitled to keep.
        assert!(err.contains("<a href=…>"), "{err}");
    }

    /// A finding names a generated file, which is the wrong thing to send
    /// someone to edit.
    #[test]
    fn the_rendered_gate_points_at_the_source_a_person_would_change() {
        let s = Scratch::new("rendered");
        s.write(
            "index.html",
            "<p>x</p>\n<img src=\"https://cdn.example/c.png\">\n\
             <link rel=\"stylesheet\" href=\"https://cdn.example/t.css\">",
        );
        let source = s.0.join("brief.md");
        std::fs::write(
            &source,
            "# Brief\n\nprose\n\n![c](https://cdn.example/c.png)\n",
        )
        .unwrap();

        let err = gate_rendered(&s.0, &source).unwrap_err().to_string();
        assert!(
            err.contains("brief.md:5"),
            "the source line is named: {err}"
        );
        // The one the template emitted is called out rather than left as a gap
        // the reader has to interpret.
        assert!(
            err.contains("not in the source: https://cdn.example/t.css"),
            "{err}"
        );
    }

    /// 234 of a real marimo bundle's 541 findings were these. Reporting them
    /// buried the ~30 that are genuinely fetchable under a pile nobody would
    /// read to the end of.
    #[test]
    fn an_xml_namespace_is_an_identifier_and_not_a_finding() {
        let s = Scratch::new("ns");
        s.write(
            "app.js",
            "const SVG=\"http://www.w3.org/2000/svg\";\n\
             const X=\"http://www.w3.org/1999/xlink\";\n\
             load(\"https://cdn.example/real.js\");\n",
        );
        let findings = s.scan();
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].reference, "https://cdn.example/real.js");
    }

    /// A prefix rule on the host would wave through a real script served from
    /// w3.org, so the match is exact.
    #[test]
    fn a_real_script_on_a_namespace_host_is_still_a_finding() {
        let s = Scratch::new("nshost");
        s.write("app.js", "load(\"http://www.w3.org/scripts/thing.js\");");
        assert_eq!(s.scan().len(), 1);
    }

    /// The unit of review for 710 minified files is the tree, not the line.
    #[test]
    fn a_declared_tree_is_reviewed_as_a_unit_and_an_undeclared_one_is_not() {
        let s = Scratch::new("vendored");
        s.write("index.html", "<p>ok</p>");
        s.write("assets/runtime.js", "fetch(\"https://cdn.example/tiles\");");
        s.write("other/thing.js", "fetch(\"https://cdn.example/other\");");

        // Undeclared: both are scanned.
        assert_eq!(scan(&s.0).unwrap().len(), 2);

        let digest = crate::store::digest_tree(&s.0.join("assets")).unwrap();
        let vendored = [Vendored {
            path: PathBuf::from("assets"),
            digest: digest.clone(),
            description: "marimo 0.23.16".into(),
        }];
        // Declared: the tree is skipped, and everything outside it still is not.
        let findings = scan_with(&s.0, &vendored).unwrap();
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].file, PathBuf::from("other/thing.js"));
    }

    /// Or "reviewed once" would silently mean "reviewed once, then never
    /// again".
    #[test]
    fn a_pinned_tree_that_changed_is_a_finding_rather_than_a_pass() {
        let s = Scratch::new("drift");
        s.write("assets/runtime.js", "// v1");
        let digest = crate::store::digest_tree(&s.0.join("assets")).unwrap();
        let vendored = [Vendored {
            path: PathBuf::from("assets"),
            digest,
            description: "marimo 0.23.16".into(),
        }];
        assert!(scan_with(&s.0, &vendored).unwrap().is_empty());

        s.write("assets/runtime.js", "// v2, quietly different");
        let findings = scan_with(&s.0, &vendored).unwrap();
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0].reason,
            Reason::VendoredTreeChanged { .. }
        ));

        // And a declaration pointing at nothing is drift too — a rename must
        // not quietly turn strict scanning on or off.
        let missing = [Vendored {
            path: PathBuf::from("nowhere"),
            digest: "sha256:x".into(),
            description: "gone".into(),
        }];
        assert_eq!(scan_with(&s.0, &missing).unwrap().len(), 1);
    }

    #[test]
    fn a_self_contained_bundle_passes() {
        let s = Scratch::new("clean");
        s.write(
            "index.html",
            "<link rel=\"stylesheet\" href=\"report.css\">\n<img src=\"chart.png\">\n\
             <a href=\"https://example.edu\">a link</a>",
        );
        s.write("report.css", "body { font-family: system-ui; }");
        gate(&s.0).unwrap();
    }

    /// Binary files are bytes, not references, and one that is not valid UTF-8
    /// must not be an error.
    #[test]
    fn a_binary_file_is_skipped_rather_than_failing_the_scan() {
        let s = Scratch::new("binary");
        s.write("index.html", "<p>ok</p>");
        std::fs::write(s.0.join("chart.png"), [0x89, 0x50, 0x4e, 0xff, 0xfe, 0x00]).unwrap();
        assert!(s.scan().is_empty());
    }
}
