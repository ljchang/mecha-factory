//! Where uploaded files rest: `<data_dir>/attachments/<user_id>/<id>`.
//!
//! Flat and opaque on purpose. `id` is minted by us (128 random bits, hex),
//! so a stranger's filename never touches the filesystem — it lives in the
//! ledger as display data for the human who eventually opens the file. There
//! is no version, no directory tree, no extension: the ledger row carries the
//! content type, and the drain names the file properly at the other end.
//!
//! Lifetime is the ledger's: an attachment row is created in the same
//! transaction that queues its submission ([`crate::db::Db::upload_complete`])
//! and deleted in the same transaction as its queue row (ack, expiry). The
//! file follows the row — written before the transaction, deleted after it —
//! and [`Store::orphan_sweep`] is the backstop for a crash landing between a
//! file operation and its transaction, in either direction.

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

/// How old an unclaimed file must be before the sweep removes it. An upload
/// writes the file *before* its transaction commits, so a file with no row is
/// either mid-upload or debris — and one hour is far past any request's life.
const ORPHAN_GRACE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

impl Store {
    pub fn new(root: PathBuf) -> Result<Store> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating {}", root.display()))?;
        Ok(Store { root })
    }

    /// Mint a blob id: the on-disk name and the ledger key. Ours, never
    /// derived from anything a stranger sent — the same generator as every
    /// other id the box mints.
    pub fn mint_id() -> String {
        crate::keys::random_id()
    }

    pub fn path(&self, user_id: &str, id: &str) -> PathBuf {
        self.root.join(user_id).join(id)
    }

    /// Write bytes under a minted id: staging sibling, then rename, the same
    /// discipline as the bundle store — a reader never sees a half-written
    /// file, and the reader here is the drain.
    pub fn write(&self, user_id: &str, id: &str, bytes: &[u8]) -> Result<PathBuf> {
        let target = self.path(user_id, id);
        let parent = target.parent().expect("path always has the user dir");
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let staging = parent.join(format!(".staging-{id}"));
        std::fs::write(&staging, bytes)
            .with_context(|| format!("writing {}", staging.display()))?;
        std::fs::rename(&staging, &target)
            .with_context(|| format!("installing {}", target.display()))?;
        Ok(target)
    }

    /// Remove a blob. Missing is fine — deletion runs after the transaction
    /// that removed the row, so a retry may find the file already gone.
    pub fn delete(&self, user_id: &str, id: &str) {
        let _ = std::fs::remove_file(self.path(user_id, id));
    }

    /// Delete a batch, e.g. everything an ack named.
    pub fn delete_all(&self, user_id: &str, ids: &[String]) {
        for id in ids {
            self.delete(user_id, id);
        }
    }

    /// Delete a batch that crosses users — the expiry sweeps' shape.
    pub fn delete_owned(&self, pairs: &[(String, String)]) {
        for (user_id, id) in pairs {
            self.delete(user_id, id);
        }
    }

    /// Bytes still free on the filesystem the store writes to.
    ///
    /// What the upload handler compares against `min_free_bytes`: a refusal
    /// while there is still headroom beats the whole box discovering a full
    /// disk. The one libc call in the crate.
    pub fn free_bytes(&self) -> Result<u64> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(self.root.as_os_str().as_bytes())
            .context("store path holds a NUL")?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
        anyhow::ensure!(rc == 0, "statvfs on {} failed", self.root.display());
        // f_bavail: what an unprivileged writer can actually use, which is
        // the honest number — root's reserve is not ours to spend.
        Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
    }

    /// Remove files no ledger row claims, older than the grace window.
    ///
    /// This is what lets every other file deletion be best-effort-after-
    /// commit: a crash between a write and its transaction, or a commit and
    /// its deletion, leaves a file the next sweep reclaims. Returns how many
    /// went, so the sweep can say it.
    pub fn orphan_sweep(&self, db: &crate::db::Db) -> Result<usize> {
        let mut removed = 0;
        let users = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(_) => return Ok(0),
        };
        for user in users.flatten() {
            if !user.path().is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(user.path())?.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let age = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok());
                if age.is_none_or(|a| a < ORPHAN_GRACE) {
                    continue;
                }
                let name = entry.file_name();
                let Some(id) = name.to_str() else { continue };
                // Staging debris ages past the grace window too — a crash
                // mid-write leaves one — and no row will ever claim it.
                let claimed = !id.starts_with(".staging-") && db.attachment_exists(id)?;
                if !claimed {
                    let _ = std::fs::remove_file(&path);
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("attachments")).unwrap();
        (dir, store)
    }

    #[test]
    fn write_is_rename_and_delete_is_idempotent() {
        let (_dir, store) = store();
        let path = store.write("u1", "abc123", b"bytes").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"bytes");
        assert!(
            !path.parent().unwrap().join(".staging-abc123").exists(),
            "staging is renamed away, not copied"
        );
        store.delete("u1", "abc123");
        assert!(!path.exists());
        store.delete("u1", "abc123"); // and again, without complaint
    }

    #[test]
    fn minted_ids_are_ours_and_do_not_collide_casually() {
        let a = Store::mint_id();
        let b = Store::mint_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// A fresh file with no row survives the sweep (it may be mid-upload); an
    /// old file with no row goes; an old file *with* a row stays.
    #[test]
    fn the_orphan_sweep_reads_the_ledger_and_the_clock() {
        let (dir, store) = store();
        let db = Db::open(&dir.path().join("factory.db")).unwrap();
        let user = db
            .user_create("handle1", "h@example.org", &crate::db::now())
            .unwrap();
        let u1 = user.id.as_str();

        let fresh = store.write(u1, "freshfresh", b"x").unwrap();
        let old_orphan = store.write(u1, "orphanorphan", b"x").unwrap();
        let old_claimed = store.write(u1, "claimedclaimed", b"x").unwrap();

        // Age two of them past the grace window.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        for path in [&old_orphan, &old_claimed] {
            let file = std::fs::File::options().append(true).open(path).unwrap();
            file.set_modified(old).unwrap();
        }

        // Claim one through a real completed upload, not a hand-inserted row.
        let seq = seed_awaiting_upload(&db, u1);
        let row = crate::db::AttachmentRow {
            id: "claimedclaimed".into(),
            user_id: u1.into(),
            seq,
            field: "cv".into(),
            filename: "cv.pdf".into(),
            content_type: "application/pdf".into(),
            size: 1,
            sha256: format!("sha256:{}", "ab".repeat(32)),
            created_at: crate::db::now(),
        };
        db.upload_complete(u1, "uploadhash", &far_future(), "{}", &[row])
            .unwrap()
            .expect("the upload completes");

        let removed = store.orphan_sweep(&db).unwrap();
        assert_eq!(removed, 1);
        assert!(fresh.exists(), "too young to be debris");
        assert!(!old_orphan.exists(), "old and unclaimed: removed");
        assert!(old_claimed.exists(), "old but the ledger claims it");
    }

    fn far_future() -> String {
        "2020-01-01T00:00:00Z".into() // `now` before any expiry in the test
    }

    fn seed_awaiting_upload(db: &Db, user_id: &str) -> i64 {
        let submission = crate::db::Submission {
            user_id: user_id.into(),
            type_id: "letterish".into(),
            payload: "{}".into(),
            created_at: crate::db::now(),
            retain_until: None,
            verify_hash: "verifyhash".into(),
            verify_expires: "2999-01-01T00:00:00Z".into(),
            recipient_hash: "recipient".into(),
        };
        let seq = db.submission_add(&submission).unwrap();
        db.submission_verify(
            user_id,
            "verifyhash",
            "2000-01-01T00:00:00Z",
            crate::db::VerifyNext::AwaitingUpload {
                upload_hash: "uploadhash".into(),
                upload_expires: "2999-01-01T00:00:00Z".into(),
            },
        )
        .unwrap()
        .expect("verification spends");
        seq
    }
}
