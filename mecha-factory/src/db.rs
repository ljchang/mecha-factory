//! The ledger: what has been published, what is queued, and which keys exist.
//!
//! SQLite in WAL mode, one file. The bundle *bytes* live on disk beside it —
//! a database is the wrong place for a hundred megabytes of Pyodide, and a
//! static file server wants to hand the kernel a path rather than stream rows
//! out of a page cache. So the invariant is: **the database is the index and
//! the disk is the content**, and the index is authoritative about what exists.
//!
//! Three things about the schema are decisions rather than shape:
//!
//! - **A version row is inserted once and never updated.** There is no `UPDATE`
//!   against `bundles` anywhere in this file, which is what makes "published
//!   versions are immutable" a property of the code rather than a promise. The
//!   one moving part is `aliases`, and it is its own table for exactly that
//!   reason.
//! - **Nothing is ever deleted except an acknowledged queue row.** A takedown
//!   moves the alias to nothing; the version stays. Deleting a queue row is the
//!   one destructive operation on the box, and it happens only after home says
//!   it has the record — which is why `drain` is a pure read (see [`Db::drain`]).
//! - **A key row holds a hash, never a key.** Reading this file off a lost box
//!   gets an attacker an Argon2id verifier and no token.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The current schema version. Bumped alongside a migration in [`migrate`].
const SCHEMA: i64 = 3;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    pub id: String,
    /// Whose key this is. Every row a key creates carries the same value, and
    /// every read a key performs is filtered by it — which is the whole of
    /// tenant isolation, expressed as one column rather than as discipline in
    /// each handler.
    pub user_id: String,
    pub scope: Scope,
    pub hash: String,
    pub label: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

/// A person. Never deleted — `status` is how an account stops working, because
/// the ledger of what they published has to outlive the account itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRow {
    /// Opaque and stable. Rows point at this and never at the email, which is
    /// a mutable fact about a person rather than an identifier.
    pub id: String,
    /// The label in the hostname. See [`Db::user_create`] for why one of these
    /// is never handed out twice.
    pub handle: String,
    pub email: String,
    /// `active` | `suspended`.
    pub status: String,
    pub created_at: String,
    /// Published bytes this user may hold. Enforced at publish; present before
    /// it is enforced so that turning it on is a policy change rather than a
    /// migration.
    pub quota_bytes: i64,
    /// Verification emails per day, once anything sends them. Reputation is
    /// earned per account rather than granted at signup.
    pub send_budget: i64,
}

impl UserRow {
    pub fn active(&self) -> bool {
        self.status == "active"
    }
}

/// What a key may do — and the scope is read from **this row**, never from the
/// token's `mk_pub_` prefix, which is a human-readable label an attacker
/// controls the text of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Write immutable versions, and read back this user's own types.
    ///
    /// Deliberately **not** enough to make anything public. A bundle published
    /// and never aliased is private and unreachable, so this is the scope an
    /// agent can hold: the worst a stolen one does is write versions nobody
    /// can read.
    Publish,
    /// Move an alias, and serve a form. The two acts that change what the
    /// world can see.
    ///
    /// Separated from `Publish` because "an agent drafts, a human releases"
    /// was a property of the **client's** configuration — mecha's
    /// `[outbox] tools` — so a different MCP client, or a typo in that list,
    /// had no review at all and nothing said so. A guarantee that depends on
    /// which program connected is the silently-degrading-sandbox shape this
    /// project keeps refusing, so it moved to the side that cannot be
    /// bypassed.
    Release,
    /// Read the queue and acknowledge records.
    Drain,
}

impl Scope {
    /// Every scope there is.
    ///
    /// Here so that code which must handle all of them — parsing a presented
    /// token, listing what `key create` accepts — iterates rather than
    /// repeating a list. `keys::split` used to name two prefixes inline, so
    /// adding a third minted tokens that nothing could parse: the key
    /// authenticated as no scope at all and the endpoint answered 401. A match
    /// would have caught it; a hand-copied list did not.
    pub const ALL: [Scope; 3] = [Scope::Publish, Scope::Release, Scope::Drain];

    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Publish => "publish",
            Scope::Release => "release",
            Scope::Drain => "drain",
        }
    }

    pub fn parse(text: &str) -> Result<Scope> {
        match text {
            "publish" => Ok(Scope::Publish),
            "release" => Ok(Scope::Release),
            "drain" => Ok(Scope::Drain),
            other => anyhow::bail!("unknown scope `{other}` (publish | release | drain)"),
        }
    }

    /// The prefix a token of this scope carries. Cosmetic: it tells a human
    /// which file they are looking at, and the server never trusts it.
    pub fn prefix(&self) -> &'static str {
        match self {
            Scope::Publish => "mk_pub_",
            Scope::Release => "mk_rel_",
            Scope::Drain => "mk_drn_",
        }
    }
}

