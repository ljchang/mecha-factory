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
const SCHEMA: i64 = 1;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    pub id: String,
    pub scope: Scope,
    pub hash: String,
    pub label: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

/// What a key may do. Two of them, mirroring the two forced-command SSH keys
/// this replaces — and the scope is read from **this row**, never from the
/// token's `mk_pub_` prefix, which is a human-readable label an attacker
/// controls the text of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Upload types, publish bundles, move aliases.
    Publish,
    /// Read the queue and acknowledge records.
    Drain,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Publish => "publish",
            Scope::Drain => "drain",
        }
    }

    pub fn parse(text: &str) -> Result<Scope> {
        match text {
            "publish" => Ok(Scope::Publish),
            "drain" => Ok(Scope::Drain),
            other => anyhow::bail!("unknown scope `{other}` (publish | drain)"),
        }
    }

    /// The prefix a token of this scope carries. Cosmetic: it tells a human
    /// which file they are looking at, and the server never trusts it.
    pub fn prefix(&self) -> &'static str {
        match self {
            Scope::Publish => "mk_pub_",
            Scope::Drain => "mk_drn_",
        }
    }
}

/// One published version, as the index knows it.
#[derive(Debug, Clone)]
pub struct BundleRow {
    pub id: String,
    pub version: u32,
    pub digest: String,
    pub class: mecha_manifest::ContentClass,
    pub title: String,
    pub description: Option<String>,
    pub template: String,
    pub published_at: Option<String>,
    pub received_at: String,
}

#[derive(Debug, Clone)]
pub struct AliasRow {
    pub version: Option<u32>,
    pub visibility: mecha_manifest::Visibility,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct TypeRow {
    pub id: String,
    pub title: String,
    pub manifest: String,
    pub schema: String,
    pub updated_at: String,
}

/// A typed request as it sits on the box, before home has ever seen it.
#[derive(Debug, Clone)]
pub struct QueueRow {
    pub seq: i64,
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
        migrate(&conn)?;
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

    // ---- keys -----------------------------------------------------------

    pub fn key_insert(&self, row: &KeyRow) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO keys (id, scope, hash, label, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    row.id,
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
                    "SELECT id, scope, hash, label, created_at, revoked_at FROM keys WHERE id = ?1",
                    params![id],
                    key_row,
                )
                .optional()?)
        })
    }

    pub fn keys(&self) -> Result<Vec<KeyRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, scope, hash, label, created_at, revoked_at FROM keys ORDER BY created_at",
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
                "INSERT INTO bundles (id, version, digest, class, title, description, template, \
                 published_at, received_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
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

    pub fn bundle(&self, id: &str, version: u32) -> Result<Option<BundleRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, version, digest, class, title, description, template, \
                     published_at, received_at FROM bundles WHERE id = ?1 AND version = ?2",
                    params![id, version],
                    bundle_row,
                )
                .optional()?)
        })
    }

    /// The content-addressed lookup: has this id already been published with
    /// exactly these bytes? A yes means a republish mints nothing.
    pub fn bundle_by_digest(&self, id: &str, digest: &str) -> Result<Option<BundleRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, version, digest, class, title, description, template, \
                     published_at, received_at FROM bundles WHERE id = ?1 AND digest = ?2 \
                     ORDER BY version LIMIT 1",
                    params![id, digest],
                    bundle_row,
                )
                .optional()?)
        })
    }

    pub fn bundle_versions(&self, id: &str) -> Result<Vec<u32>> {
        self.with(|conn| {
            let mut stmt =
                conn.prepare("SELECT version FROM bundles WHERE id = ?1 ORDER BY version")?;
            let rows = stmt.query_map(params![id], |r| r.get::<_, u32>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn bundle_count(&self) -> Result<i64> {
        self.with(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM bundles", [], |r| r.get(0))?))
    }

    // ---- aliases --------------------------------------------------------

    pub fn alias(&self, id: &str) -> Result<Option<AliasRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT version, visibility, updated_at FROM aliases WHERE id = ?1",
                    params![id],
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
        id: &str,
        version: Option<u32>,
        visibility: mecha_manifest::Visibility,
        now: &str,
    ) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO aliases (id, version, visibility, updated_at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(id) DO UPDATE SET version = ?2, visibility = ?3, updated_at = ?4",
                params![
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
                "INSERT INTO types (id, title, manifest, schema, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id) DO UPDATE SET \
                 title = ?2, manifest = ?3, schema = ?4, updated_at = ?5",
                params![row.id, row.title, row.manifest, row.schema, row.updated_at],
            )?;
            Ok(())
        })
    }

    pub fn type_get(&self, id: &str) -> Result<Option<TypeRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, title, manifest, schema, updated_at FROM types WHERE id = ?1",
                    params![id],
                    type_row,
                )
                .optional()?)
        })
    }

    pub fn types(&self) -> Result<Vec<TypeRow>> {
        self.with(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, title, manifest, schema, updated_at FROM types ORDER BY id")?;
            let rows = stmt.query_map([], type_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- the queue ------------------------------------------------------

    pub fn queue_add(&self, type_id: &str, state: &str, payload: &str, now: &str) -> Result<i64> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO queue (type_id, state, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![type_id, state, payload, now],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Everything queued after `since`, oldest first.
    ///
    /// **A pure read.** Marking a row drained here would lose it whenever the
    /// response failed to arrive, and the whole point of the queue is that a
    /// stranger's request cannot silently evaporate. Home acknowledges what it
    /// has stored, and until then the row comes back.
    pub fn drain(&self, since: i64, limit: usize) -> Result<Vec<QueueRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq, type_id, state, payload, created_at FROM queue \
                 WHERE seq > ?1 AND state = 'queued' ORDER BY seq LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![since, limit as i64], |r| {
                Ok(QueueRow {
                    seq: r.get(0)?,
                    type_id: r.get(1)?,
                    state: r.get(2)?,
                    payload: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Delete exactly the sequence numbers home says it has.
    ///
    /// By list rather than by watermark: a watermark deletes rows nobody named,
    /// and the failure is silent. Returns how many rows actually went.
    pub fn queue_ack(&self, seqs: &[i64]) -> Result<usize> {
        self.with(|conn| {
            let mut removed = 0;
            let tx = conn.unchecked_transaction()?;
            for seq in seqs {
                removed += tx.execute("DELETE FROM queue WHERE seq = ?1", params![seq])?;
            }
            tx.commit()?;
            Ok(removed)
        })
    }

    pub fn queue_depth(&self) -> Result<i64> {
        self.with(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM queue WHERE state = 'queued'",
                [],
                |r| r.get(0),
            )?)
        })
    }

    // ---- idempotency ----------------------------------------------------

    pub fn idempotent(&self, key: &str) -> Result<Option<(String, u32)>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT bundle_id, version FROM idempotency WHERE key = ?1",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?)
        })
    }

    pub fn idempotency_record(&self, key: &str, id: &str, version: u32, now: &str) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO idempotency (key, bundle_id, version, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![key, id, version, now],
            )?;
            Ok(())
        })
    }
}

