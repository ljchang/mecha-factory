//! The bundle store: immutable versions, one moving alias.
//!
//! ```text
//! ~/.mecha/bundles/<id>/1/          a version — immutable, never deleted
//!                      /2/          …and its bundle.json
//!                      /alias.json  the only moving part
//!                      /index.html  a redirect to wherever the alias points
//! ```
//!
//! The four properties that make a published artifact worth having, and how
//! each is obtained here:
//!
//! | Property | How |
//! |---|---|
//! | Permanent | a version directory is written once and never rewritten or removed |
//! | Versioned | `<id>/<n>/` addresses one forever |
//! | A stable share URL | `<id>/` redirects through the alias |
//! | Readable by the agent | it is an ordinary directory under the mecha home |
//!
//! **A version is content-addressed**, and that is what makes "did anything
//! actually change?" a comparison rather than a guess: the digest is taken over
//! the bundle's bytes, and re-publishing identical bytes returns the existing
//! version instead of minting a new one. A nightly briefing that produced the
//! same page twice costs one row, and an outbox that staged the same publish
//! twice — staging takes no lock, deliberately, so a retried tool call stages
//! twice — collapses to one version rather than to two identical ones.
//!
//! **There is no delete.** Taking something down moves the alias to nothing and
//! flips the visibility flag; the versions stay. A delete verb would be the one
//! operation that could destroy the record, and everything else here is built
//! so that nothing can.
//!
//! **The layout is a contract with mecha**, not just an arrangement: `mecha work
//! clean` reads `<id>/<version>/bundle.json` for a `sources` array and refuses
//! to remove anything named in one. Two levels, exactly — inserting a `v/`
//! directory here would silently break that, and the failure mode is a
//! retention sweep deleting the input of a published report.

use anyhow::{bail, Context, Result};
use mecha_manifest::{BundleManifest, ContentClass, Visibility};
use std::path::{Path, PathBuf};

/// `~/.mecha`, or `$MECHA_HOME`.
///
/// Duplicated from mecha rather than shared, because this repository may not
/// depend on `mecha-core` — the whole credential-isolation claim rests on that,
/// and eleven lines is a cheap price for a property you can verify by looking.
pub fn mecha_home() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("MECHA_HOME") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = std::env::var("HOME").context("neither MECHA_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".mecha"))
}

pub struct BundleStore {
    root: PathBuf,
}

/// Where the alias points, and who may read the bundle.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Alias {
    /// `None` after an unpublish. The versions are still there; nothing points
    /// at them.
    pub version: Option<u32>,
    #[serde(default)]
    pub visibility: Visibility,
    pub updated_at: String,
}

/// What a publish did. `existing` is the interesting field: it says the bytes
/// were identical to a version already stored, so nothing was minted.
#[derive(Debug)]
pub struct Published {
    pub id: String,
    pub version: u32,
    pub digest: String,
    pub existing: bool,
    pub path: PathBuf,
}