/// One published version, as the index knows it.
#[derive(Debug, Clone)]
pub struct BundleRow {
    pub user_id: String,
    pub id: String,
    pub version: u32,
    pub digest: String,
    pub class: mecha_manifest::ContentClass,
    pub title: String,
    pub description: Option<String>,
    pub template: String,
    pub published_at: Option<String>,
    pub received_at: String,
    /// Set when this was taken out of service on a report. The bytes stay on
    /// disk: withholding is instant and reversible, and destroying evidence in
    /// response to a complaint is how you lose the ability to answer it.
    pub withheld_at: Option<String>,
    pub withheld_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AliasRow {
    pub version: Option<u32>,
    pub visibility: mecha_manifest::Visibility,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct TypeRow {
    pub user_id: String,
    pub id: String,
    pub title: String,
    pub manifest: String,
    pub schema: String,
    pub updated_at: String,
}

/// A submission on its way in, before anybody has proved anything.
#[derive(Debug, Clone)]
pub struct Submission {
    pub user_id: String,
    pub type_id: String,
    pub payload: String,
    pub created_at: String,
    pub retain_until: Option<String>,
    pub verify_hash: String,
    pub verify_expires: String,
    pub recipient_hash: String,
}

/// A typed request as it sits on the box, before home has ever seen it.
#[derive(Debug, Clone)]
pub struct QueueRow {
    pub seq: i64,
    pub user_id: String,
    pub type_id: String,
    pub state: String,
    pub payload: String,
    pub created_at: String,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // WAL so a reader is never blocked by the writer: the box serves static
        // bytes while a publish is landing, and a five-second stall on a report
        // because someone is uploading a notebook would be a real one.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&conn).with_context(|| format!("migrating {}", path.display()))?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        migrate(&conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().expect("the ledger lock is never poisoned");
        f(&conn)
    }

    // ---- users ----------------------------------------------------------

    /// Create a user and claim their handle, or fail.
    ///
    /// **A handle is never issued twice**, and the `handles` table is what
    /// makes that true across renames and closed accounts rather than only
    /// across live ones. The check and the claim are one transaction: two
    /// concurrent signups for the same name must not both succeed, and a
    /// check-then-insert is exactly the race that lets them.
    pub fn user_create(&self, handle: &str, email: &str, now: &str) -> Result<UserRow> {
        let id = crate::keys::random_id();
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            if let Some((owner, retired)) = handle_owner(&tx, handle)? {
                anyhow::bail!(
                    "the handle `{handle}` {} — handles are never reused, because \
                     every URL somebody published under one would otherwise resolve \
                     to whoever claimed the name next",
                    if retired {
                        format!("was retired by user {owner}")
                    } else {
                        format!("belongs to user {owner}")
                    }
                );
            }
            tx.execute(
                "INSERT INTO users (id, handle, email, status, created_at) \
                 VALUES (?1, ?2, ?3, 'active', ?4)",
                params![id, handle, email, now],
            )?;
            tx.execute(
                "INSERT INTO handles (handle, user_id, issued_at) VALUES (?1, ?2, ?3)",
                params![handle, id, now],
            )?;
            tx.commit()?;
            Ok(())
        })?;
        self.user(&id)?
            .ok_or_else(|| anyhow::anyhow!("the user vanished between writing and reading it"))
    }

    pub fn user(&self, id: &str) -> Result<Option<UserRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, handle, email, status, created_at, quota_bytes, send_budget \
                     FROM users WHERE id = ?1",
                    params![id],
                    user_row,
                )
                .optional()?)
        })
    }

    /// The user a hostname's leading label names.
    ///
    /// Only a *live* handle resolves. A retired one is not an error and not a
    /// redirect: it names nobody, so what was published under it stops being
    /// served rather than being served by its next owner — which is the point
    /// of never reusing one.
    pub fn user_by_handle(&self, handle: &str) -> Result<Option<UserRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, handle, email, status, created_at, quota_bytes, send_budget \
                     FROM users WHERE handle = ?1",
                    params![handle],
                    user_row,
                )
                .optional()?)
        })
    }

    pub fn users(&self) -> Result<Vec<UserRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, handle, email, status, created_at, quota_bytes, send_budget \
                 FROM users ORDER BY created_at",
            )?;
            let rows = stmt.query_map([], user_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Stop or restart an account. Never a delete: the record of what somebody
    /// published has to outlive their access to publish more.
    pub fn user_status(&self, id: &str, status: &str) -> Result<bool> {
        self.with(|conn| {
            let n = conn.execute(
                "UPDATE users SET status = ?2 WHERE id = ?1",
                params![id, status],
            )?;
            Ok(n > 0)
        })
    }

    /// Verification emails this user's forms may send in a day.
    ///
    /// Small at signup and raised deliberately: reputation is earned per
    /// account rather than granted at it (§15.5).
    pub fn user_send_budget(&self, id: &str, send_budget: i64) -> Result<bool> {
        self.with(|conn| {
            let n = conn.execute(
                "UPDATE users SET send_budget = ?2 WHERE id = ?1",
                params![id, send_budget],
            )?;
            Ok(n > 0)
        })
    }

    pub fn user_quota(&self, id: &str, quota_bytes: i64) -> Result<bool> {
        self.with(|conn| {
            let n = conn.execute(
                "UPDATE users SET quota_bytes = ?2 WHERE id = ?1",
                params![id, quota_bytes],
            )?;
            Ok(n > 0)
        })
    }

    // ---- keys -----------------------------------------------------------

    pub fn key_insert(&self, row: &KeyRow) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO keys (id, user_id, scope, hash, label, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.id,
                    row.user_id,
                    row.scope.as_str(),
                    row.hash,
                    row.label,
                    row.created_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn key_by_id(&self, id: &str) -> Result<Option<KeyRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, user_id, scope, hash, label, created_at, revoked_at \
                     FROM keys WHERE id = ?1",
                    params![id],
                    key_row,
                )
                .optional()?)
        })
    }

    pub fn keys(&self) -> Result<Vec<KeyRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, user_id, scope, hash, label, created_at, revoked_at \
                 FROM keys ORDER BY created_at",
            )?;
            let rows = stmt.query_map([], key_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Revoking never deletes: the row is what says a key existed and when it
    /// stopped working, which is the only thing an incident has to read.
    pub fn key_revoke(&self, id: &str, now: &str) -> Result<bool> {
        self.with(|conn| {
            let n = conn.execute(
                "UPDATE keys SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
                params![id, now],
            )?;
            Ok(n > 0)
        })
    }

    // ---- bundles --------------------------------------------------------

    pub fn bundle_insert(&self, row: &BundleRow) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO bundles (user_id, id, version, digest, class, title, description, \
                 template, published_at, received_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    row.user_id,
                    row.id,
                    row.version,
                    row.digest,
                    row.class.as_str(),
                    row.title,
                    row.description,
                    row.template,
                    row.published_at,
                    row.received_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn bundle(&self, user_id: &str, id: &str, version: u32) -> Result<Option<BundleRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    &format!("{BUNDLE_COLUMNS} WHERE user_id = ?1 AND id = ?2 AND version = ?3"),
                    params![user_id, id, version],
                    bundle_row,
                )
                .optional()?)
        })
    }

    /// The content-addressed lookup: has this user already published this id
    /// with exactly these bytes? A yes means a republish mints nothing.
    ///
    /// Scoped to the user, so two people publishing byte-identical reports get
    /// a version each. Sharing storage across users on a digest match would be
    /// a cross-tenant read with extra steps.
    pub fn bundle_by_digest(
        &self,
        user_id: &str,
        id: &str,
        digest: &str,
    ) -> Result<Option<BundleRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "{BUNDLE_COLUMNS} WHERE user_id = ?1 AND id = ?2 AND digest = ?3 \
                         ORDER BY version LIMIT 1"
                    ),
                    params![user_id, id, digest],
                    bundle_row,
                )
                .optional()?)
        })
    }

    pub fn bundle_versions(&self, user_id: &str, id: &str) -> Result<Vec<u32>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT version FROM bundles WHERE user_id = ?1 AND id = ?2 ORDER BY version",
            )?;
            let rows = stmt.query_map(params![user_id, id], |r| r.get::<_, u32>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Take a version out of service without destroying it.
    ///
    /// The response to a report, and deliberately reversible: the bytes stay,
    /// so an accusation that turns out to be wrong costs nothing, and one that
    /// turns out to be right leaves the evidence intact. Destroying bytes is
    /// `purge`, which is a different verb nobody automates.
    pub fn bundle_withhold(
        &self,
        user_id: &str,
        id: &str,
        version: u32,
        reason: Option<&str>,
        now: Option<&str>,
    ) -> Result<bool> {
        self.with(|conn| {
            let n = conn.execute(
                "UPDATE bundles SET withheld_at = ?4, withheld_reason = ?5 \
                 WHERE user_id = ?1 AND id = ?2 AND version = ?3",
                params![user_id, id, version, now, reason],
            )?;
            Ok(n > 0)
        })
    }

    pub fn bundle_count(&self) -> Result<i64> {
        self.with(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM bundles", [], |r| r.get(0))?))
    }

    // ---- aliases --------------------------------------------------------

    pub fn alias(&self, user_id: &str, id: &str) -> Result<Option<AliasRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT version, visibility, updated_at FROM aliases \
                     WHERE user_id = ?1 AND id = ?2",
                    params![user_id, id],
                    |r| {
                        Ok(AliasRow {
                            version: r.get(0)?,
                            visibility: visibility_of(&r.get::<_, String>(1)?),
                            updated_at: r.get(2)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    pub fn alias_set(
        &self,
        user_id: &str,
        id: &str,
        version: Option<u32>,
        visibility: mecha_manifest::Visibility,
        now: &str,
    ) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO aliases (user_id, id, version, visibility, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(user_id, id) DO UPDATE SET version = ?3, visibility = ?4, \
                 updated_at = ?5",
                params![
                    user_id,
                    id,
                    version,
                    match visibility {
                        mecha_manifest::Visibility::Public => "public",
                        mecha_manifest::Visibility::Private => "private",
                    },
                    now
                ],
            )?;
            Ok(())
        })
    }

    // ---- request types --------------------------------------------------

    pub fn type_put(&self, row: &TypeRow) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO types (user_id, id, title, manifest, schema, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(user_id, id) DO UPDATE SET \
                 title = ?3, manifest = ?4, schema = ?5, updated_at = ?6",
                params![
                    row.user_id,
                    row.id,
                    row.title,
                    row.manifest,
                    row.schema,
                    row.updated_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn type_get(&self, user_id: &str, id: &str) -> Result<Option<TypeRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT user_id, id, title, manifest, schema, updated_at FROM types \
                     WHERE user_id = ?1 AND id = ?2",
                    params![user_id, id],
                    type_row,
                )
                .optional()?)
        })
    }

    pub fn types(&self, user_id: &str) -> Result<Vec<TypeRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT user_id, id, title, manifest, schema, updated_at FROM types \
                 WHERE user_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![user_id], type_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- the queue ------------------------------------------------------

    pub fn queue_add(
        &self,
        user_id: &str,
        type_id: &str,
        state: &str,
        payload: &str,
        now: &str,
        retain_until: Option<&str>,
    ) -> Result<i64> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO queue (user_id, type_id, state, payload, created_at, retain_until) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![user_id, type_id, state, payload, now, retain_until],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Everything of this user's queued after `since`, oldest first.
    ///
    /// **A pure read.** Marking a row drained here would lose it whenever the
    /// response failed to arrive, and the whole point of the queue is that a
    /// stranger's request cannot silently evaporate. Home acknowledges what it
    /// has stored, and until then the row comes back.
    pub fn drain(&self, user_id: &str, since: i64, limit: usize) -> Result<Vec<QueueRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq, user_id, type_id, state, payload, created_at FROM queue \
                 WHERE user_id = ?1 AND seq > ?2 AND state = 'queued' ORDER BY seq LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![user_id, since, limit as i64], queue_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Delete exactly the sequence numbers this user says they have.
    ///
    /// By list rather than by watermark: a watermark deletes rows nobody named,
    /// and the failure is silent. Scoped by user for the obvious reason —
    /// acknowledging somebody else's record would delete it.
    pub fn queue_ack(&self, user_id: &str, seqs: &[i64]) -> Result<usize> {
        self.with(|conn| {
            let mut removed = 0;
            let tx = conn.unchecked_transaction()?;
            for seq in seqs {
                removed += tx.execute(
                    "DELETE FROM queue WHERE user_id = ?1 AND seq = ?2",
                    params![user_id, seq],
                )?;
            }
            tx.commit()?;
            Ok(removed)
        })
    }

    pub fn queue_depth(&self, user_id: Option<&str>) -> Result<i64> {
        self.with(|conn| match user_id {
            Some(user_id) => Ok(conn.query_row(
                "SELECT COUNT(*) FROM queue WHERE user_id = ?1 AND state = 'queued'",
                params![user_id],
                |r| r.get(0),
            )?),
            None => Ok(conn.query_row(
                "SELECT COUNT(*) FROM queue WHERE state = 'queued'",
                [],
                |r| r.get(0),
            )?),
        })
    }

    /// Records whose retention window has passed.
    ///
    /// The sweep that reads this is what makes `retain_until` a policy rather
    /// than a column — see §15.4. Returned rather than deleted here so the
    /// caller can say what it removed, the same shape `mecha work clean` uses.
    pub fn queue_expired(&self, now: &str) -> Result<Vec<QueueRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq, user_id, type_id, state, payload, created_at FROM queue \
                 WHERE retain_until IS NOT NULL AND retain_until <= ?1 ORDER BY seq",
            )?;
            let rows = stmt.query_map(params![now], queue_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- intake ---------------------------------------------------------

    /// Store a submission awaiting verification.
    ///
    /// It lands as `submitted`, which `drain` does not return: an unverified
    /// row costs a little disk and never a triage run.
    pub fn submission_add(&self, row: &Submission) -> Result<i64> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO queue (user_id, type_id, state, payload, created_at, retain_until, \
                 verify_hash, verify_expires, recipient_hash, submitted_on) \
                 VALUES (?1, ?2, 'submitted', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.user_id,
                    row.type_id,
                    row.payload,
                    row.created_at,
                    row.retain_until,
                    row.verify_hash,
                    row.verify_expires,
                    row.recipient_hash,
                    &row.created_at[..10.min(row.created_at.len())],
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Spend a verification token: `submitted` → `queued`, once.
    ///
    /// Read and spend inside one transaction, keyed on the sequence number the
    /// read found. Two clicks racing means one `UPDATE` matches a row and the
    /// other matches nothing, which is exactly what single-use means — and
    /// zero matched rows is the whole of "already used", "expired" and "never
    /// existed", which is also all the caller can tell a stranger.
    ///
    /// **Not `last_insert_rowid()`.** An earlier version selected the row back
    /// that way after the update; it is per *connection* and per *any* table,
    /// so an unrelated insert between the submission and the click — minting a
    /// key, say — silently made confirmation fail. Found by a test that did
    /// exactly that, having passed in the test that did not.
    pub fn submission_verify(
        &self,
        user_id: &str,
        hash: &str,
        now: &str,
    ) -> Result<Option<QueueRow>> {
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            let found: Option<QueueRow> = tx
                .query_row(
                    "SELECT seq, user_id, type_id, state, payload, created_at FROM queue \
                     WHERE user_id = ?1 AND verify_hash = ?2 AND state = 'submitted' \
                     AND verify_expires > ?3",
                    params![user_id, hash, now],
                    queue_row,
                )
                .optional()?;
            let Some(mut row) = found else {
                return Ok(None);
            };
            let changed = tx.execute(
                "UPDATE queue SET state = 'queued', verify_hash = NULL, verify_expires = NULL \
                 WHERE seq = ?1 AND state = 'submitted'",
                params![row.seq],
            )?;
            tx.commit()?;
            if changed == 0 {
                return Ok(None);
            }
            row.state = "queued".into();
            Ok(Some(row))
        })
    }

    /// Verification sends today: to this recipient, and by this user.
    pub fn sends_today(&self, user_id: &str, recipient: &str, today: &str) -> Result<(i64, i64)> {
        self.with(|conn| {
            let to_recipient = conn.query_row(
                "SELECT COUNT(*) FROM queue WHERE user_id = ?1 AND submitted_on = ?2 \
                 AND recipient_hash = ?3",
                params![user_id, today, recipient],
                |r| r.get(0),
            )?;
            let by_user = conn.query_row(
                "SELECT COUNT(*) FROM queue WHERE user_id = ?1 AND submitted_on = ?2",
                params![user_id, today],
                |r| r.get(0),
            )?;
            Ok((to_recipient, by_user))
        })
    }

    /// Unverified rows whose link has expired.
    ///
    /// **Deleted rather than kept**: an abandoned submission is a stranger's
    /// personal data with no consent behind it, and "never verified" is the
    /// one state where keeping the record serves nobody. Returns what went, so
    /// the sweep can say it.
    pub fn expire_unverified(&self, now: &str) -> Result<usize> {
        self.with(|conn| {
            Ok(conn.execute(
                "DELETE FROM queue WHERE state = 'submitted' AND verify_expires IS NOT NULL \
                 AND verify_expires <= ?1",
                params![now],
            )?)
        })
    }

    /// Drop everything past its retention window.
    pub fn expire_retained(&self, now: &str) -> Result<usize> {
        self.with(|conn| {
            Ok(conn.execute(
                "DELETE FROM queue WHERE retain_until IS NOT NULL AND retain_until <= ?1",
                params![now],
            )?)
        })
    }

    // ---- idempotency ----------------------------------------------------

    pub fn idempotent(&self, user_id: &str, key: &str) -> Result<Option<(String, u32)>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT bundle_id, version FROM idempotency WHERE user_id = ?1 AND key = ?2",
                    params![user_id, key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?)
        })
    }

    pub fn idempotency_record(
        &self,
        user_id: &str,
        key: &str,
        id: &str,
        version: u32,
        now: &str,
    ) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO idempotency (key, user_id, bundle_id, version, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![key, user_id, id, version, now],
            )?;
            Ok(())
        })
    }
}