fn key_row(r: &rusqlite::Row) -> rusqlite::Result<KeyRow> {
    Ok(KeyRow {
        id: r.get(0)?,
        // An unreadable scope is treated as the narrower one rather than as an
        // error, because the alternative is an unparseable row locking out a
        // legitimate key. `drain` cannot publish.
        scope: Scope::parse(&r.get::<_, String>(1)?).unwrap_or(Scope::Drain),
        hash: r.get(2)?,
        label: r.get(3)?,
        created_at: r.get(4)?,
        revoked_at: r.get(5)?,
    })
}

fn bundle_row(r: &rusqlite::Row) -> rusqlite::Result<BundleRow> {
    Ok(BundleRow {
        id: r.get(0)?,
        version: r.get(1)?,
        digest: r.get(2)?,
        class: class_of(&r.get::<_, String>(3)?),
        title: r.get(4)?,
        description: r.get(5)?,
        template: r.get(6)?,
        published_at: r.get(7)?,
        received_at: r.get(8)?,
    })
}

fn type_row(r: &rusqlite::Row) -> rusqlite::Result<TypeRow> {
    Ok(TypeRow {
        id: r.get(0)?,
        title: r.get(1)?,
        manifest: r.get(2)?,
        schema: r.get(3)?,
        updated_at: r.get(4)?,
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
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS keys (
            id          TEXT PRIMARY KEY,
            scope       TEXT NOT NULL,
            hash        TEXT NOT NULL,
            label       TEXT NOT NULL DEFAULT '',
            created_at  TEXT NOT NULL,
            revoked_at  TEXT
        );

        CREATE TABLE IF NOT EXISTS bundles (
            id           TEXT NOT NULL,
            version      INTEGER NOT NULL,
            digest       TEXT NOT NULL,
            class        TEXT NOT NULL,
            title        TEXT NOT NULL,
            description  TEXT,
            template     TEXT NOT NULL,
            published_at TEXT,
            received_at  TEXT NOT NULL,
            PRIMARY KEY (id, version)
        );
        CREATE INDEX IF NOT EXISTS bundles_by_digest ON bundles (id, digest);

        CREATE TABLE IF NOT EXISTS aliases (
            id          TEXT PRIMARY KEY,
            version     INTEGER,
            visibility  TEXT NOT NULL DEFAULT 'private',
            updated_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS types (
            id          TEXT PRIMARY KEY,
            title       TEXT NOT NULL DEFAULT '',
            manifest    TEXT NOT NULL,
            schema      TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS queue (
            seq         INTEGER PRIMARY KEY AUTOINCREMENT,
            type_id     TEXT NOT NULL,
            state       TEXT NOT NULL,
            payload     TEXT NOT NULL,
            created_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS queue_by_state ON queue (state, seq);

        CREATE TABLE IF NOT EXISTS idempotency (
            key         TEXT PRIMARY KEY,
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

#[cfg(test)]
mod tests {
    use super::*;
    use mecha_manifest::{ContentClass, Visibility};

    fn bundle(id: &str, version: u32, digest: &str) -> BundleRow {
        BundleRow {
            id: id.into(),
            version,
            digest: digest.into(),
            class: ContentClass::Static,
            title: "Morning briefing".into(),
            description: None,
            template: "report".into(),
            published_at: Some("2026-08-06T07:00:00Z".into()),
            received_at: "2026-08-06T07:00:01Z".into(),
        }
    }

    /// The property the whole artifact model rests on, as a constraint the
    /// database enforces rather than as discipline in a handler.
    #[test]
    fn a_version_cannot_be_rewritten() {
        let db = Db::open_in_memory().unwrap();
        db.bundle_insert(&bundle("brief", 1, "sha256:aaa")).unwrap();
        let err = db
            .bundle_insert(&bundle("brief", 1, "sha256:bbb"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("UNIQUE"), "{err}");
        assert_eq!(db.bundle("brief", 1).unwrap().unwrap().digest, "sha256:aaa");
    }

    #[test]
    fn identical_bytes_are_found_by_their_address() {
        let db = Db::open_in_memory().unwrap();
        db.bundle_insert(&bundle("brief", 1, "sha256:aaa")).unwrap();
        db.bundle_insert(&bundle("brief", 2, "sha256:bbb")).unwrap();
        assert_eq!(
            db.bundle_by_digest("brief", "sha256:bbb")
                .unwrap()
                .unwrap()
                .version,
            2
        );
        assert!(db
            .bundle_by_digest("brief", "sha256:ccc")
            .unwrap()
            .is_none());
        // Another bundle's identical bytes are a different publication.
        assert!(db
            .bundle_by_digest("other", "sha256:aaa")
            .unwrap()
            .is_none());
        assert_eq!(db.bundle_versions("brief").unwrap(), vec![1, 2]);
    }

    #[test]
    fn the_alias_is_the_only_thing_that_moves_and_a_takedown_keeps_the_versions() {
        let db = Db::open_in_memory().unwrap();
        db.bundle_insert(&bundle("brief", 1, "sha256:aaa")).unwrap();
        db.alias_set("brief", Some(1), Visibility::Public, "2026-08-06T08:00:00Z")
            .unwrap();
        let alias = db.alias("brief").unwrap().unwrap();
        assert_eq!(alias.version, Some(1));
        assert_eq!(alias.visibility, Visibility::Public);

        db.alias_set("brief", None, Visibility::Private, "2026-08-07T08:00:00Z")
            .unwrap();
        assert_eq!(db.alias("brief").unwrap().unwrap().version, None);
        assert_eq!(db.bundle_versions("brief").unwrap(), vec![1]);
    }

    /// A drain that mutated would lose a stranger's request whenever the
    /// response failed to arrive.
    #[test]
    fn draining_twice_returns_the_same_records_until_they_are_acknowledged() {
        let db = Db::open_in_memory().unwrap();
        let a = db
            .queue_add("meeting", "queued", r#"{"a":1}"#, "t1")
            .unwrap();
        let b = db
            .queue_add("meeting", "queued", r#"{"b":2}"#, "t2")
            .unwrap();
        // Never drained: it has not been verified, and unverified never costs a
        // token.
        db.queue_add("meeting", "submitted", r#"{"c":3}"#, "t3")
            .unwrap();

        let first = db.drain(0, 10).unwrap();
        assert_eq!(first.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![a, b]);
        assert_eq!(db.drain(0, 10).unwrap().len(), 2, "a read changed nothing");
        assert_eq!(db.queue_depth().unwrap(), 2);

        assert_eq!(db.queue_ack(&[a]).unwrap(), 1);
        assert_eq!(
            db.drain(0, 10)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            vec![b]
        );
        // And the watermark form of the same question.
        assert!(db.drain(b, 10).unwrap().is_empty());
    }

    #[test]
    fn a_key_is_revoked_once_and_the_row_survives_it() {
        let db = Db::open_in_memory().unwrap();
        db.key_insert(&KeyRow {
            id: "abcd1234".into(),
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
