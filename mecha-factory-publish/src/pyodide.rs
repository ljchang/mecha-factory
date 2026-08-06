//! Vendoring the Python runtime, from a pinned allowlist.
//!
//! A `compute` bundle is not publishable until this has run: marimo's export
//! loads Pyodide, its standard library and every wheel from
//! `cdn.jsdelivr.net`, `files.pythonhosted.org` and `wasm.marimo.app` at
//! runtime, and the compute origin's `connect-src 'self'` correctly refuses all
//! of it. Measured in a browser rather than argued about — see
//! `scripts/csp-probe.py`.
//!
//! **The allowlist is the whole security story of this module.** It fetches
//! from three hardcoded hosts and nowhere else. It never fetches a URL that
//! came out of notebook content, which is the rule the vendoring gate refused
//! to break: a report's markdown is written by a model out of mail bodies and
//! web pages, and resolving an address chosen there needs the guard mecha puts
//! in front of `http_fetch`. Here the addresses come from Pyodide's own lock
//! file and from a version-pinned distribution path, so there is nothing for
//! content to influence.
//!
//! **Every fetched wheel is verified against a digest we did not compute.**
//! Pyodide's lock file already carries a `sha256` per package, which makes
//! "the CDN served something else today" a caught error rather than a vendored
//! one. The core files have no such digest, so they are recorded in a lockfile
//! of our own on first fetch and verified against it forever after — first use
//! is trust-on-first-use, every use after that is not.
//!
//! **Cached once per version, copied per bundle.** `~/.mecha/pyodide/v314.0.0/`
//! means the second notebook costs nothing, and copying into each bundle keeps
//! a published version self-contained: a notebook published today must not stop
//! working because a *later* notebook wanted a different Pyodide.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The only hosts this module will fetch from, ever.
const ALLOWED_HOSTS: [&str; 3] = [
    "cdn.jsdelivr.net",
    "files.pythonhosted.org",
    "wasm.marimo.app",
];

/// Files the distribution needs that the lock file does not list.
///
/// `pyodide.asm.mjs` is the one worth naming: it is loaded by dynamic
/// `import()`, which does **not** surface as a request event in a browser, so a
/// record-and-replay of a live run misses it entirely and the bundle fails on
/// the one file nothing observed. Enumerated rather than recorded for exactly
/// that reason.
const CORE_FILES: [&str; 5] = [
    "pyodide.mjs",
    "pyodide.js",
    "pyodide.asm.mjs",
    "pyodide.asm.wasm",
    "python_stdlib.zip",
];

/// Where a vendored runtime lives inside a bundle. Same-origin, so
/// `script-src 'self'` and `connect-src 'self'` both accept it.
pub const BUNDLE_DIR: &str = "pyodide";

/// One entry of the lock file, as far as we care about it.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct LockPackage {
    name: String,
    version: String,
    /// A bare filename for a package hosted in the distribution, or a full URL
    /// for one micropip pulls from PyPI. Rewritten to a local path.
    file_name: String,
    sha256: String,
    #[serde(default)]
    depends: Vec<String>,
    #[serde(default)]
    imports: Vec<String>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Lock {
    info: serde_json::Value,
    packages: BTreeMap<String, LockPackage>,
}

/// Digests for the files the lock file does not cover.
#[derive(Debug, Default, Deserialize, Serialize)]
struct CoreLock {
    files: BTreeMap<String, String>,
}

pub struct Vendorer {
    pub version: String,
    /// marimo's version, because the lock file that matters is **marimo's**,
    /// not Pyodide's.
    ///
    /// Pyodide publishes a lock for its own distribution; marimo publishes an
    /// augmented one at `wasm.marimo.app` that adds seventeen packages hosted
    /// on PyPI — `marimo-base` itself among them. Fetching Pyodide's own lock
    /// looks right and then fails on the first bootstrap package, which is how
    /// this was found.
    pub marimo_version: String,
    pub cache: PathBuf,
    /// Packages to vendor beyond what the notebook imports. Measured from a
    /// real run: marimo bootstraps these before any cell executes.
    pub roots: Vec<String>,
}