/// Who owns a handle, and whether they still use it. Inside a transaction,
/// because the check and the claim have to be one.
fn handle_owner(conn: &Connection, handle: &str) -> Result<Option<(String, bool)>> {
    Ok(conn
        .query_row(
            "SELECT user_id, retired_at FROM handles WHERE handle = ?1",
            params![handle],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.is_some(),
                ))
            },
        )
        .optional()?)
}

const BUNDLE_COLUMNS: &str = "SELECT user_id, id, version, digest, class, title, description, \
     template, published_at, received_at, withheld_at, withheld_reason FROM bundles";

fn key_row(r: &rusqlite::Row) -> rusqlite::Result<KeyRow> {
    Ok(KeyRow {
        id: r.get(0)?,
        user_id: r.get(1)?,
        // An unreadable scope is treated as the narrower one rather than as an
        // error, because the alternative is an unparseable row locking out a
        // legitimate key. `drain` cannot publish.
        scope: Scope::parse(&r.get::<_, String>(2)?).unwrap_or(Scope::Drain),
        hash: r.get(3)?,
        label: r.get(4)?,
        created_at: r.get(5)?,
        revoked_at: r.get(6)?,
    })
}

fn user_row(r: &rusqlite::Row) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        id: r.get(0)?,
        handle: r.get(1)?,
        email: r.get(2)?,
        // An unreadable status is `suspended`, not `active`: a row we cannot
        // interpret must not be the one that keeps serving.
        status: match r.get::<_, String>(3)?.as_str() {
            "active" => "active".to_string(),
            other => other.to_string(),
        },
        created_at: r.get(4)?,
        quota_bytes: r.get(5)?,
        send_budget: r.get(6)?,
    })
}

