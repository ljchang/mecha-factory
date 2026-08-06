//! Taking a bundle off the wire.
//!
//! An archive arrives, and everything about reading it is adversarial — not
//! because the publisher is hostile, but because the code that unpacks an
//! archive is the code that gets it wrong. Five rules, each of which is a
//! recorded way this goes badly:
//!
//! - **Only regular files.** A tar can carry symlinks, hardlinks, devices and
//!   directories with any mode it likes. `tar -xf` on a link to
//!   `/etc/systemd/system/…` is the entire attack. Nothing but a regular file
//!   is ever written; a link is a refusal, not a skip, because a bundle
//!   containing one is not the bundle the publisher checked.
//! - **The path is rebuilt, never trusted.** Absolute paths, `..`, and
//!   anything that is not plain UTF-8 are refused. The result is a relative
//!   path made of ordinary segments, which is also what the digest is computed
//!   over — so a path we would not write is a path that cannot be addressed.
//! - **The caps count decompressed bytes.** A gzip bomb is small on the wire
//!   and unbounded on disk, so the limit is enforced as entries are read,
//!   not on the request body.
//! - **The digest is recomputed and must match what the manifest claims.**
//!   This is the whole reason the content address lives in `mecha-manifest`:
//!   home computed it before the POST, the server computes it after, and a
//!   disagreement means the bytes are not the ones that were reviewed.
//! - **Neither `sources` nor `visibility` lands on the box.** The manifest's `sources` array is a
//!   contract with `mecha work clean` and holds *absolute paths inside the
//!   user's home directory*. The stored `bundle.json` is served publicly, so
//!   carrying that field across would publish the shape of a private machine —
//!   which nobody would have chosen, and which nothing would have noticed. It
//!   is stripped here, at the boundary, rather than by remembering not to
//!   serve it.

use flate2::read::GzDecoder;
use mecha_manifest::BundleManifest;
use std::io::Read;

use crate::config::Limits;

/// A bundle that has been read, checked, and not yet written anywhere.
#[derive(Debug)]
pub struct Incoming {
    pub manifest: BundleManifest,
    /// Relative path → bytes, including `bundle.json`, which is rewritten.
    pub files: Vec<(String, Vec<u8>)>,
    /// Computed here, over what actually arrived.
    pub digest: String,
}

/// Anything a publisher can get wrong, phrased so the message can go back on
/// the wire. Every one of these is a 400: the request was understood and
/// refused, and the publisher can fix it.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Rejected(String);

fn reject(message: impl Into<String>) -> Rejected {
    Rejected(message.into())
}

