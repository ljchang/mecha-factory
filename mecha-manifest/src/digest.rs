//! The content address of a bundle — and it is part of the **contract**, not
//! the publisher's private business.
//!
//! Two machines compute this over the same bytes and have to agree: the
//! publisher digests a rendered directory to decide whether anything changed,
//! and the server digests what arrived on the wire to decide whether it is
//! the version the publisher says it is. A digest computed two ways is a
//! disagreement waiting for the worst moment to happen — the server minting a
//! second copy of a report that did not change, or accepting an upload as a
//! version it is not. So the definition lives here, beside the schema, with
//! the rest of what both ends must read identically.
//!
//! Three rules, each of which is a way the two sides could silently diverge:
//!
//! - **The order is imposed here, not by the caller.** Directory iteration
//!   order is not stable across filesystems and a tar's entry order is
//!   whatever the writer chose, so a digest that trusted its input's order
//!   would depend on which machine walked the tree.
//! - **Lengths are hashed before the values they precede**, so `("ab", "c")`
//!   and `("a", "bc")` cannot collide. Without it, moving one character from a
//!   path into a file's contents produces the same digest, and two different
//!   bundles share a version.
//! - **[`MANIFEST_FILE`] is excluded**, because the publisher writes it *after*
//!   digesting and it carries both the digest and a timestamp. Including it
//!   would be circular, and it would make every republish of identical content
//!   a new version. The exclusion is applied inside this function rather than
//!   asked of the caller for the same reason the sort is: it is a rule that
//!   has to hold on both sides of a network boundary.

use sha2::{Digest, Sha256};

/// The manifest that travels inside a bundle directory, and the one file a
/// bundle's own digest never covers.
pub const MANIFEST_FILE: &str = "bundle.json";

/// The content address of a set of files, as `sha256:<hex>`.
///
/// Paths are relative to the bundle root, with `/` separators on every
/// platform. See the module docs for the three rules that make this the same
/// number at home and at the origin.
pub fn digest_files<'a, I>(files: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let mut entries: Vec<(&str, &[u8])> = files
        .into_iter()
        .filter(|(path, _)| *path != MANIFEST_FILE)
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let mut hasher = Sha256::new();
    for (path, bytes) in entries {
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    format!("sha256:{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path and a file's contents both feed the digest, so their boundary has
    /// to be unambiguous or two different bundles share a version.
    #[test]
    fn the_digest_cannot_confuse_a_path_boundary_with_a_content_boundary() {
        assert_ne!(
            digest_files([("ab", b"c".as_slice())]),
            digest_files([("a", b"bc".as_slice())])
        );
    }

    /// The publisher walks a directory and the server unpacks an archive.
    /// Neither order is anybody's choice, so the order cannot be part of the
    /// answer.
    #[test]
    fn the_order_the_files_arrive_in_is_not_part_of_the_answer() {
        let forwards = digest_files([
            ("a.html", b"one".as_slice()),
            ("assets/b.css", b"two".as_slice()),
            ("c.js", b"three".as_slice()),
        ]);
        let backwards = digest_files([
            ("c.js", b"three".as_slice()),
            ("assets/b.css", b"two".as_slice()),
            ("a.html", b"one".as_slice()),
        ]);
        assert_eq!(forwards, backwards);
    }

    /// The manifest carries the digest and a timestamp. Digesting it would be
    /// circular, and it would make every republish of identical content mint a
    /// new version.
    #[test]
    fn the_manifest_is_never_part_of_its_own_bundles_address() {
        let without = digest_files([("index.html", b"x".as_slice())]);
        let with = digest_files([
            ("index.html", b"x".as_slice()),
            (MANIFEST_FILE, br#"{"version":9}"#.as_slice()),
        ]);
        assert_eq!(without, with);

        // …and only at the root. A file that merely ends in the same name is
        // ordinary bundle content, and dropping it would put two different
        // bundles at one address.
        let nested = digest_files([
            ("index.html", b"x".as_slice()),
            ("data/bundle.json", br#"{"rows":[]}"#.as_slice()),
        ]);
        assert_ne!(without, nested);
    }

    #[test]
    fn an_empty_bundle_still_has_an_address_and_it_is_not_a_files() {
        assert!(digest_files([]).starts_with("sha256:"));
        assert_ne!(digest_files([]), digest_files([("a", b"".as_slice())]));
    }
}