impl BundleStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(BundleStore { root })
    }

    pub fn open_default() -> Result<Self> {
        Self::open(mecha_home()?.join("bundles"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn bundle_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn version_dir(&self, id: &str, version: u32) -> PathBuf {
        self.bundle_dir(id).join(version.to_string())
    }

    /// Every version of one bundle, ascending.
    pub fn versions(&self, id: &str) -> Result<Vec<u32>> {
        let dir = self.bundle_dir(id);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            {
                out.push(n);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Every bundle with at least one version, alphabetically.
    pub fn bundles(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if !self.root.is_dir() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if !self.versions(name)?.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn manifest(&self, id: &str, version: u32) -> Result<BundleManifest> {
        let path = self.version_dir(id, version).join("bundle.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(BundleManifest::from_json(&text)?)
    }

    pub fn alias(&self, id: &str) -> Result<Option<Alias>> {
        let path = self.bundle_dir(id).join("alias.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text)
                    .with_context(|| format!("reading {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Copy a rendered directory in as a new immutable version.
    ///
    /// Returns the existing version, unchanged and unrewritten, when the bytes
    /// digest to something already stored.
    #[allow(clippy::too_many_arguments)]
    pub fn publish(
        &self,
        id: &str,
        rendered: &Path,
        title: &str,
        description: Option<String>,
        template: &str,
        class: ContentClass,
        sources: Vec<PathBuf>,
        now: &str,
    ) -> Result<Published> {
        if !rendered.is_dir() {
            bail!("{} is not a directory", rendered.display());
        }
        let files = collect(rendered)?;
        if files.is_empty() {
            bail!(
                "{} contains no files — publishing an empty bundle would move \
                 the alias to a blank page",
                rendered.display()
            );
        }
        // The digest is taken over the rendered bytes only. `bundle.json`
        // carries the digest, so including it would be circular — and it also
        // carries a timestamp, which would make every publish of identical
        // content a new version and defeat the whole point.
        let digest = digest_of(&files);

        for version in self.versions(id)? {
            if self.manifest(id, version)?.digest.as_deref() == Some(digest.as_str()) {
                return Ok(Published {
                    id: id.to_string(),
                    version,
                    digest,
                    existing: true,
                    path: self.version_dir(id, version),
                });
            }
        }

        let version = self.versions(id)?.last().copied().unwrap_or(0) + 1;
        let dir = self.version_dir(id, version);
        if dir.exists() {
            // Cannot happen through this API, and if it ever did it would mean
            // a version was about to be rewritten — which is the one thing the
            // store promises never happens.
            bail!(
                "{} already exists; a version is written once and never rewritten",
                dir.display()
            );
        }
        // Written to a temporary sibling and renamed, so a reader never sees a
        // half-written version and a failure part-way leaves no partial one.
        let staging = self.bundle_dir(id).join(format!(".staging-{version}"));
        let _ = std::fs::remove_dir_all(&staging);
        for (relative, bytes) in &files {
            let target = staging.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, bytes)?;
        }

        let manifest = BundleManifest {
            id: id.to_string(),
            version,
            title: title.to_string(),
            description,
            template: template.to_string(),
            class,
            // Recorded on the version, but the alias is what a reader is
            // actually gated by — see `set_alias`.
            visibility: self
                .alias(id)?
                .map(|a| a.visibility)
                .unwrap_or(Visibility::Private),
            digest: Some(digest.clone()),
            published_at: Some(now.to_string()),
            sources,
        };
        manifest.check()?;
        std::fs::write(staging.join("bundle.json"), manifest.to_json())?;
        std::fs::rename(&staging, &dir)
            .with_context(|| format!("installing version {version} of {id}"))?;

        Ok(Published {
            id: id.to_string(),
            version,
            digest,
            existing: false,
            path: dir,
        })
    }

    /// Point the share URL at a version, or at nothing.
    ///
    /// Moving an alias changes what every existing share link resolves to, which
    /// is why it is a publication rather than bookkeeping — and why mecha routes
    /// it through the outbox like a publish.
    pub fn set_alias(
        &self,
        id: &str,
        version: Option<u32>,
        visibility: Visibility,
        now: &str,
    ) -> Result<()> {
        if let Some(version) = version {
            if !self.version_dir(id, version).is_dir() {
                bail!("{id} has no version {version}");
            }
        }
        let alias = Alias {
            version,
            visibility,
            updated_at: now.to_string(),
        };
        let dir = self.bundle_dir(id);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("alias.json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&alias)?)?;
        std::fs::rename(&tmp, &path)?;
        self.write_redirect(id, version)
    }

    /// A dumb static server has no routing table, so `<id>/index.html` is how
    /// `<id>/` resolves through the alias. Regenerated on every alias move; a
    /// stale one would send readers to a superseded version, which is the bug
    /// aliasing exists to prevent.
    fn write_redirect(&self, id: &str, version: Option<u32>) -> Result<()> {
        let path = self.bundle_dir(id).join("index.html");
        let html = match version {
            Some(version) => format!(
                "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
                 <meta http-equiv=\"refresh\" content=\"0; url=./{version}/\">\n\
                 <title>{}</title></head>\n\
                 <body><p><a href=\"./{version}/\">{} — version {version}</a></p></body></html>\n",
                escape(id),
                escape(id)
            ),
            // Unpublished. An honest page rather than a 404, because the reader
            // followed a link somebody sent them and "gone" is more useful than
            // "broken".
            None => format!(
                "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n\
                 <title>{}</title></head>\n\
                 <body><p>This has been taken down.</p></body></html>\n",
                escape(id)
            ),
        };
        std::fs::write(&path, html)?;
        Ok(())
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A digest over a directory, by the same rule a bundle version uses.
///
/// Exposed because the vendoring gate needs it: a vendored third-party tree is
/// reviewed once at a pinned version and identified by its digest thereafter,
/// and it has to be the *same* digest the store would compute or the two would
/// disagree about whether anything changed.
pub fn digest_tree(dir: &Path) -> Result<String> {
    Ok(digest_of(&collect(dir)?))
}

/// Every file under `dir`, as (path relative to `dir`, bytes), sorted by path.
///
/// Sorted because the digest is taken over this list and directory iteration
/// order is not stable across filesystems — an unsorted walk would make the
/// same bytes digest differently on two machines, and content addressing would
/// silently stop collapsing duplicates.
fn collect(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk(root, &path, out)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("walked from root")
                .to_string_lossy()
                .replace('\\', "/");
            // A rendered directory that already carries a manifest is one being
            // re-published; the store writes its own, so the old one must not
            // be copied in. (`digest_files` excludes it from the address on its
            // own, on both sides of the wire — this is about the bytes that
            // land in the version directory.)
            if relative == mecha_manifest::MANIFEST_FILE {
                continue;
            }
            out.push((relative, std::fs::read(&path)?));
        }
        // Symlinks are neither followed nor copied: a link out of the bundle
        // would publish bytes from outside it, and a link within it survives
        // as nothing rather than as a dangling reference.
    }
    Ok(())
}

/// A digest over the whole bundle.
///
/// The rule itself lives in `mecha-manifest` because the **server** computes it
/// too, over the same bundle after it crossed a network. One definition, or the
/// two ends eventually disagree about whether anything changed.
fn digest_of(files: &[(String, Vec<u8>)]) -> String {
    mecha_manifest::digest_files(files.iter().map(|(p, b)| (p.as_str(), b.as_slice())))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "factory-store-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rendered(dir: &Path, body: &str) -> PathBuf {
        let out = dir.join("rendered");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(out.join("assets")).unwrap();
        std::fs::write(out.join("index.html"), body).unwrap();
        std::fs::write(out.join("assets/style.css"), "body{}").unwrap();
        out
    }

    fn publish(store: &BundleStore, src: &Path, now: &str) -> Published {
        store
            .publish(
                "brief",
                src,
                "Morning briefing",
                None,
                "report",
                ContentClass::Static,
                vec![],
                now,
            )
            .unwrap()
    }

    #[test]
    fn a_version_is_written_once_and_addressed_by_its_content() {
        let scratch = Scratch::new("content");
        let store = BundleStore::open(scratch.path().join("bundles")).unwrap();
        let src = rendered(scratch.path(), "<h1>Monday</h1>");

        let first = publish(&store, &src, "2026-08-06T07:00:00Z");
        assert_eq!(first.version, 1);
        assert!(!first.existing);

        // Identical bytes, a later timestamp: the same version, and nothing
        // rewritten. This is what makes "did anything change?" a comparison.
        let again = publish(&store, &src, "2026-08-07T07:00:00Z");
        assert_eq!(again.version, 1);
        assert!(again.existing);
        assert_eq!(again.digest, first.digest);
        assert_eq!(store.versions("brief").unwrap(), vec![1]);
        assert_eq!(
            store.manifest("brief", 1).unwrap().published_at.unwrap(),
            "2026-08-06T07:00:00Z",
            "the stored version was not touched"
        );

        // Different bytes: a new version, and version 1 still exactly as it was.
        let src2 = rendered(scratch.path(), "<h1>Tuesday</h1>");
        let second = publish(&store, &src2, "2026-08-07T07:00:00Z");
        assert_eq!(second.version, 2);
        assert!(!second.existing);
        assert_ne!(second.digest, first.digest);
        assert_eq!(
            std::fs::read_to_string(store.version_dir("brief", 1).join("index.html")).unwrap(),
            "<h1>Monday</h1>",
            "an immutable version stayed immutable"
        );
    }

    /// A path and a file's contents both feed the digest, so their boundary has
    /// to be unambiguous or two different bundles share a version.
    #[test]
    fn the_digest_cannot_confuse_a_path_boundary_with_a_content_boundary() {
        let a = digest_of(&[("ab".into(), b"c".to_vec())]);
        let b = digest_of(&[("a".into(), b"bc".to_vec())]);
        assert_ne!(a, b);
    }

    #[test]
    fn the_alias_is_the_only_moving_part_and_a_takedown_destroys_nothing() {
        let scratch = Scratch::new("alias");
        let store = BundleStore::open(scratch.path().join("bundles")).unwrap();
        publish(
            &store,
            &rendered(scratch.path(), "one"),
            "2026-08-06T00:00:00Z",
        );
        publish(
            &store,
            &rendered(scratch.path(), "two"),
            "2026-08-07T00:00:00Z",
        );

        store
            .set_alias(
                "brief",
                Some(2),
                Visibility::Private,
                "2026-08-07T00:00:01Z",
            )
            .unwrap();
        assert_eq!(store.alias("brief").unwrap().unwrap().version, Some(2));
        let redirect =
            std::fs::read_to_string(store.bundle_dir("brief").join("index.html")).unwrap();
        assert!(
            redirect.contains("url=./2/"),
            "the share URL follows the alias"
        );

        // A takedown: the alias points at nothing, and both versions remain.
        store
            .set_alias("brief", None, Visibility::Private, "2026-08-08T00:00:00Z")
            .unwrap();
        assert_eq!(store.alias("brief").unwrap().unwrap().version, None);
        assert_eq!(store.versions("brief").unwrap(), vec![1, 2]);
        let gone = std::fs::read_to_string(store.bundle_dir("brief").join("index.html")).unwrap();
        assert!(gone.contains("taken down"));
        assert!(
            !gone.contains("url=./"),
            "a stale redirect would still resolve"
        );
    }

    #[test]
    fn aliasing_a_version_that_does_not_exist_is_refused() {
        let scratch = Scratch::new("badalias");
        let store = BundleStore::open(scratch.path().join("bundles")).unwrap();
        publish(
            &store,
            &rendered(scratch.path(), "one"),
            "2026-08-06T00:00:00Z",
        );
        let err = store
            .set_alias(
                "brief",
                Some(7),
                Visibility::Private,
                "2026-08-06T00:00:01Z",
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("no version 7"), "{err}");
    }

    /// The contract `mecha work clean` reads. Two levels exactly, and the
    /// sources absolute — a relative path would match nothing and the retention
    /// sweep would delete the input of a published report.
    #[test]
    fn the_layout_and_the_sources_array_are_what_retention_reads() {
        let scratch = Scratch::new("sources");
        let store = BundleStore::open(scratch.path().join("bundles")).unwrap();
        let source = scratch.path().join("2026-08-06.md");
        std::fs::write(&source, "# hello").unwrap();

        store
            .publish(
                "brief",
                &rendered(scratch.path(), "x"),
                "Morning briefing",
                None,
                "report",
                ContentClass::Static,
                vec![source.clone()],
                "2026-08-06T00:00:00Z",
            )
            .unwrap();

        // Exactly the walk mecha performs: <root>/<id>/<version>/bundle.json.
        let mut found = Vec::new();
        for bundle in std::fs::read_dir(store.root()).unwrap().flatten() {
            let Ok(versions) = std::fs::read_dir(bundle.path()) else {
                continue;
            };
            for version in versions.flatten() {
                let manifest = version.path().join("bundle.json");
                if let Ok(text) = std::fs::read_to_string(&manifest) {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    for s in value["sources"].as_array().unwrap() {
                        found.push(PathBuf::from(s.as_str().unwrap()));
                    }
                }
            }
        }
        assert_eq!(found, vec![source], "retention would not have found it");
    }

    #[test]
    fn an_empty_bundle_is_refused_rather_than_aliased_to_a_blank_page() {
        let scratch = Scratch::new("empty");
        let store = BundleStore::open(scratch.path().join("bundles")).unwrap();
        let empty = scratch.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let err = store
            .publish(
                "brief",
                &empty,
                "t",
                None,
                "report",
                ContentClass::Static,
                vec![],
                "2026-08-06T00:00:00Z",
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("no files"), "{err}");
    }

    /// Re-publishing a directory that a previous publish wrote a manifest into
    /// must digest to the same thing, or the timestamp inside it would make
    /// every republish a new version.
    #[test]
    fn a_stale_manifest_in_the_source_is_neither_copied_nor_digested() {
        let scratch = Scratch::new("stale");
        let store = BundleStore::open(scratch.path().join("bundles")).unwrap();
        let src = rendered(scratch.path(), "same");
        let first = publish(&store, &src, "2026-08-06T00:00:00Z");

        std::fs::write(
            src.join("bundle.json"),
            r#"{"id":"someone-else","version":99,"title":"x","template":"report","class":"static"}"#,
        )
        .unwrap();
        let again = publish(&store, &src, "2026-08-07T00:00:00Z");
        assert_eq!(again.digest, first.digest);
        assert!(again.existing);
        assert_eq!(store.manifest("brief", 1).unwrap().version, 1);
    }
}