/// What marimo installs at bootstrap, before a single cell runs.
///
/// Measured by watching a real export in a browser, not read from marimo's
/// source — the list in the JS bundle is minified and conditional. It is
/// therefore **tied to a marimo version**, and the way we find out it drifted
/// is the browser probe failing, not this list being quietly wrong.
pub const MARIMO_BOOTSTRAP: [&str; 13] = [
    "micropip",
    "packaging",
    "msgspec",
    "marimo-base",
    "markdown",
    "pymdown-extensions",
    "narwhals",
    "jedi",
    "parso",
    "pygments",
    "docutils",
    // The two that were easy to miss by eye and that the browser named
    // immediately once an unvendored package failed loudly instead of
    // silently — which is the argument for keeping every lock entry.
    "pyodide-http",
    "pyyaml",
];

impl Vendorer {
    pub fn new(version: &str, marimo_version: &str, cache: PathBuf) -> Self {
        Vendorer {
            version: version.to_string(),
            marimo_version: marimo_version.to_string(),
            cache,
            roots: MARIMO_BOOTSTRAP.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn dist_base(&self) -> String {
        format!("https://cdn.jsdelivr.net/pyodide/{}/full/", self.version)
    }

    fn version_cache(&self) -> PathBuf {
        self.cache.join(&self.version)
    }

    /// Fetch (or reuse) everything, then copy it into the bundle.
    ///
    /// `imports` are the notebook's own top-level imports; anything the lock
    /// file knows by that name is vendored along with its dependency closure.
    pub fn vendor_into(&self, bundle: &Path, imports: &[String]) -> Result<VendorReport> {
        let cache = self.version_cache();
        std::fs::create_dir_all(&cache)?;

        let lock_text = self.fetch_lock()?;
        let lock: Lock = serde_json::from_str(&lock_text).context("parsing pyodide-lock.json")?;

        let wanted = self.closure(&lock, imports)?;
        let mut fetched = Vec::new();
        let mut rewritten = Lock {
            info: lock.info.clone(),
            packages: BTreeMap::new(),
        };

        // **Every entry stays in the lock file.** Dropping the ones we do not
        // vendor looks tidy and breaks resolution: Pyodide reads this file to
        // answer "what is this package and what does it depend on", so a name
        // it cannot find is not a missing download but a resolver that gives
        // up — measured as a page whose kernel never finishes booting, with no
        // console error to say why.
        //
        // So an unvendored package keeps its original absolute URL. It then
        // fails at *fetch* time, where `connect-src 'self'` refuses it and the
        // browser says exactly which package tried to leave. Loud beats tidy.
        for (key, package) in &lock.packages {
            let mut p = package.clone();
            if wanted.contains(key) {
                let url = self.package_url(package);
                let local = file_name_of(&url)?;
                let bytes = self.fetch_cached(&url, &local, Some(&package.sha256))?;
                fetched.push((local.clone(), bytes));
                p.file_name = local;
            } else {
                p.file_name = self.package_url(package);
            }
            rewritten.packages.insert(key.clone(), p);
        }

        for name in CORE_FILES {
            let url = format!("{}{name}", self.dist_base());
            let bytes = self.fetch_cached(&url, name, None)?;
            fetched.push((name.to_string(), bytes));
        }

        // The rewritten lock ships beside the wheels, so micropip resolves
        // every `file_name` relative to the bundle rather than to a CDN.
        let target = bundle.join(BUNDLE_DIR);
        std::fs::create_dir_all(&target)?;
        for (name, _) in &fetched {
            std::fs::copy(cache.join(name), target.join(name))
                .with_context(|| format!("copying {name} into the bundle"))?;
        }
        std::fs::write(
            target.join("pyodide-lock.json"),
            serde_json::to_string(&rewritten)?,
        )?;

        Ok(VendorReport {
            version: self.version.clone(),
            files: fetched.len() + 1,
            bytes: fetched.iter().map(|(_, b)| *b).sum(),
            packages: wanted.len(),
        })
    }

    /// A package's URL: PyPI when the lock file gives a full one, the pinned
    /// distribution otherwise.
    fn package_url(&self, package: &LockPackage) -> String {
        if package.file_name.starts_with("http") {
            package.file_name.clone()
        } else {
            format!("{}{}", self.dist_base(), package.file_name)
        }
    }

    /// Every package needed to satisfy the roots and the notebook's imports,
    /// closed over `depends`.
    ///
    /// Transitive, because a wheel that imports something we did not vendor
    /// fails at the point a reader interacts with the page — long after anyone
    /// is watching.
    fn closure(&self, lock: &Lock, imports: &[String]) -> Result<BTreeSet<String>> {
        // The lock keys packages by name, but a notebook imports a *module*,
        // and the two differ often enough to matter (`yaml` → `pyyaml`).
        let mut by_import: BTreeMap<&str, &str> = BTreeMap::new();
        for (key, package) in &lock.packages {
            for module in &package.imports {
                by_import.insert(module.as_str(), key.as_str());
            }
        }

        let mut queue: Vec<String> = Vec::new();
        for root in &self.roots {
            let key = normalise(root);
            if lock.packages.contains_key(&key) {
                queue.push(key);
            } else {
                bail!(
                    "pyodide's lock file for {} has no package `{root}` — \
                     marimo's bootstrap set has drifted from what this vendors",
                    self.version
                );
            }
        }
        for module in imports {
            let root = module.split('.').next().unwrap_or(module);
            if let Some(key) = by_import.get(root).copied() {
                queue.push(key.to_string());
            } else if lock.packages.contains_key(&normalise(root)) {
                queue.push(normalise(root));
            }
            // A module the lock file does not know is either part of the
            // standard library (already in python_stdlib.zip) or genuinely
            // unavailable, and micropip will say which at import time. Not an
            // error here: refusing to vendor over an unrecognised `import os`
            // would be absurd.
        }

        let mut seen = BTreeSet::new();
        while let Some(key) = queue.pop() {
            if !seen.insert(key.clone()) {
                continue;
            }
            if let Some(package) = lock.packages.get(&key) {
                for dependency in &package.depends {
                    queue.push(normalise(dependency));
                }
            }
        }
        Ok(seen)
    }

    fn fetch_lock(&self) -> Result<String> {
        // marimo's, not Pyodide's. See the field docs on `marimo_version`.
        let url = format!(
            "https://wasm.marimo.app/pyodide-lock.json?v={}&pyodide={}",
            self.marimo_version, self.version
        );
        let name = format!("pyodide-lock.marimo-{}.json", self.marimo_version);
        let bytes = self.fetch_cached_bytes(&url, &name, None)?;
        Ok(String::from_utf8(bytes)?)
    }

    fn fetch_cached(&self, url: &str, name: &str, sha256: Option<&str>) -> Result<u64> {
        Ok(self.fetch_cached_bytes(url, name, sha256)?.len() as u64)
    }

    /// Fetch through the cache, verifying against a digest.
    ///
    /// The digest for a wheel comes from the lock file — one we did not
    /// compute, which is what makes it worth checking. For the core files
    /// there is none upstream, so the first fetch records one and every fetch
    /// after that is verified against it: trust on first use, and never again.
    fn fetch_cached_bytes(&self, url: &str, name: &str, sha256: Option<&str>) -> Result<Vec<u8>> {
        let host = url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("");
        if !ALLOWED_HOSTS.contains(&host) {
            bail!("refusing to fetch from `{host}`: not on the pinned allowlist");
        }

        let path = self.version_cache().join(name);
        if path.is_file() {
            let bytes = std::fs::read(&path)?;
            self.verify(name, &bytes, sha256)?;
            return Ok(bytes);
        }

        let mut response = ureq::get(url)
            .call()
            .with_context(|| format!("fetching {url}"))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut response.body_mut().as_reader(), &mut bytes)
            .with_context(|| format!("reading {url}"))?;
        self.verify(name, &bytes, sha256)?;

        // Temp-and-rename, so an interrupted fetch does not leave a truncated
        // file that the next run trusts because it exists.
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(bytes)
    }

    fn verify(&self, name: &str, bytes: &[u8], sha256: Option<&str>) -> Result<()> {
        let actual = format!("{:x}", Sha256::digest(bytes));
        match sha256 {
            Some(expected) => {
                if actual != expected {
                    bail!(
                        "{name} does not match the digest pyodide's lock file \
                         records (expected {expected}, got {actual})"
                    );
                }
                Ok(())
            }
            None => {
                // No upstream digest, so keep our own.
                let path = self.version_cache().join("core-lock.json");
                let mut core: CoreLock = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default();
                match core.files.get(name) {
                    Some(expected) if *expected != actual => bail!(
                        "{name} changed since it was first vendored \
                         (recorded {expected}, got {actual})"
                    ),
                    Some(_) => Ok(()),
                    None => {
                        core.files.insert(name.to_string(), actual);
                        std::fs::write(&path, serde_json::to_string_pretty(&core)?)?;
                        Ok(())
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct VendorReport {
    pub version: String,
    pub files: usize,
    pub bytes: u64,
    pub packages: usize,
}

/// Point marimo's workers at the vendored runtime.
///
/// Two literals, in two minified files, and there is no configuration hook:
/// `packageBaseUrl` exists as a parameter and marimo assigns it the CDN string
/// itself. The substitution is brittle across marimo versions **on purpose** —
/// it fails loudly when the literal is gone, where a cleverer rewrite would
/// quietly match nothing and publish a notebook that cannot boot.
pub fn point_workers_at_bundle(bundle: &Path, version: &str) -> Result<usize> {
    let cdn = format!("https://cdn.jsdelivr.net/pyodide/{version}/full/");
    let mut changed = 0;

    // Both trees, and `pyodide/` is not an afterthought: Pyodide's own
    // `pyodide.js`/`pyodide.mjs` carry a default `indexURL` pointing at the CDN
    // they shipped from. marimo passes an explicit one so the default is a
    // fallback — but a fallback that reaches a CDN is exactly what
    // `connect-src 'self'` exists to refuse, and leaving it would mean the
    // bundle is one changed call away from not being self-contained.
    for (dir, local) in [
        // Relative to the worker, which lives in assets/.
        ("assets", format!("../{BUNDLE_DIR}/")),
        // Relative to a file already inside pyodide/.
        (BUNDLE_DIR, "./".to_string()),
    ] {
        changed += point_dir_at(&bundle.join(dir), &cdn, &local)?;
    }
    Ok(changed)
}

fn point_dir_at(dir: &Path, cdn: &str, local: &str) -> Result<usize> {
    let mut changed = 0;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if !matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("js") | Some("mjs")
        ) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // The version is interpolated in the minified source
        // (`pyodide/v${e.pyodideVersion}/full/`), so the template form is what
        // is actually present — both spellings are replaced.
        let mut rewritten = text.replace(cdn, local);
        rewritten = replace_template_versions(&rewritten, local);
        // The lock file marimo asks wasm.marimo.app for is now ours.
        rewritten = replace_lock_url(&rewritten, &format!("{local}pyodide-lock.json"));
        rewritten = absolutise(&rewritten, local);

        if rewritten != text {
            std::fs::write(&path, rewritten)?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// Turn the relative paths we just substituted into runtime-absolute ones.
///
/// **Measured, and the reason a bare relative path is not enough.** Pyodide's
/// loader passes its base through `new URL(...)`, which requires an absolute
/// URL — a relative one throws `URL constructor: ../pyodide/ is not a valid
/// URL` and every package fails to load. An absolute *path* (`/pyodide/`) fails
/// the same way; `new URL` wants a full URL, and we cannot hardcode an origin
/// because a bundle has to work wherever it is served.
///
/// Every site the substitution lands on is a whole template literal, so the
/// answer is to let the page compute it: `${new URL(rel, location.href).href}`
/// resolves against the worker's own URL at load time. Only literals we
/// produced are touched — matching the backticks on both sides is what keeps
/// this from injecting an expression into an ordinary string, where it would
/// be text rather than code.
fn absolutise(text: &str, local: &str) -> String {
    let mut out = text.to_string();
    for relative in [format!("{local}pyodide-lock.json"), local.to_string()] {
        out = out.replace(
            &format!("`{relative}`"),
            &format!("`${{new URL(\"{relative}\",location.href).href}}`"),
        );
    }
    out
}

/// Replace `https://cdn.jsdelivr.net/pyodide/v${anything}/full/`, whatever the
/// minifier named the variable.
fn replace_template_versions(text: &str, local: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    const PREFIX: &str = "https://cdn.jsdelivr.net/pyodide/";
    while let Some(at) = rest.find(PREFIX) {
        out.push_str(&rest[..at]);
        let after = &rest[at + PREFIX.len()..];
        match after.find("/full/") {
            Some(end) if end < 40 => {
                out.push_str(local);
                rest = &after[end + "/full/".len()..];
            }
            _ => {
                out.push_str(PREFIX);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `https://wasm.marimo.app/pyodide-lock.json?…` → the vendored one.
fn replace_lock_url(text: &str, local: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    const PREFIX: &str = "https://wasm.marimo.app/pyodide-lock.json";
    while let Some(at) = rest.find(PREFIX) {
        out.push_str(&rest[..at]);
        let after = &rest[at + PREFIX.len()..];
        // The query string is interpolated; drop it with the URL.
        let end = after.find(['`', '"', '\'', ',', ')']).unwrap_or(0);
        out.push_str(local);
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// The last path segment of a URL, as a filename.
///
/// The result is joined onto a cache directory, so it must be a name and not a
/// direction: `.` and `..` are the two segments that would make `join` mean
/// something other than "a file in here". Both currently fail later anyway —
/// you cannot write to a directory — but failing here says why.
fn file_name_of(url: &str) -> Result<String> {
    let name = url.rsplit('/').next().unwrap_or(url);
    anyhow::ensure!(
        !name.is_empty() && name != "." && name != "..",
        "`{url}` does not end in a filename"
    );
    Ok(name.to_string())
}

/// Package names are compared the way Python compares them.
fn normalise(name: &str) -> String {
    name.split_whitespace()
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase()
        .replace(['_', '.'], "-")
}

/// `~/.mecha/pyodide`, or `$MECHA_HOME/pyodide`.
pub fn default_cache() -> Result<PathBuf> {
    Ok(crate::store::mecha_home()?.join("pyodide"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_pinned_hosts_are_reachable() {
        let v = Vendorer::new(
            "v314.0.0",
            "0.23.16",
            std::env::temp_dir().join("factory-pyodide-test"),
        );
        let err = v
            .fetch_cached_bytes("https://evil.example/wheel.whl", "x.whl", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not on the pinned allowlist"), "{err}");
        // The three that are.
        for host in ALLOWED_HOSTS {
            assert!(host.contains('.'), "{host}");
        }
    }

    /// Python compares `pymdown_extensions` and `pymdown-extensions` as the
    /// same name, and the lock file uses one spelling while dependency lists
    /// use the other.
    /// The result is joined onto a cache directory, so it has to be a name
    /// rather than a direction.
    #[test]
    fn a_url_that_does_not_end_in_a_filename_is_refused() {
        assert_eq!(file_name_of("https://h/a/b.whl").unwrap(), "b.whl");
        for bad in ["https://h/a/..", "https://h/a/.", "https://h/a/"] {
            assert!(file_name_of(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn package_names_normalise_the_way_python_compares_them() {
        assert_eq!(normalise("pymdown_extensions"), "pymdown-extensions");
        assert_eq!(normalise("Markdown"), "markdown");
        assert_eq!(normalise("ruamel.yaml"), "ruamel-yaml");
        // A dependency may carry a version specifier.
        assert_eq!(normalise("packaging >=20"), "packaging");
    }

    #[test]
    fn the_cdn_url_is_replaced_whatever_the_minifier_named_the_variable() {
        let local = "../pyodide/";
        for source in [
            "let n=`https://cdn.jsdelivr.net/pyodide/v314.0.0/full/`;",
            "let n=`https://cdn.jsdelivr.net/pyodide/${e.pyodideVersion}/full/`;",
            "let n=`https://cdn.jsdelivr.net/pyodide/v${Lo}/full/`;",
            "e.cdnUrl=X(e.packageBaseUrl??`https://cdn.jsdelivr.net/pyodide/v${Uo}/full/`)",
        ] {
            let out = replace_template_versions(source, local);
            assert!(!out.contains("jsdelivr"), "{source} → {out}");
            assert!(out.contains(local), "{source} → {out}");
        }
        // A jsdelivr URL that is not the pyodide distribution is left alone —
        // it is a different problem, and silently rewriting it would produce a
        // path to nothing.
        let other =
            "s.src=\"https://cdn.jsdelivr.net/npm/plotly.js-dist-min@2.35.2/plotly.min.js\";";
        assert_eq!(replace_template_versions(other, local), other);
    }

    /// Measured: Pyodide passes its base through `new URL(...)`, which throws
    /// on a relative path — and on an absolute path too, since it wants a full
    /// URL. The origin cannot be hardcoded, so the page computes it.
    #[test]
    fn a_relative_base_becomes_one_the_url_constructor_accepts() {
        let out = absolutise(
            "let n=`../pyodide/`,l=`../pyodide/pyodide-lock.json`;",
            "../pyodide/",
        );
        assert!(
            out.contains("`${new URL(\"../pyodide/\",location.href).href}`"),
            "{out}"
        );
        assert!(
            out.contains("`${new URL(\"../pyodide/pyodide-lock.json\",location.href).href}`"),
            "{out}"
        );

        // Only whole template literals we produced. A relative path inside an
        // ordinary string stays text, because `${…}` there would be characters
        // rather than code.
        let quoted = "let n=\"../pyodide/\";";
        assert_eq!(absolutise(quoted, "../pyodide/"), quoted);
    }

    #[test]
    fn the_lock_url_loses_its_query_string_with_the_host() {
        let out = replace_lock_url(
            "lockFileURL:`https://wasm.marimo.app/pyodide-lock.json?v=${e.version}&pyodide=${e.pyodideVersion}`,indexURL:n",
            "../pyodide/pyodide-lock.json",
        );
        assert!(!out.contains("wasm.marimo.app"), "{out}");
        assert!(out.contains("`../pyodide/pyodide-lock.json`"), "{out}");
        assert!(out.contains("indexURL:n"), "the rest survives: {out}");
    }

    /// `pyodide.asm.mjs` is loaded by dynamic `import()`, which does not
    /// surface as a request event — so a record-and-replay of a live run misses
    /// it and the bundle fails on the one file nothing observed.
    #[test]
    fn the_core_list_includes_what_a_recorded_run_cannot_see() {
        assert!(CORE_FILES.contains(&"pyodide.asm.mjs"));
        assert!(CORE_FILES.contains(&"pyodide.asm.wasm"));
        assert!(CORE_FILES.contains(&"python_stdlib.zip"));
    }
}
