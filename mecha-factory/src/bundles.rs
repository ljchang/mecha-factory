//! Where published bytes live on the box, and the proof that a request stays
//! inside them.
//!
//! ```text
//! <data>/bundles/<id>/<version>/index.html   immutable, written once
//!                              /bundle.json  the manifest that arrived with it
//! ```
//!
//! Two levels, exactly as at home, so a mirrored bundle and a served one have
//! the same shape and a human debugging one is looking at the other. There is
//! no `latest` directory and no alias on disk: the alias is a row, because a
//! symlink is a second place for the answer to live and the two can disagree.
//!
//! **Every path a stranger supplies goes through [`Files::resolve`]**, which
//! canonicalizes and proves containment — the same rule mecha's path jail
//! follows, for the same reason. `GET /b/x/v/1/../../../etc/passwd` is the
//! first thing anyone tries, and it is refused rather than normalised away: a
//! path that climbs out and back in has still been interpreted, and a server
//! that answers it at all is one whose containment is a coincidence.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct Files {
    root: PathBuf,
}

impl Files {
    pub fn new(root: PathBuf) -> Result<Files> {
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Files {
            root: root
                .canonicalize()
                .with_context(|| format!("resolving {}", root.display()))?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn version_dir(&self, id: &str, version: u32) -> PathBuf {
        self.root.join(id).join(version.to_string())
    }

    /// Write a version's files, once.
    ///
    /// Into a temporary sibling and then renamed, so a reader never sees a
    /// half-written version and a failure part-way leaves no partial one —
    /// the same rule the home store follows, and it matters more here because
    /// the thing that might read it mid-write is the internet.
    ///
    /// An existing version directory is an error rather than an overwrite. It
    /// cannot happen through the API (the ledger's primary key refuses it
    /// first), and if it ever did it would mean a published version was about
    /// to change underneath a URL that promised it could not.
    pub fn install(&self, id: &str, version: u32, files: &[(String, Vec<u8>)]) -> Result<PathBuf> {
        let target = self.version_dir(id, version);
        if target.exists() {
            anyhow::bail!(
                "{} already exists; a version is written once and never rewritten",
                target.display()
            );
        }
        let staging = self.root.join(id).join(format!(".staging-{version}"));
        let _ = std::fs::remove_dir_all(&staging);
        for (relative, bytes) in files {
            // Containment again, because a path that survived the unpacker is
            // still a path this process is about to write — the one check
            // between "we validated it" and "we wrote it".
            //
            // Built from components rather than checked with `starts_with`,
            // which is **lexical**: `<staging>/../escape.html` starts with
            // `<staging>` by that test and lands one directory up. A test
            // caught exactly that here, which is the argument for the test.
            let mut path = staging.clone();
            for part in std::path::Path::new(relative).components() {
                match part {
                    std::path::Component::Normal(name) => path.push(name),
                    std::path::Component::CurDir => {}
                    _ => {
                        let _ = std::fs::remove_dir_all(&staging);
                        anyhow::bail!("`{relative}` would be written outside the bundle");
                    }
                }
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, bytes)?;
        }
        std::fs::rename(&staging, &target)
            .with_context(|| format!("installing version {version} of {id}"))?;
        Ok(target)
    }

    /// The file a request names inside one version, or nothing.
    ///
    /// `path` is the remainder after `/b/<id>/v/<n>/`, already percent-decoded
    /// by the router. An empty path, or one naming a directory, resolves to
    /// that directory's `index.html` — a bundle is a page, not a listing, and
    /// there is deliberately no directory index.
    pub fn resolve(&self, id: &str, version: u32, path: &str) -> Option<PathBuf> {
        let base = self.version_dir(id, version);
        let mut candidate = base.clone();
        for part in path.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            // Refused outright rather than normalised: see the module docs.
            if part == ".." {
                return None;
            }
            candidate.push(part);
        }
        if candidate.is_dir() {
            candidate.push("index.html");
        }
        let real = candidate.canonicalize().ok()?;
        // Containment is proved against the canonical version directory, so a
        // symlink planted inside a bundle cannot serve bytes from outside it.
        let base = base.canonicalize().ok()?;
        (real.starts_with(&base) && real.is_file()).then_some(real)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, Files) {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::new(dir.path().join("bundles")).unwrap();
        let v1 = files.version_dir("brief", 1);
        std::fs::create_dir_all(v1.join("assets")).unwrap();
        std::fs::write(v1.join("index.html"), "<h1>Monday</h1>").unwrap();
        std::fs::write(v1.join("assets/style.css"), "body{}").unwrap();
        std::fs::create_dir_all(v1.join("sub")).unwrap();
        std::fs::write(v1.join("sub/index.html"), "nested").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "outside").unwrap();
        (dir, files)
    }