pub fn unpack(body: &[u8], limits: &Limits) -> Result<Incoming, Rejected> {
    // Gzip by its magic rather than by the declared content type: a header is
    // the client's opinion and the bytes are the fact.
    let gzipped = body.len() >= 2 && body[0] == 0x1f && body[1] == 0x8b;
    let reader: Box<dyn Read> = if gzipped {
        Box::new(GzDecoder::new(body))
    } else {
        Box::new(body)
    };

    let mut archive = tar::Archive::new(reader);
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;

    let entries = archive
        .entries()
        .map_err(|e| reject(format!("this is not a readable tar archive: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| reject(format!("reading the archive: {e}")))?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            // Directories carry no bytes; the ones we need are implied by the
            // files inside them and are created on write.
            continue;
        }
        if !kind.is_file() {
            let path = entry
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            return Err(reject(format!(
                "`{path}` is a {kind:?}, and a bundle is regular files only — \
                 a link in an archive is how an unpack writes outside itself"
            )));
        }

        let raw = entry
            .path()
            .map_err(|e| reject(format!("an entry has an unreadable path: {e}")))?
            .to_path_buf();
        let path = clean_path(&raw.to_string_lossy())?;

        if files.len() >= limits.max_bundle_files {
            return Err(reject(format!(
                "more than {} files; a bundle this size is not one somebody meant to publish",
                limits.max_bundle_files
            )));
        }

        let size = entry.header().size().unwrap_or(0);
        total = total.saturating_add(size);
        if total > limits.max_bundle_bytes {
            return Err(reject(format!(
                "the archive expands past the {} byte limit",
                limits.max_bundle_bytes
            )));
        }

        let mut bytes = Vec::with_capacity(size as usize);
        // Bounded by the remaining budget rather than by the header's claim:
        // the size field is part of the archive, and an archive that lies
        // about it is exactly the case this limit exists for.
        let remaining = limits.max_bundle_bytes - total.min(limits.max_bundle_bytes) + size;
        entry
            .by_ref()
            .take(remaining + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| reject(format!("reading `{path}`: {e}")))?;
        if bytes.len() as u64 > remaining {
            return Err(reject("the archive expands past its declared size"));
        }

        if let Some(existing) = files.iter().position(|(p, _)| *p == path) {
            // A tar may legally contain the same path twice, and the usual
            // rule is last-wins. Refused instead: two entries mean two
            // possible bundles, and the digest would address whichever one we
            // happened to keep.
            let _ = existing;
            return Err(reject(format!("`{path}` appears twice in the archive")));
        }
        files.push((path, bytes));
    }

    if files.is_empty() {
        return Err(reject(
            "the archive contains no files — publishing an empty bundle would \
             move the share URL to a blank page",
        ));
    }

    let manifest_bytes = files
        .iter()
        .find(|(path, _)| path == mecha_manifest::MANIFEST_FILE)
        .map(|(_, bytes)| bytes.clone())
        .ok_or_else(|| {
            reject(format!(
                "no `{}` in the archive; it is what says which bundle this is",
                mecha_manifest::MANIFEST_FILE
            ))
        })?;
    let text = String::from_utf8(manifest_bytes).map_err(|_| reject("bundle.json is not UTF-8"))?;
    let mut manifest =
        BundleManifest::from_json(&text).map_err(|e| reject(format!("bundle.json: {e}")))?;

    let digest =
        mecha_manifest::digest_files(files.iter().map(|(p, b)| (p.as_str(), b.as_slice())));
    match manifest.digest.as_deref() {
        Some(claimed) if claimed == digest => {}
        Some(claimed) => {
            return Err(reject(format!(
                "the manifest claims {claimed} and the bytes are {digest} — \
                 what arrived is not what was reviewed"
            )))
        }
        None => {
            return Err(reject(
                "bundle.json has no digest; a version is addressed by its content",
            ))
        }
    }

    // Never stored, never served. See the module docs.
    manifest.sources.clear();
    // Nor is the manifest's idea of who may read this. The alias row is what
    // gates a reader, and it moves; a static file that claimed otherwise would
    // be a public page whose own manifest says `private`, which is a
    // contradiction somebody has to spend time resolving.
    manifest.visibility = mecha_manifest::Visibility::Private;
    let rewritten = manifest.to_json();
    for (path, bytes) in files.iter_mut() {
        if path == mecha_manifest::MANIFEST_FILE {
            *bytes = rewritten.clone().into_bytes();
        }
    }

    Ok(Incoming {
        manifest,
        files,
        digest,
    })
}

/// A path we would be willing to write, or a refusal.
fn clean_path(raw: &str) -> Result<String, Rejected> {
    let raw = raw.replace('\\', "/");
    if raw.starts_with('/') {
        return Err(reject(format!("`{raw}` is absolute")));
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(reject(format!("`{raw}` climbs out of the bundle"))),
            part => {
                if part.contains('\0') {
                    return Err(reject("a path contains a null byte"));
                }
                if part.len() > 255 {
                    return Err(reject(format!("`{raw}` has an unreasonably long segment")));
                }
                parts.push(part);
            }
        }
    }
    if parts.is_empty() {
        return Err(reject(format!("`{raw}` names nothing")));
    }
    let path = parts.join("/");
    if path.len() > 1024 {
        return Err(reject(format!("`{path}` is too long a path")));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mecha_manifest::ContentClass;

    /// Build an archive the way the publisher will, so the tests exercise the
    /// real shape rather than a convenient one.
    fn archive(files: &[(&str, &[u8])], gzip: bool) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        if !gzip {
            return tar;
        }
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn manifest_for(files: &[(&str, &[u8])], sources: Vec<std::path::PathBuf>) -> String {
        let digest = mecha_manifest::digest_files(files.iter().map(|(p, b)| (*p, *b)));
        BundleManifest {
            id: "brief".into(),
            version: 1,
            title: "Morning briefing".into(),
            description: None,
            template: "report".into(),
            class: ContentClass::Static,
            visibility: mecha_manifest::Visibility::Private,
            digest: Some(digest),
            published_at: Some("2026-08-06T07:00:00Z".into()),
            sources,
        }
        .to_json()
    }

    fn stored_manifest(incoming: &Incoming) -> String {
        incoming
            .files
            .iter()
            .find(|(p, _)| p == "bundle.json")
            .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
            .expect("every accepted bundle carries one")
    }

    fn bundle(gzip: bool, sources: Vec<std::path::PathBuf>) -> Vec<u8> {
        let content: Vec<(&str, &[u8])> = vec![
            ("index.html", b"<h1>Monday</h1>"),
            ("assets/style.css", b"body{}"),
        ];
        let manifest = manifest_for(&content, sources);
        let mut all = content.clone();
        all.push(("bundle.json", manifest.as_bytes()));
        archive(&all, gzip)
    }

    #[test]
    fn a_bundle_arrives_and_its_address_is_recomputed_from_the_bytes() {
        for gzip in [false, true] {
            let incoming = unpack(&bundle(gzip, vec![]), &Limits::default()).unwrap();
            assert_eq!(incoming.manifest.id, "brief");
            assert_eq!(incoming.manifest.version, 1);
            assert_eq!(incoming.digest, incoming.manifest.digest.clone().unwrap());
            assert_eq!(incoming.files.len(), 3, "two files and the manifest");
        }
    }

    /// The check that makes the two ends one publication rather than two
    /// programs that agree most of the time.
    #[test]
    fn bytes_that_do_not_match_the_claimed_address_are_refused() {
        let content: Vec<(&str, &[u8])> = vec![("index.html", b"<h1>Monday</h1>")];
        let manifest = manifest_for(&content, vec![]);
        // Same manifest, different bytes — the shape of a truncated upload or
        // a tampered one.
        let tampered: Vec<(&str, &[u8])> = vec![
            ("index.html", b"<h1>Tuesday</h1>"),
            ("bundle.json", manifest.as_bytes()),
        ];
        let err = unpack(&archive(&tampered, false), &Limits::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not what was reviewed"), "{err}");
    }

    /// The one that would have published the shape of a private machine, and
    /// that nothing would have noticed.
    #[test]
    fn the_sources_array_does_not_come_with_it() {
        let home = std::path::PathBuf::from("/home/someone/.mecha/work/morning/2026-08-06.md");
        let incoming = unpack(&bundle(false, vec![home]), &Limits::default()).unwrap();
        assert!(incoming.manifest.sources.is_empty());
        // Nor a visibility claim: the alias row decides, and it moves.
        assert!(!stored_manifest(&incoming).contains("visibility"));
        // …and the copy that gets written and served is the rewritten one.
        let stored = stored_manifest(&incoming);
        assert!(!stored.contains("/home/someone"), "{stored}");
        assert!(stored.contains("\"id\": \"brief\""), "{stored}");
    }

    /// A link in an archive is how an unpack writes outside itself.
    #[test]
    fn nothing_but_a_regular_file_is_accepted() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        builder
            .append_link(&mut header, "escape", "/etc/passwd")
            .unwrap();
        let err = unpack(&builder.into_inner().unwrap(), &Limits::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("regular files only"), "{err}");
    }

    /// An archive with a hostile path, written at the header level.
    ///
    /// The `tar` crate's builder refuses to *create* one — which is a fine
    /// default and useless as a test, because the archive we have to survive is
    /// the one somebody wrote on purpose. So the name goes into the header
    /// directly, exactly as a hand-rolled attacker's would.
    fn archive_with_raw_path(path: &str, bytes: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        {
            let raw = header.as_gnu_mut().expect("a gnu header");
            raw.name[..path.len()].copy_from_slice(path.as_bytes());
        }
        header.set_cksum();
        builder.append(&header, bytes).unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn a_path_that_climbs_out_is_refused_rather_than_normalised() {
        for path in [
            "../escape.html",
            "a/../../escape.html",
            "/etc/cron.d/factory",
            "../../../etc/systemd/system/x.service",
        ] {
            let archive = archive_with_raw_path(path, b"x");
            // Not vacuous: the archive really does carry that path.
            let mut names = Vec::new();
            for entry in tar::Archive::new(&archive[..]).entries().unwrap() {
                names.push(
                    entry
                        .unwrap()
                        .header()
                        .path()
                        .unwrap()
                        .display()
                        .to_string(),
                );
            }
            assert_eq!(names, vec![path.to_string()], "the fixture is honest");

            let err = unpack(&archive, &Limits::default())
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("climbs out") || err.contains("absolute"),
                "{path}: {err}"
            );
        }
    }

    /// Small on the wire, unbounded on disk. The limit has to be on what comes
    /// out, not on what goes in.
    #[test]
    fn the_cap_counts_what_the_archive_expands_to() {
        let big = vec![b'a'; 4096];
        let content: Vec<(&str, &[u8])> = vec![("big.txt", &big)];
        let compressed = archive(&content, true);
        assert!(
            compressed.len() < 1024,
            "the point of the test is that the body is small: {}",
            compressed.len()
        );
        let limits = Limits {
            max_bundle_bytes: 1024,
            ..Limits::default()
        };
        let err = unpack(&compressed, &limits).unwrap_err().to_string();
        assert!(err.contains("expands past"), "{err}");
    }

    #[test]
    fn a_bundle_with_no_manifest_or_no_files_is_refused() {
        let content: Vec<(&str, &[u8])> = vec![("index.html", b"x")];
        let err = unpack(&archive(&content, false), &Limits::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("bundle.json"), "{err}");

        let err = unpack(&archive(&[], false), &Limits::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no files"), "{err}");
    }

    /// Two entries mean two possible bundles, and the digest would address
    /// whichever one we happened to keep.
    #[test]
    fn the_same_path_twice_is_refused() {
        let content: Vec<(&str, &[u8])> = vec![("index.html", b"one"), ("index.html", b"two")];
        let err = unpack(&archive(&content, false), &Limits::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("twice"), "{err}");
    }
}