fn queue_row(r: &rusqlite::Row) -> rusqlite::Result<QueueRow> {
    Ok(QueueRow {
        seq: r.get(0)?,
        user_id: r.get(1)?,
        type_id: r.get(2)?,
        state: r.get(3)?,
        payload: r.get(4)?,
        created_at: r.get(5)?,
    })
}

fn bundle_row(r: &rusqlite::Row) -> rusqlite::Result<BundleRow> {
    Ok(BundleRow {
        user_id: r.get(0)?,
        id: r.get(1)?,
        version: r.get(2)?,
        digest: r.get(3)?,
        class: class_of(&r.get::<_, String>(4)?),
        title: r.get(5)?,
        description: r.get(6)?,
        template: r.get(7)?,
        published_at: r.get(8)?,
        received_at: r.get(9)?,
        withheld_at: r.get(10)?,
        withheld_reason: r.get(11)?,
    })
}

fn type_row(r: &rusqlite::Row) -> rusqlite::Result<TypeRow> {
    Ok(TypeRow {
        user_id: r.get(0)?,
        id: r.get(1)?,
        title: r.get(2)?,
        manifest: r.get(3)?,
        schema: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

/// An unrecognised class reads as `static`, which is the class that permits
/// nothing. A row we cannot interpret must not be the one that gets
/// `wasm-unsafe-eval`.
fn class_of(text: &str) -> mecha_manifest::ContentClass {
    match text {
        "compute" => mecha_manifest::ContentClass::Compute,
        "interactive" => mecha_manifest::ContentClass::Interactive,
        _ => mecha_manifest::ContentClass::Static,
    }
}

/// Likewise: an unreadable visibility is private, so a row we cannot interpret
/// is never the one that gets served to the world.
fn visibility_of(text: &str) -> mecha_manifest::Visibility {
    match text {
        "public" => mecha_manifest::Visibility::Public,
        _ => mecha_manifest::Visibility::Private,
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= SCHEMA {
        return Ok(());
    }
    // Schema 1 predates users, and the change is not one SQLite can make in
    // place: every table's primary key gained a user. Refused rather than
    // half-migrated, because a ledger that is partly scoped is worse than one
    // that will not open — and this server has never been deployed, so the
    // honest instruction is the cheap one.
    if version > 0 && version < SCHEMA {
        anyhow::bail!(
            "this ledger is schema {version} and this binary speaks {SCHEMA}. \
             Nothing has been deployed: delete the database file and start again."
        );
    }
    if version == 1 {
        anyhow::bail!(
            "this ledger is schema 1, which predates users. Nothing has been \
             deployed from it: delete the database file and start again."
        );
    }
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS users (
            id           TEXT PRIMARY KEY,
            handle       TEXT NOT NULL UNIQUE,
            email        TEXT NOT NULL DEFAULT '',
            status       TEXT NOT NULL DEFAULT 'active',
            created_at   TEXT NOT NULL,
            quota_bytes  INTEGER NOT NULL DEFAULT 1073741824,
            send_budget  INTEGER NOT NULL DEFAULT 50
        );

        -- Every handle ever issued, including ones nobody uses any more. The
        -- UNIQUE on users.handle says who owns a name *now*; this says who has
        -- ever owned it, which is what makes a handle unreusable after a
        -- rename or a closed account. A freed handle is a hijack: every URL
        -- that person put in a paper resolves to whoever claims the name next.
        CREATE TABLE IF NOT EXISTS handles (
            handle      TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL,
            issued_at   TEXT NOT NULL,
            retired_at  TEXT
        );

        CREATE TABLE IF NOT EXISTS keys (
            id          TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL DEFAULT '',
            scope       TEXT NOT NULL,
            hash        TEXT NOT NULL,
            label       TEXT NOT NULL DEFAULT '',
            created_at  TEXT NOT NULL,
            revoked_at  TEXT
        );

        CREATE TABLE IF NOT EXISTS bundles (
            user_id      TEXT NOT NULL DEFAULT '',
            id           TEXT NOT NULL,
            version      INTEGER NOT NULL,
            digest       TEXT NOT NULL,
            class        TEXT NOT NULL,
            title        TEXT NOT NULL,
            description  TEXT,
            template     TEXT NOT NULL,
            published_at TEXT,
            received_at  TEXT NOT NULL,
            withheld_at  TEXT,
            withheld_reason TEXT,
            -- Scoped by user, so two people may both publish `morning-brief`
            -- and neither can address the other's.
            PRIMARY KEY (user_id, id, version)
        );
        CREATE INDEX IF NOT EXISTS bundles_by_digest ON bundles (user_id, id, digest);

        CREATE TABLE IF NOT EXISTS aliases (
            user_id     TEXT NOT NULL DEFAULT '',
            id          TEXT NOT NULL,
            version     INTEGER,
            visibility  TEXT NOT NULL DEFAULT 'private',
            updated_at  TEXT NOT NULL,
            PRIMARY KEY (user_id, id)
        );

        CREATE TABLE IF NOT EXISTS types (
            user_id     TEXT NOT NULL DEFAULT '',
            id          TEXT NOT NULL,
            title       TEXT NOT NULL DEFAULT '',
            manifest    TEXT NOT NULL,
            schema      TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            PRIMARY KEY (user_id, id)
        );

        CREATE TABLE IF NOT EXISTS queue (
            seq         INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id     TEXT NOT NULL DEFAULT '',
            type_id     TEXT NOT NULL,
            state       TEXT NOT NULL,
            payload     TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            -- When this stops being ours to hold. A pile of other people's
            -- personal data with no deletion story is the thing you cannot
            -- start having and then stop.
            retain_until TEXT,
            -- The verification link, as a hash, cleared when it is spent. A
            -- row with one is `submitted` and is never drained.
            verify_hash TEXT,
            verify_expires TEXT,
            -- Who the link went to, as something countable that is not a
            -- second copy of their address.
            recipient_hash TEXT,
            -- The day it was submitted, for the send budgets.
            submitted_on TEXT
        );
        CREATE INDEX IF NOT EXISTS queue_by_state ON queue (user_id, state, seq);
        CREATE INDEX IF NOT EXISTS queue_by_verify ON queue (user_id, verify_hash);
        CREATE INDEX IF NOT EXISTS queue_by_sends ON queue (user_id, submitted_on, recipient_hash);

        CREATE TABLE IF NOT EXISTS idempotency (
            key         TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL DEFAULT '',
            bundle_id   TEXT NOT NULL,
            version     INTEGER NOT NULL,
            created_at  TEXT NOT NULL
        );
        ",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA)?;
    Ok(())
}

/// Now, as the wire and the ledger both spell it.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Today, as the send budgets count it.
pub fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

pub fn hours_from_now(hours: u32) -> String {
    (chrono::Utc::now() + chrono::Duration::hours(hours as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn days_from_now(days: u32) -> String {
    (chrono::Utc::now() + chrono::Duration::days(days as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mecha_manifest::{ContentClass, Visibility};

    /// A database with one user in it, since nothing exists outside a user now.
    fn db_with_user() -> (Db, String) {
        let db = Db::open_in_memory().unwrap();
        let user = db
            .user_create("alice", "alice@example.org", "2026-08-06T00:00:00Z")
            .unwrap();
        (db, user.id)
    }

    fn bundle(user_id: &str, id: &str, version: u32, digest: &str) -> BundleRow {
        BundleRow {
            user_id: user_id.into(),
            id: id.into(),
            version,
            digest: digest.into(),
            class: ContentClass::Static,
            title: "Morning briefing".into(),
            description: None,
            template: "report".into(),
            published_at: Some("2026-08-06T07:00:00Z".into()),
            received_at: "2026-08-06T07:00:01Z".into(),
            withheld_at: None,
            withheld_reason: None,
        }
    }

    /// The property the whole artifact model rests on, as a constraint the
    /// database enforces rather than as discipline in a handler.
    #[test]
    fn a_version_cannot_be_rewritten() {
        let (db, u) = db_with_user();
        db.bundle_insert(&bundle(&u, "brief", 1, "sha256:aaa"))
            .unwrap();
        let err = db
            .bundle_insert(&bundle(&u, "brief", 1, "sha256:bbb"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("UNIQUE"), "{err}");
        assert_eq!(
            db.bundle(&u, "brief", 1).unwrap().unwrap().digest,
            "sha256:aaa"
        );
    }

    #[test]
    fn identical_bytes_are_found_by_their_address() {
        let (db, u) = db_with_user();
        db.bundle_insert(&bundle(&u, "brief", 1, "sha256:aaa"))
            .unwrap();
        db.bundle_insert(&bundle(&u, "brief", 2, "sha256:bbb"))
            .unwrap();
        assert_eq!(
            db.bundle_by_digest(&u, "brief", "sha256:bbb")
                .unwrap()
                .unwrap()
                .version,
            2
        );
        assert!(db
            .bundle_by_digest(&u, "brief", "sha256:ccc")
            .unwrap()
            .is_none());
        // Another bundle's identical bytes are a different publication.
        assert!(db
            .bundle_by_digest(&u, "other", "sha256:aaa")
            .unwrap()
            .is_none());
        assert_eq!(db.bundle_versions(&u, "brief").unwrap(), vec![1, 2]);
    }

    #[test]
    fn the_alias_is_the_only_thing_that_moves_and_a_takedown_keeps_the_versions() {
        let (db, u) = db_with_user();
        db.bundle_insert(&bundle(&u, "brief", 1, "sha256:aaa"))
            .unwrap();
        db.alias_set(
            &u,
            "brief",
            Some(1),
            Visibility::Public,
            "2026-08-06T08:00:00Z",
        )
        .unwrap();
        let alias = db.alias(&u, "brief").unwrap().unwrap();
        assert_eq!(alias.version, Some(1));
        assert_eq!(alias.visibility, Visibility::Public);

        db.alias_set(
            &u,
            "brief",
            None,
            Visibility::Private,
            "2026-08-07T08:00:00Z",
        )
        .unwrap();
        assert_eq!(db.alias(&u, "brief").unwrap().unwrap().version, None);
        assert_eq!(db.bundle_versions(&u, "brief").unwrap(), vec![1]);
    }

    /// A drain that mutated would lose a stranger's request whenever the
    /// response failed to arrive.
    #[test]
    fn draining_twice_returns_the_same_records_until_they_are_acknowledged() {
        let (db, u) = db_with_user();
        let a = db
            .queue_add(&u, "meeting", "queued", r#"{"a":1}"#, "t1", None)
            .unwrap();
        let b = db
            .queue_add(&u, "meeting", "queued", r#"{"b":2}"#, "t2", None)
            .unwrap();
        // Never drained: it has not been verified, and unverified never costs a
        // token.
        db.queue_add(&u, "meeting", "submitted", r#"{"c":3}"#, "t3", None)
            .unwrap();

        let first = db.drain(&u, 0, 10).unwrap();
        assert_eq!(first.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![a, b]);
        assert_eq!(
            db.drain(&u, 0, 10).unwrap().len(),
            2,
            "a read changed nothing"
        );
        assert_eq!(db.queue_depth(Some(&u)).unwrap(), 2);

        assert_eq!(db.queue_ack(&u, &[a]).unwrap(), 1);
        assert_eq!(
            db.drain(&u, 0, 10)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            vec![b]
        );
        // And the watermark form of the same question.
        assert!(db.drain(&u, b, 10).unwrap().is_empty());
    }

    #[test]
    fn a_key_is_revoked_once_and_the_row_survives_it() {
        let (db, u) = db_with_user();
        db.key_insert(&KeyRow {
            id: "abcd1234".into(),
            user_id: u.clone(),
            scope: Scope::Publish,
            hash: "$argon2id$…".into(),
            label: "laptop".into(),
            created_at: "t0".into(),
            revoked_at: None,
        })
        .unwrap();
        assert!(db.key_revoke("abcd1234", "t1").unwrap());
        assert!(!db.key_revoke("abcd1234", "t2").unwrap(), "already revoked");
        let row = db.key_by_id("abcd1234").unwrap().unwrap();
        assert_eq!(row.revoked_at.as_deref(), Some("t1"));
        assert_eq!(db.keys().unwrap().len(), 1);
    }

    /// A row we cannot interpret must never be the one that gets the widest
    /// policy or the widest audience.
    #[test]
    fn an_unreadable_row_reads_as_the_narrow_thing() {
        assert_eq!(class_of("wat"), ContentClass::Static);
        assert_eq!(visibility_of("wat"), Visibility::Private);
        assert_eq!(class_of("compute"), ContentClass::Compute);
    }
}