    #[test]
    fn a_bundle_serves_its_own_files_and_a_directory_means_its_index() {
        let (_dir, files) = scratch();
        assert!(files
            .resolve("brief", 1, "")
            .unwrap()
            .ends_with("index.html"));
        assert!(files
            .resolve("brief", 1, "/")
            .unwrap()
            .ends_with("index.html"));
        assert!(files
            .resolve("brief", 1, "assets/style.css")
            .unwrap()
            .ends_with("style.css"));
        assert!(files
            .resolve("brief", 1, "sub")
            .unwrap()
            .ends_with("sub/index.html"));
        assert!(files.resolve("brief", 1, "nope.html").is_none());
        assert!(files.resolve("brief", 2, "").is_none(), "no such version");
        assert!(files.resolve("other", 1, "").is_none(), "no such bundle");
    }

    /// The first thing anyone tries, in the spellings they try it in. The
    /// router percent-decodes before this sees it, which is why the decoded
    /// forms are the ones tested.
    #[test]
    fn nothing_climbs_out() {
        let (dir, files) = scratch();
        for path in [
            "../../secret.txt",
            "../../../secret.txt",
            "a/../../../secret.txt",
            "..",
            "assets/../../../secret.txt",
        ] {
            assert!(files.resolve("brief", 1, path).is_none(), "{path} resolved");
        }
        // Not vacuous: the file it was reaching for is really there.
        assert!(dir.path().join("secret.txt").is_file());
    }

    #[test]
    fn a_version_is_installed_once_and_never_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::new(dir.path().join("bundles")).unwrap();
        let content = vec![
            ("index.html".to_string(), b"<h1>Monday</h1>".to_vec()),
            ("assets/app.js".to_string(), b"console.log(1)".to_vec()),
        ];
        files.install("brief", 1, &content).unwrap();
        assert_eq!(
            std::fs::read_to_string(files.resolve("brief", 1, "").unwrap()).unwrap(),
            "<h1>Monday</h1>"
        );

        let err = files.install("brief", 1, &content).unwrap_err().to_string();
        assert!(err.contains("never rewritten"), "{err}");
    }

    /// A failure part-way must leave no partial version, because the thing that
    /// might read one mid-write is the internet.
    #[test]
    fn a_staging_directory_is_never_visible_as_a_version() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::new(dir.path().join("bundles")).unwrap();
        // A path that survived validation but would still write outside.
        let content = vec![("../escape.html".to_string(), b"x".to_vec())];
        assert!(files.install("brief", 1, &content).is_err());
        assert!(!files.version_dir("brief", 1).exists());
        assert!(!files.root().join("brief/.staging-1").exists());
    }

    /// A symlink planted inside a published bundle would otherwise serve bytes
    /// from outside it — including, on this box, the ledger.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_bundle_serves_nothing() {
        let (dir, files) = scratch();
        let link = files.version_dir("brief", 1).join("escape.txt");
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), &link).unwrap();
        assert!(link.exists(), "the link is really there");
        assert!(files.resolve("brief", 1, "escape.txt").is_none());
    }
}
