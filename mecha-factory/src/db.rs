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
const SCHEMA: i64 = 9;

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
    /// Stamped on every authenticated call, best-effort. What lets the
    /// machine list say "alive last Tuesday" instead of only "minted in
    /// June" — a silent compromise shows up as life where none was expected.
    pub last_used_at: Option<String>,
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

/// One minted right to claim a handle, in whatever state it has reached.
#[derive(Debug, Clone)]
pub struct InviteRow {
    pub id: String,
    /// Where the link was sent — and the address the claimed account gets,
    /// because clicking a link that arrived there is what proved it.
    pub email: String,
    /// The operator's own note: who this is, so `invite list` means something
    /// a month later.
    pub note: String,
    pub created_at: String,
    pub expires_at: String,
    pub claimed_at: Option<String>,
    pub claimed_by: Option<String>,
    pub revoked_at: Option<String>,
}

impl InviteRow {
    /// Derived rather than stored: a state column would be one more thing to
    /// keep in step with the timestamps that already say it.
    pub fn status(&self, now: &str) -> &'static str {
        if self.claimed_at.is_some() {
            "claimed"
        } else if self.revoked_at.is_some() {
            "revoked"
        } else if self.expires_at.as_str() <= now {
            "expired"
        } else {
            "pending"
        }
    }

    fn live(&self, now: &str) -> bool {
        self.status(now) == "pending"
    }
}

/// What became of a signup's attempt to claim a handle.
///
/// An enum rather than error strings, because the page a stranger gets hangs
/// on the difference: a taken handle is *their form back with an error on it*
/// (the invite is still good), where a dead invite is the same nothing-page
/// every dead invite gets. Matching on `anyhow` text to tell those apart is
/// the kind of seam that breaks silently when a message is reworded.
#[derive(Debug)]
pub enum Claim {
    Created(UserRow),
    /// Somebody holds it, or once did. Which of the two is not a stranger's
    /// business — see `user_create` for why the CLI, whose caller is the
    /// operator, does get the difference.
    HandleTaken,
    /// Claimed, revoked, expired, or never real — one variant for all four,
    /// for the same reason the page is one page.
    InviteGone,
}

/// One bundle as the owner's page lists it.
#[derive(Debug, Clone)]
pub struct BundleSummary {
    pub id: String,
    pub latest: u32,
    pub title: String,
    /// What the share URL resolves to, if anything.
    pub aliased: Option<u32>,
    pub visibility: mecha_manifest::Visibility,
}

/// What became of a machine's attempt to redeem a pairing code.
///
/// Two variants where [`Claim`] has three, and that is the design: a wrong
/// handle assertion, an expired code, a spent code and a code that never
/// existed are all the same refusal, because telling a wrong assertion apart
/// from a dead code would let whoever holds a stolen code probe for the
/// handle it belongs to.
#[derive(Debug)]
pub enum Paired {
    Redeemed(UserRow),
    Refused,
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
    /// Replace an instrument's slot cache — the availability a booking page
    /// serves. Its own scope rather than a use of `Publish`, because the key
    /// sits in a systemd timer's environment on a schedule with no human
    /// near it: the worst a stolen one does is misstate when the user is
    /// free, and it must not also be able to write versions, move aliases,
    /// or read anything at all.
    Slots,
    /// Run the box: users, invites, every key, withholds, queue depths.
    ///
    /// The operator's credential — what retires the SSH session from routine
    /// operation. Bound to no tenant (its key row has an empty `user_id`),
    /// which is exactly why the tenant authoriser must never accept it and
    /// the admin authoriser never joins on a user.
    Operate,
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
    pub const ALL: [Scope; 5] = [
        Scope::Publish,
        Scope::Release,
        Scope::Drain,
        Scope::Slots,
        Scope::Operate,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Publish => "publish",
            Scope::Release => "release",
            Scope::Drain => "drain",
            Scope::Slots => "slots",
            Scope::Operate => "operate",
        }
    }

    pub fn parse(text: &str) -> Result<Scope> {
        match text {
            "publish" => Ok(Scope::Publish),
            "release" => Ok(Scope::Release),
            "drain" => Ok(Scope::Drain),
            "slots" => Ok(Scope::Slots),
            "operate" => Ok(Scope::Operate),
            other => anyhow::bail!(
                "unknown scope `{other}` (publish | release | drain | slots | operate)"
            ),
        }
    }

    /// The prefix a token of this scope carries. Cosmetic: it tells a human
    /// which file they are looking at, and the server never trusts it.
    pub fn prefix(&self) -> &'static str {
        match self {
            Scope::Publish => "mk_pub_",
            Scope::Release => "mk_rel_",
            Scope::Drain => "mk_drn_",
            Scope::Slots => "mk_slt_",
            Scope::Operate => "mk_opr_",
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

/// One withheld version, as the operator's panel lists it.
#[derive(Debug, Clone)]
pub struct WithheldRow {
    pub handle: String,
    pub id: String,
    pub version: u32,
    pub withheld_at: String,
    pub reason: Option<String>,
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

/// One instrument's cached availability, exactly as home pushed it.
#[derive(Debug, Clone)]
pub struct SlotCacheRow {
    pub user_id: String,
    pub instrument_id: String,
    /// Home's stamp: when the slots were computed, not when they arrived.
    pub generated_at: String,
    pub horizon_days: i64,
    /// A JSON array of `{start, end, duration_minutes}`, shape-validated at
    /// the endpoint before it is stored.
    pub slots: String,
    pub received_at: String,
}

/// One booking row, hold or later.
#[derive(Debug, Clone)]
pub struct BookingRow {
    pub id: String,
    pub user_id: String,
    pub instrument_id: String,
    pub slot_start: String,
    pub slot_end: String,
    pub duration_minutes: i64,
    pub state: String,
    pub hold_expires: Option<String>,
    pub queue_seq: Option<i64>,
    pub manage_hash: Option<String>,
    pub ics_sequence: i64,
    pub created_at: String,
    pub confirmed_at: Option<String>,
    pub cancelled_at: Option<String>,
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

/// One uploaded file's ledger row. The bytes live in the attachment store
/// under `id`; everything here is what the box measured about them, plus the
/// stranger's claimed filename, carried for the human who will open the file
/// and for nobody else.
#[derive(Debug, Clone)]
pub struct AttachmentRow {
    pub id: String,
    pub user_id: String,
    pub seq: i64,
    pub field: String,
    pub filename: String,
    pub content_type: String,
    pub size: i64,
    pub sha256: String,
    pub created_at: String,
}

/// Where a spent verification token sends the row: straight to the queue, or
/// through the upload step first. The caller decides — it holds the manifest
/// and knows whether the type asks for files — and the ledger just moves.
#[derive(Debug, Clone)]
pub enum VerifyNext {
    Queued,
    AwaitingUpload {
        upload_hash: String,
        upload_expires: String,
    },
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
            create_user_in(&tx, &id, handle, email, now)?;
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

    /// Every active account behind an address, for sign-in. Plural because
    /// nothing makes emails unique — one person, several handles — and a
    /// sign-in link binds to exactly one account, so the sender loops.
    pub fn users_by_email(&self, email: &str) -> Result<Vec<UserRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, handle, email, status, created_at, quota_bytes, send_budget \
                 FROM users WHERE LOWER(email) = LOWER(?1) AND status = 'active' \
                 ORDER BY created_at",
            )?;
            let rows = stmt.query_map(params![email.trim()], user_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- invites --------------------------------------------------------

    /// Mint the right to claim one handle. The caller holds the token; this
    /// holds its hash, exactly as the keys and the verification links do.
    pub fn invite_create(
        &self,
        email: &str,
        note: &str,
        token_hash: &str,
        now: &str,
        expires_at: &str,
    ) -> Result<InviteRow> {
        let id = crate::keys::random_id();
        self.with(|conn| {
            conn.execute(
                "INSERT INTO invites (id, email, note, token_hash, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, email, note, token_hash, now, expires_at],
            )?;
            Ok(())
        })?;
        self.invites()?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or_else(|| anyhow::anyhow!("the invite vanished between writing and reading it"))
    }

    pub fn invites(&self) -> Result<Vec<InviteRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, email, note, created_at, expires_at, claimed_at, claimed_by, \
                 revoked_at FROM invites ORDER BY created_at",
            )?;
            let rows = stmt.query_map([], invite_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Stop an unclaimed invite working. A claimed one is history, not policy,
    /// and refusing to touch it is what keeps `claimed_by` meaning something.
    pub fn invite_revoke(&self, id: &str, now: &str) -> Result<bool> {
        self.with(|conn| {
            let n = conn.execute(
                "UPDATE invites SET revoked_at = ?2 \
                 WHERE id = ?1 AND claimed_at IS NULL AND revoked_at IS NULL",
                params![id, now],
            )?;
            Ok(n > 0)
        })
    }

    /// The live invite this token names, or nothing — where claimed, revoked,
    /// expired and never-existed are all the same nothing, because the page
    /// they produce is the same page.
    pub fn invite_by_token(&self, token_hash: &str, now: &str) -> Result<Option<InviteRow>> {
        Ok(self
            .with(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT id, email, note, created_at, expires_at, claimed_at, \
                         claimed_by, revoked_at FROM invites WHERE token_hash = ?1",
                        params![token_hash],
                        invite_row,
                    )
                    .optional()?)
            })?
            .filter(|row: &InviteRow| row.live(now)))
    }

    /// Spend an invite on a handle: the signup's one write.
    ///
    /// One transaction re-checks the invite is still live, claims the handle
    /// through the same path `user_create` uses, and marks the invite spent —
    /// so two clicks on one link race to a single account, and a claim that
    /// fails on the handle leaves the invite good for another try. The caller
    /// has already validated the handle's shape; what is decided here is only
    /// what needs the ledger to decide.
    pub fn invite_claim(&self, token_hash: &str, handle: &str, now: &str) -> Result<Claim> {
        let id = crate::keys::random_id();
        let outcome = self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            let invite = tx
                .query_row(
                    "SELECT id, email, note, created_at, expires_at, claimed_at, \
                     claimed_by, revoked_at FROM invites WHERE token_hash = ?1",
                    params![token_hash],
                    invite_row,
                )
                .optional()?;
            let Some(invite) = invite.filter(|row| row.live(now)) else {
                return Ok(None);
            };
            if handle_owner(&tx, handle)?.is_some() {
                return Ok(Some(Err(())));
            }
            create_user_in(&tx, &id, handle, &invite.email, now)?;
            tx.execute(
                "UPDATE invites SET claimed_at = ?2, claimed_by = ?3 WHERE id = ?1",
                params![invite.id, now, id],
            )?;
            tx.commit()?;
            Ok(Some(Ok(())))
        })?;
        match outcome {
            None => Ok(Claim::InviteGone),
            Some(Err(())) => Ok(Claim::HandleTaken),
            Some(Ok(())) => {
                let user = self.user(&id)?.ok_or_else(|| {
                    anyhow::anyhow!("the user vanished between writing and reading it")
                })?;
                Ok(Claim::Created(user))
            }
        }
    }

    // ---- pairings -------------------------------------------------------

    /// Mint the right to connect one machine. Returns the pairing id.
    pub fn pairing_create(
        &self,
        user_id: &str,
        code_hash: &str,
        now: &str,
        expires_at: &str,
    ) -> Result<String> {
        let id = crate::keys::random_id();
        self.with(|conn| {
            conn.execute(
                "INSERT INTO pairings (id, user_id, code_hash, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, user_id, code_hash, now, expires_at],
            )?;
            Ok(())
        })?;
        Ok(id)
    }

    /// Spend a pairing code on two keys — if everything about it holds.
    ///
    /// The caller prepared the key rows first (hashing is compute, not
    /// ledger), and this is one transaction: the code is live, the user is
    /// active, and the **asserted handle matches** — the assertion is checked
    /// here, on the server, so no client can skip it and a `y` piped into a
    /// prompt has nothing to defeat. On any refusal nothing is written and
    /// nothing is revealed, including whether the code exists: a wrong
    /// assertion must not become the probe that confirms a stolen code is
    /// real.
    pub fn pairing_redeem(
        &self,
        code_hash: &str,
        asserted_handle: &str,
        publish: &KeyRow,
        drain: &KeyRow,
        now: &str,
    ) -> Result<Paired> {
        let redeemed = self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            let pairing = tx
                .query_row(
                    "SELECT id, user_id, expires_at, redeemed_at FROM pairings \
                     WHERE code_hash = ?1",
                    params![code_hash],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((pairing_id, user_id, expires_at, redeemed_at)) = pairing else {
                return Ok(None);
            };
            if redeemed_at.is_some() || expires_at.as_str() <= now {
                return Ok(None);
            }
            let user = tx
                .query_row(
                    "SELECT id, handle, email, status, created_at, quota_bytes, send_budget \
                     FROM users WHERE id = ?1",
                    params![user_id],
                    user_row,
                )
                .optional()?;
            let Some(user) = user.filter(|u| u.active() && u.handle == asserted_handle) else {
                return Ok(None);
            };

            for row in [publish, drain] {
                tx.execute(
                    "INSERT INTO keys (id, user_id, scope, hash, label, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        row.id,
                        user.id,
                        row.scope.as_str(),
                        row.hash,
                        row.label,
                        row.created_at
                    ],
                )?;
            }
            tx.execute(
                "UPDATE pairings SET redeemed_at = ?2, publish_key_id = ?3, \
                 drain_key_id = ?4 WHERE id = ?1",
                params![pairing_id, now, publish.id, drain.id],
            )?;
            tx.commit()?;
            Ok(Some(user))
        })?;
        Ok(match redeemed {
            Some(user) => Paired::Redeemed(user),
            None => Paired::Refused,
        })
    }

    /// Expired, unredeemed pairing codes: gone. What `sweep` calls — a spent
    /// code is a record (it says which keys a machine connected with), where
    /// an expired one is machinery that never did anything.
    pub fn expire_pairings(&self, now: &str) -> Result<usize> {
        self.with(|conn| {
            let n = conn.execute(
                "DELETE FROM pairings WHERE redeemed_at IS NULL AND expires_at <= ?1",
                params![now],
            )?;
            Ok(n)
        })
    }

    /// One user's bundles as their page lists them: latest version, what the
    /// share URL points at, and who may read it.
    pub fn bundles_overview(&self, user_id: &str) -> Result<Vec<BundleSummary>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT b.id, MAX(b.version), MAX(b.title), a.version, a.visibility \
                 FROM bundles b LEFT JOIN aliases a \
                 ON a.user_id = b.user_id AND a.id = b.id \
                 WHERE b.user_id = ?1 GROUP BY b.id ORDER BY b.id",
            )?;
            let rows = stmt.query_map(params![user_id], |r| {
                Ok(BundleSummary {
                    id: r.get(0)?,
                    latest: r.get::<_, i64>(1)? as u32,
                    title: r.get(2)?,
                    aliased: r.get::<_, Option<i64>>(3)?.map(|v| v as u32),
                    visibility: match r.get::<_, Option<String>>(4)?.as_deref() {
                        Some("public") => mecha_manifest::Visibility::Public,
                        _ => mecha_manifest::Visibility::Private,
                    },
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- sessions -------------------------------------------------------

    /// Record a sign-in link. The budget check is the caller's; this is the
    /// write.
    pub fn signin_link_create(
        &self,
        user_id: &str,
        token_hash: &str,
        now: &str,
        expires_at: &str,
    ) -> Result<()> {
        let id = crate::keys::random_id();
        self.with(|conn| {
            conn.execute(
                "INSERT INTO signin_links (id, user_id, token_hash, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, user_id, token_hash, now, expires_at],
            )?;
            Ok(())
        })
    }

    /// Sign-in links minted for a user today — the budget's denominator.
    pub fn signin_links_today(&self, user_id: &str, today: &str) -> Result<i64> {
        self.with(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM signin_links WHERE user_id = ?1 \
                 AND substr(created_at, 1, 10) = ?2",
                params![user_id, today],
                |r| r.get(0),
            )?)
        })
    }

    /// Spend a sign-in link and mint its session — one transaction, the
    /// same shape as [`Db::operator_signin`] and for the same reason: the
    /// link is consumed only when the session lands, so a failure between
    /// the two cannot burn it, and the "expired" page a retry sees is only
    /// ever the truth. The redeem also demands the user still active, so a
    /// suspended account's link is a dead link that never spends. Two
    /// methods rather than one parameterised over the tables, because the
    /// tables are the boundary between the two session surfaces — SQL
    /// cannot parameterise a table name, and this code should not try.
    /// Returns whose session it now is.
    pub fn signin(
        &self,
        link_hash: &str,
        session_hash: &str,
        now: &str,
        session_expires: &str,
    ) -> Result<Option<String>> {
        let id = crate::keys::random_id();
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            let user_id: Option<String> = tx
                .query_row(
                    "UPDATE signin_links SET used_at = ?2 \
                     WHERE token_hash = ?1 AND used_at IS NULL AND expires_at > ?2 \
                     AND user_id IN (SELECT id FROM users WHERE status = 'active') \
                     RETURNING user_id",
                    params![link_hash, now],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(user_id) = &user_id {
                tx.execute(
                    "INSERT INTO sessions (id, user_id, token_hash, created_at, expires_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, user_id, session_hash, now, session_expires],
                )?;
            }
            tx.commit()?;
            Ok(user_id)
        })
    }

    pub fn session_create(
        &self,
        user_id: &str,
        token_hash: &str,
        now: &str,
        expires_at: &str,
    ) -> Result<()> {
        let id = crate::keys::random_id();
        self.with(|conn| {
            conn.execute(
                "INSERT INTO sessions (id, user_id, token_hash, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, user_id, token_hash, now, expires_at],
            )?;
            Ok(())
        })
    }

    /// The active user behind a live session, or nothing. Suspension ends
    /// sessions with everything else: the join is on `active`, so there is no
    /// window where a suspended account still drives a signed-in page.
    pub fn session_user(&self, token_hash: &str, now: &str) -> Result<Option<UserRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT u.id, u.handle, u.email, u.status, u.created_at, \
                     u.quota_bytes, u.send_budget FROM sessions s \
                     JOIN users u ON u.id = s.user_id \
                     WHERE s.token_hash = ?1 AND s.revoked_at IS NULL \
                     AND s.expires_at > ?2 AND u.status = 'active'",
                    params![token_hash, now],
                    user_row,
                )
                .optional()?)
        })
    }

    /// Sign out: the session stops working now, whatever the cookie says.
    pub fn session_revoke(&self, token_hash: &str, now: &str) -> Result<bool> {
        self.with(|conn| {
            let n = conn.execute(
                "UPDATE sessions SET revoked_at = ?2 \
                 WHERE token_hash = ?1 AND revoked_at IS NULL",
                params![token_hash, now],
            )?;
            Ok(n > 0)
        })
    }

    /// Expired sessions and sign-in links: gone, via `sweep`. Revoked or
    /// spent rows within their window stay — they are the recent record.
    pub fn expire_sessions(&self, now: &str) -> Result<usize> {
        self.with(|conn| {
            let a = conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", params![now])?;
            let b = conn.execute(
                "DELETE FROM signin_links WHERE expires_at <= ?1",
                params![now],
            )?;
            let c = conn.execute(
                "DELETE FROM operator_sessions WHERE expires_at <= ?1",
                params![now],
            )?;
            let d = conn.execute(
                "DELETE FROM operator_links WHERE expires_at <= ?1",
                params![now],
            )?;
            Ok(a + b + c + d)
        })
    }

    // ---- the operator's sessions ----------------------------------------
    //
    // The same three verbs as the tenant ones, against their own tables, and
    // the difference is what each joins on: a tenant session is a user
    // signed in, an operator session is a *key* signed in. Nothing here
    // reads `users` at all, which is what "shares nothing with tenant
    // sessions" means as a property of queries rather than of intention.

    /// Record a one-time browser link for an operate key.
    pub fn operator_link_create(
        &self,
        key_id: &str,
        token_hash: &str,
        now: &str,
        expires_at: &str,
    ) -> Result<()> {
        let id = crate::keys::random_id();
        self.with(|conn| {
            conn.execute(
                "INSERT INTO operator_links (id, key_id, token_hash, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, key_id, token_hash, now, expires_at],
            )?;
            Ok(())
        })
    }

    /// Spend an operator link and mint its session — one transaction, so a
    /// failure after the redeem cannot burn the link: it is consumed only
    /// when the session lands, and retrying a failed click still works. The
    /// redeem also demands the minting key still live and still `operate`,
    /// so a link whose key died between minting and clicking is a dead link
    /// that never spends. Returns the key id, for the log line.
    pub fn operator_signin(
        &self,
        link_hash: &str,
        session_hash: &str,
        now: &str,
        session_expires: &str,
    ) -> Result<Option<String>> {
        let id = crate::keys::random_id();
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            let key_id: Option<String> = tx
                .query_row(
                    "UPDATE operator_links SET used_at = ?2 \
                     WHERE token_hash = ?1 AND used_at IS NULL AND expires_at > ?2 \
                     AND key_id IN (SELECT id FROM keys \
                                    WHERE revoked_at IS NULL AND scope = 'operate') \
                     RETURNING key_id",
                    params![link_hash, now],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(key_id) = &key_id {
                tx.execute(
                    "INSERT INTO operator_sessions \
                     (id, key_id, token_hash, created_at, expires_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, key_id, session_hash, now, session_expires],
                )?;
            }
            tx.commit()?;
            Ok(key_id)
        })
    }

    /// The live operate key behind an operator session, or nothing. The
    /// session dies with its key — the join demands the key unrevoked and
    /// still `operate` — so break-glass revocation ends the browser too, the
    /// way suspension ends a tenant's page.
    pub fn operator_session_key(&self, token_hash: &str, now: &str) -> Result<Option<KeyRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT k.id, k.user_id, k.scope, k.hash, k.label, k.created_at, \
                     k.revoked_at, k.last_used_at FROM operator_sessions s \
                     JOIN keys k ON k.id = s.key_id \
                     WHERE s.token_hash = ?1 AND s.revoked_at IS NULL \
                     AND s.expires_at > ?2 AND k.revoked_at IS NULL \
                     AND k.scope = 'operate'",
                    params![token_hash, now],
                    key_row,
                )
                .optional()?)
        })
    }

    /// Sign out: the operator session stops working now.
    pub fn operator_session_revoke(&self, token_hash: &str, now: &str) -> Result<bool> {
        self.with(|conn| {
            let n = conn.execute(
                "UPDATE operator_sessions SET revoked_at = ?2 \
                 WHERE token_hash = ?1 AND revoked_at IS NULL",
                params![token_hash, now],
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
                    "SELECT id, user_id, scope, hash, label, created_at, revoked_at, \
                     last_used_at FROM keys WHERE id = ?1",
                    params![id],
                    key_row,
                )
                .optional()?)
        })
    }

    pub fn keys(&self) -> Result<Vec<KeyRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, user_id, scope, hash, label, created_at, revoked_at, \
                 last_used_at FROM keys ORDER BY created_at",
            )?;
            let rows = stmt.query_map([], key_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Stamp a key as used. Best-effort by contract: authentication must not
    /// fail because a timestamp could not be written.
    pub fn key_touch(&self, id: &str, now: &str) {
        let _ = self.with(|conn| {
            conn.execute(
                "UPDATE keys SET last_used_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
            Ok(())
        });
    }

    /// One user's keys, for the machine list on their page.
    pub fn keys_for_user(&self, user_id: &str) -> Result<Vec<KeyRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, user_id, scope, hash, label, created_at, revoked_at, \
                 last_used_at FROM keys WHERE user_id = ?1 ORDER BY created_at",
            )?;
            let rows = stmt.query_map(params![user_id], key_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Revoke a key **belonging to this user**. The tenant page passes the
    /// session's user, so a form that names somebody else's key id revokes
    /// nothing — ownership is the WHERE clause, not a check before it.
    pub fn key_revoke_for(&self, user_id: &str, id: &str, now: &str) -> Result<bool> {
        self.with(|conn| {
            let n = conn.execute(
                "UPDATE keys SET revoked_at = ?3 \
                 WHERE id = ?1 AND user_id = ?2 AND revoked_at IS NULL",
                params![id, user_id, now],
            )?;
            Ok(n > 0)
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

    /// Every version currently withheld, with whose it is — the operator's
    /// review list, so a withhold flipped months ago is a row on the panel
    /// rather than a memory.
    pub fn withheld(&self) -> Result<Vec<WithheldRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT u.handle, b.id, b.version, b.withheld_at, b.withheld_reason \
                 FROM bundles b JOIN users u ON u.id = b.user_id \
                 WHERE b.withheld_at IS NOT NULL ORDER BY b.withheld_at",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(WithheldRow {
                    handle: r.get(0)?,
                    id: r.get(1)?,
                    version: r.get(2)?,
                    withheld_at: r.get(3)?,
                    reason: r.get(4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
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

    // ---- slot caches ----------------------------------------------------

    /// Replace an instrument's slot cache. One statement, so a reader never
    /// sees half an update; the previous cache is gone the moment this lands,
    /// because two generations of availability shown together would offer
    /// slots home has already withdrawn.
    pub fn slots_put(&self, row: &SlotCacheRow) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO slot_cache \
                 (user_id, instrument_id, generated_at, horizon_days, slots, received_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(user_id, instrument_id) DO UPDATE SET \
                 generated_at = ?3, horizon_days = ?4, slots = ?5, received_at = ?6",
                params![
                    row.user_id,
                    row.instrument_id,
                    row.generated_at,
                    row.horizon_days,
                    row.slots,
                    row.received_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn slots_get(&self, user_id: &str, instrument_id: &str) -> Result<Option<SlotCacheRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT user_id, instrument_id, generated_at, horizon_days, slots, \
                     received_at FROM slot_cache WHERE user_id = ?1 AND instrument_id = ?2",
                    params![user_id, instrument_id],
                    |r| {
                        Ok(SlotCacheRow {
                            user_id: r.get(0)?,
                            instrument_id: r.get(1)?,
                            generated_at: r.get(2)?,
                            horizon_days: r.get(3)?,
                            slots: r.get(4)?,
                            received_at: r.get(5)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    // ---- bookings -------------------------------------------------------

    /// Take the soft hold, atomically: the INSERT lands only when no live
    /// row overlaps the slot. One statement, so two strangers racing the
    /// same afternoon cannot both win — the loser learns `false` and the
    /// page offers what remains. Overlap is by time range, not slot
    /// identity: a confirmed 60-minute meeting blocks both half-hours it
    /// covers, whichever duration a later visitor picked.
    pub fn booking_hold(
        &self,
        row: &BookingRow,
        now: &str,
    ) -> Result<bool> {
        self.with(|conn| {
            let inserted = conn.execute(
                "INSERT INTO bookings (id, user_id, instrument_id, slot_start, slot_end,                  duration_minutes, state, hold_expires, queue_seq, ics_sequence, created_at)                  SELECT ?1, ?2, ?3, ?4, ?5, ?6, 'held', ?7, ?8, 0, ?9                  WHERE NOT EXISTS (SELECT 1 FROM bookings                    WHERE user_id = ?2 AND instrument_id = ?3                      AND slot_start < ?5 AND ?4 < slot_end                      AND (state = 'confirmed' OR (state = 'held' AND hold_expires > ?10)))",
                params![
                    row.id,
                    row.user_id,
                    row.instrument_id,
                    row.slot_start,
                    row.slot_end,
                    row.duration_minutes,
                    row.hold_expires,
                    row.queue_seq,
                    row.created_at,
                    now,
                ],
            )?;
            Ok(inserted == 1)
        })
    }

    /// Convert a live hold into the booking, atomically re-proving the slot
    /// is still clear of *other* live rows — a second hold cannot exist by
    /// construction, but re-checking costs one clause and assumes less.
    pub fn booking_confirm(
        &self,
        id: &str,
        manage_hash: &str,
        now: &str,
    ) -> Result<bool> {
        self.with(|conn| {
            let updated = conn.execute(
                "UPDATE bookings SET state = 'confirmed', confirmed_at = ?3,                  manage_hash = ?2, hold_expires = NULL                  WHERE id = ?1 AND state = 'held' AND hold_expires > ?3                    AND NOT EXISTS (SELECT 1 FROM bookings b2                      WHERE b2.user_id = bookings.user_id                        AND b2.instrument_id = bookings.instrument_id                        AND b2.id != bookings.id                        AND b2.slot_start < bookings.slot_end                        AND bookings.slot_start < b2.slot_end                        AND (b2.state = 'confirmed'                             OR (b2.state = 'held' AND b2.hold_expires > ?3)))",
                params![id, manage_hash, now],
            )?;
            Ok(updated == 1)
        })
    }

    /// Every interval a stranger may not book right now: confirmed rows and
    /// unexpired holds, as `(start, end)` pairs for the page to subtract.
    pub fn bookings_blocking(
        &self,
        user_id: &str,
        instrument_id: &str,
        now: &str,
    ) -> Result<Vec<(String, String)>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT slot_start, slot_end FROM bookings                  WHERE user_id = ?1 AND instrument_id = ?2                    AND (state = 'confirmed' OR (state = 'held' AND hold_expires > ?3))",
            )?;
            let rows = stmt.query_map(params![user_id, instrument_id, now], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn booking_get(&self, id: &str) -> Result<Option<BookingRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, user_id, instrument_id, slot_start, slot_end,                      duration_minutes, state, hold_expires, queue_seq, manage_hash,                      ics_sequence, created_at, confirmed_at, cancelled_at                      FROM bookings WHERE id = ?1",
                    params![id],
                    |r| {
                        Ok(BookingRow {
                            id: r.get(0)?,
                            user_id: r.get(1)?,
                            instrument_id: r.get(2)?,
                            slot_start: r.get(3)?,
                            slot_end: r.get(4)?,
                            duration_minutes: r.get(5)?,
                            state: r.get(6)?,
                            hold_expires: r.get(7)?,
                            queue_seq: r.get(8)?,
                            manage_hash: r.get(9)?,
                            ics_sequence: r.get(10)?,
                            created_at: r.get(11)?,
                            confirmed_at: r.get(12)?,
                            cancelled_at: r.get(13)?,
                        })
                    },
                )
                .optional()?)
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
    ///
    /// Returns the removed count and the attachment blob ids the caller must
    /// now delete from disk: attachment rows die in the same transaction as
    /// their queue row, and the files follow — with the orphan sweep as the
    /// backstop when a crash lands between the two.
    pub fn queue_ack(&self, user_id: &str, seqs: &[i64]) -> Result<(usize, Vec<String>)> {
        self.with(|conn| {
            let mut removed = 0;
            let mut blobs = Vec::new();
            let tx = conn.unchecked_transaction()?;
            for seq in seqs {
                let mut stmt = tx.prepare(
                    "SELECT id FROM attachments WHERE user_id = ?1 AND seq = ?2",
                )?;
                let ids = stmt.query_map(params![user_id, seq], |r| r.get::<_, String>(0))?;
                blobs.extend(ids.collect::<rusqlite::Result<Vec<_>>>()?);
                drop(stmt);
                tx.execute(
                    "DELETE FROM attachments WHERE user_id = ?1 AND seq = ?2",
                    params![user_id, seq],
                )?;
                removed += tx.execute(
                    "DELETE FROM queue WHERE user_id = ?1 AND seq = ?2",
                    params![user_id, seq],
                )?;
            }
            tx.commit()?;
            Ok((removed, blobs))
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

    /// Everyone's queue depth in one read — the per-user counts the operator
    /// surfaces render beside each account, grouped so a page over N tenants
    /// is one query rather than N.
    pub fn queue_depths(&self) -> Result<std::collections::HashMap<String, i64>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT user_id, COUNT(*) FROM queue WHERE state = 'queued' \
                 GROUP BY user_id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
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

    // ---- attachments -----------------------------------------------------

    /// The attachments of one queue row, in field order as inserted.
    pub fn attachments_for(&self, user_id: &str, seq: i64) -> Result<Vec<AttachmentRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, user_id, seq, field, filename, content_type, size, sha256, \
                 created_at FROM attachments WHERE user_id = ?1 AND seq = ?2 ORDER BY rowid",
            )?;
            let rows = stmt.query_map(params![user_id, seq], attachment_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// One attachment by its minted id, scoped by user: someone else's id and
    /// a nonexistent one are the same absence.
    pub fn attachment_get(&self, user_id: &str, id: &str) -> Result<Option<AttachmentRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, user_id, seq, field, filename, content_type, size, sha256, \
                     created_at FROM attachments WHERE user_id = ?1 AND id = ?2",
                    params![user_id, id],
                    attachment_row,
                )
                .optional()?)
        })
    }

    /// Whether any attachment row claims this blob id, whoever owns it. The
    /// orphan sweep's question, and only its: everything else is user-scoped.
    pub fn attachment_exists(&self, id: &str) -> Result<bool> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM attachments WHERE id = ?1",
                    params![id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
    }

    /// Complete an upload: attachment rows in, payload replaced, the row
    /// queued, the token spent — one transaction, so a crash leaves either
    /// the row still awaiting its files or fully done, never something drain
    /// would deliver half-made.
    ///
    /// Keyed on the hashed upload token exactly like [`submission_verify`] is
    /// on its token, with the same property: zero matched rows is the whole
    /// of "already done", "expired" and "never existed".
    pub fn upload_complete(
        &self,
        user_id: &str,
        upload_hash: &str,
        now: &str,
        payload: &str,
        attachments: &[AttachmentRow],
    ) -> Result<Option<i64>> {
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            let seq: Option<i64> = tx
                .query_row(
                    "SELECT seq FROM queue WHERE user_id = ?1 AND upload_hash = ?2 \
                     AND state = 'awaiting_upload' AND upload_expires > ?3",
                    params![user_id, upload_hash, now],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(seq) = seq else {
                return Ok(None);
            };
            for a in attachments {
                tx.execute(
                    "INSERT INTO attachments (id, user_id, seq, field, filename, \
                     content_type, size, sha256, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        a.id,
                        user_id,
                        seq,
                        a.field,
                        a.filename,
                        a.content_type,
                        a.size,
                        a.sha256,
                        a.created_at
                    ],
                )?;
            }
            let changed = tx.execute(
                "UPDATE queue SET state = 'queued', payload = ?1, upload_hash = NULL, \
                 upload_expires = NULL WHERE seq = ?2 AND state = 'awaiting_upload'",
                params![payload, seq],
            )?;
            tx.commit()?;
            Ok((changed > 0).then_some(seq))
        })
    }

    /// The row an upload token belongs to, if it is still spendable — a pure
    /// read, so the upload page can be reloaded without consuming anything.
    pub fn upload_pending(
        &self,
        user_id: &str,
        upload_hash: &str,
        now: &str,
    ) -> Result<Option<QueueRow>> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT seq, user_id, type_id, state, payload, created_at FROM queue \
                     WHERE user_id = ?1 AND upload_hash = ?2 AND state = 'awaiting_upload' \
                     AND upload_expires > ?3",
                    params![user_id, upload_hash, now],
                    queue_row,
                )
                .optional()?)
        })
    }

    /// Verified rows whose upload window closed with no files. Deleted like
    /// unverified rows, for the same reason — and they hold no blobs by
    /// construction, since blobs are written only in [`upload_complete`]'s
    /// transaction.
    pub fn expire_unuploaded(&self, now: &str) -> Result<usize> {
        self.with(|conn| {
            Ok(conn.execute(
                "DELETE FROM queue WHERE state = 'awaiting_upload' \
                 AND upload_expires IS NOT NULL AND upload_expires <= ?1",
                params![now],
            )?)
        })
    }

    /// Bytes this address has uploaded today.
    pub fn upload_bytes_today(&self, ip_hash: &str, day: &str) -> Result<i64> {
        self.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT bytes FROM upload_budget WHERE day = ?1 AND ip_hash = ?2",
                    params![day, ip_hash],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0))
        })
    }

    pub fn upload_bytes_add(&self, ip_hash: &str, day: &str, bytes: i64) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO upload_budget (day, ip_hash, bytes) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (day, ip_hash) DO UPDATE SET bytes = bytes + ?3",
                params![day, ip_hash, bytes],
            )?;
            Ok(())
        })
    }

    /// Drop budget rows for days gone by — yesterday's counts bound nothing.
    pub fn expire_upload_budget(&self, today: &str) -> Result<usize> {
        self.with(|conn| {
            Ok(conn.execute(
                "DELETE FROM upload_budget WHERE day < ?1",
                params![today],
            )?)
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
        next: VerifyNext,
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
            let changed = match &next {
                VerifyNext::Queued => tx.execute(
                    "UPDATE queue SET state = 'queued', verify_hash = NULL, \
                     verify_expires = NULL WHERE seq = ?1 AND state = 'submitted'",
                    params![row.seq],
                )?,
                VerifyNext::AwaitingUpload {
                    upload_hash,
                    upload_expires,
                } => tx.execute(
                    "UPDATE queue SET state = 'awaiting_upload', verify_hash = NULL, \
                     verify_expires = NULL, upload_hash = ?2, upload_expires = ?3 \
                     WHERE seq = ?1 AND state = 'submitted'",
                    params![row.seq, upload_hash, upload_expires],
                )?,
            };
            tx.commit()?;
            if changed == 0 {
                return Ok(None);
            }
            row.state = match next {
                VerifyNext::Queued => "queued".into(),
                VerifyNext::AwaitingUpload { .. } => "awaiting_upload".into(),
            };
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
    /// the sweep can say it — and the owner-qualified blob ids to delete,
    /// though a `submitted` row holds none by construction. The shape stays uniform with
    /// [`expire_retained`] anyway: a deletion helper whose blob handling
    /// depends on a state name is the silently-degrading kind.
    pub fn expire_unverified(&self, now: &str) -> Result<(usize, Vec<(String, String)>)> {
        self.delete_queue_where(
            "state = 'submitted' AND verify_expires IS NOT NULL AND verify_expires <= ?1",
            now,
        )
    }

    /// Drop everything past its retention window.
    pub fn expire_retained(&self, now: &str) -> Result<(usize, Vec<(String, String)>)> {
        self.delete_queue_where("retain_until IS NOT NULL AND retain_until <= ?1", now)
    }

    /// Delete queue rows matching a `?1 = now` predicate, their attachment
    /// rows with them, returning the count and the owner-qualified blob ids
    /// for the caller's file deletion — the expiry sweeps cross users, so a
    /// bare id would not name a path.
    fn delete_queue_where(
        &self,
        predicate: &str,
        now: &str,
    ) -> Result<(usize, Vec<(String, String)>)> {
        self.with(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut stmt = tx.prepare(&format!(
                "SELECT a.user_id, a.id FROM attachments a JOIN queue q \
                 ON a.user_id = q.user_id AND a.seq = q.seq WHERE q.{predicate}"
            ))?;
            let ids = stmt.query_map(params![now], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let blobs = ids.collect::<rusqlite::Result<Vec<(String, String)>>>()?;
            drop(stmt);
            tx.execute(
                &format!(
                    "DELETE FROM attachments WHERE (user_id, seq) IN \
                     (SELECT user_id, seq FROM queue WHERE {predicate})"
                ),
                params![now],
            )?;
            let removed = tx.execute(&format!("DELETE FROM queue WHERE {predicate}"), params![now])?;
            tx.commit()?;
            Ok((removed, blobs))
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

/// The two inserts that make a user, inside a transaction the caller owns.
///
/// Shared by `user_create` and `invite_claim` so there is exactly one way an
/// account comes to exist — the "front door is new, the mechanism is not"
/// promise the CLI's help text makes. The caller has already decided the
/// handle is free (`handle_owner`, inside the same transaction, or the check
/// and the claim race).
fn create_user_in(tx: &Connection, id: &str, handle: &str, email: &str, now: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO users (id, handle, email, status, created_at) \
         VALUES (?1, ?2, ?3, 'active', ?4)",
        params![id, handle, email, now],
    )?;
    tx.execute(
        "INSERT INTO handles (handle, user_id, issued_at) VALUES (?1, ?2, ?3)",
        params![handle, id, now],
    )?;
    Ok(())
}

fn invite_row(r: &rusqlite::Row) -> rusqlite::Result<InviteRow> {
    Ok(InviteRow {
        id: r.get(0)?,
        email: r.get(1)?,
        note: r.get(2)?,
        created_at: r.get(3)?,
        expires_at: r.get(4)?,
        claimed_at: r.get(5)?,
        claimed_by: r.get(6)?,
        revoked_at: r.get(7)?,
    })
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
        last_used_at: r.get(7)?,
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

fn attachment_row(r: &rusqlite::Row) -> rusqlite::Result<AttachmentRow> {
    Ok(AttachmentRow {
        id: r.get(0)?,
        user_id: r.get(1)?,
        seq: r.get(2)?,
        field: r.get(3)?,
        filename: r.get(4)?,
        content_type: r.get(5)?,
        size: r.get(6)?,
        sha256: r.get(7)?,
        created_at: r.get(8)?,
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
    // Schemas 1 and 2 predate users and the deployment alike, and the change
    // is not one SQLite can make in place: every table's primary key gained a
    // user. Refused rather than half-migrated, because a ledger that is
    // partly scoped is worse than one that will not open — and nothing was
    // ever deployed from them, so the honest instruction is the cheap one.
    //
    // From 3 on, the box is live and "delete the database" stopped being an
    // instruction anyone may print. Migrations from here are additive: the
    // batch below is `IF NOT EXISTS` throughout, so running it *is* the
    // migration, and a table added to it reaches an existing ledger by the
    // version bump alone. A change that cannot be expressed that way needs a
    // real migration written for it, not a wider bail.
    if version > 0 && version < 3 {
        anyhow::bail!(
            "this ledger is schema {version}, which predates deployment. \
             Nothing was ever deployed from it: delete the database file and \
             start again."
        );
    }
    // The one change the IF-NOT-EXISTS batch cannot express: a column added
    // to a table that already exists. Guarded by the version span that lacks
    // it — a fresh ledger gets the column from CREATE TABLE, and a ledger at
    // 6 or later already has it.
    if (3..6).contains(&version) {
        // Idempotent on purpose: if the ALTER lands and anything after it
        // fails before the version bump, the next start retries the whole
        // migration — and a duplicate-column refusal would wedge the ledger
        // permanently on a one-off transient failure.
        if let Err(e) = conn.execute_batch("ALTER TABLE keys ADD COLUMN last_used_at TEXT;") {
            if !e.to_string().contains("duplicate column") {
                return Err(e.into());
            }
        }
    }
    // Schema 7: the upload step. A verified queue row that is still owed its
    // files carries an upload token the same way a `submitted` row carries
    // its verification one. Same duplicate-column tolerance as above, for the
    // same reason.
    if (3..7).contains(&version) {
        for alter in [
            "ALTER TABLE queue ADD COLUMN upload_hash TEXT;",
            "ALTER TABLE queue ADD COLUMN upload_expires TEXT;",
        ] {
            if let Err(e) = conn.execute_batch(alter) {
                if !e.to_string().contains("duplicate column") {
                    return Err(e.into());
                }
            }
        }
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
            id           TEXT PRIMARY KEY,
            user_id      TEXT NOT NULL DEFAULT '',
            scope        TEXT NOT NULL,
            hash         TEXT NOT NULL,
            label        TEXT NOT NULL DEFAULT '',
            created_at   TEXT NOT NULL,
            revoked_at   TEXT,
            -- Stamped on every authenticated call. What turns the tenant
            -- page's machine list from an inventory into a security feature:
            -- a key that is used is a machine that is alive, and a silent
            -- compromise is visible as life where none was expected.
            last_used_at TEXT
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
            submitted_on TEXT,
            -- The upload step, for types with file fields: a verified row
            -- that is still owed its files is `awaiting_upload` and carries
            -- this token, hashed and expiring like the verification one.
            upload_hash TEXT,
            upload_expires TEXT
        );
        CREATE INDEX IF NOT EXISTS queue_by_state ON queue (user_id, state, seq);
        CREATE INDEX IF NOT EXISTS queue_by_verify ON queue (user_id, verify_hash);
        CREATE INDEX IF NOT EXISTS queue_by_sends ON queue (user_id, submitted_on, recipient_hash);
        CREATE INDEX IF NOT EXISTS queue_by_upload ON queue (user_id, upload_hash);

        -- One row per uploaded file, created in the same transaction that
        -- completes its queue row, so blob lifetime is row lifetime and every
        -- queue deletion extends to blobs by construction. `id` is minted by
        -- us and is the on-disk name — a stranger's filename never touches
        -- the filesystem.
        CREATE TABLE IF NOT EXISTS attachments (
            id           TEXT PRIMARY KEY,
            user_id      TEXT NOT NULL,
            seq          INTEGER NOT NULL,
            field        TEXT NOT NULL,
            filename     TEXT NOT NULL,
            content_type TEXT NOT NULL,
            size         INTEGER NOT NULL,
            sha256       TEXT NOT NULL,
            created_at   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS attachments_by_seq ON attachments (user_id, seq);

        -- Bytes accepted per address per day, in the spirit of the send
        -- budgets: countable without keeping a copy of the address.
        CREATE TABLE IF NOT EXISTS upload_budget (
            day     TEXT NOT NULL,
            ip_hash TEXT NOT NULL,
            bytes   INTEGER NOT NULL,
            PRIMARY KEY (day, ip_hash)
        );

        CREATE TABLE IF NOT EXISTS idempotency (
            key         TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL DEFAULT '',
            bundle_id   TEXT NOT NULL,
            version     INTEGER NOT NULL,
            created_at  TEXT NOT NULL
        );

        -- The right to claim one handle, minted by the operator and spent by
        -- a signup. The token travels in a link and is stored as a hash, like
        -- the verification tokens; the row outlives every state it passes
        -- through, because who was invited and what became of it is the
        -- operator's record.
        CREATE TABLE IF NOT EXISTS invites (
            id          TEXT PRIMARY KEY,
            email       TEXT NOT NULL,
            note        TEXT NOT NULL DEFAULT '',
            token_hash  TEXT NOT NULL UNIQUE,
            created_at  TEXT NOT NULL,
            expires_at  TEXT NOT NULL,
            claimed_at  TEXT,
            claimed_by  TEXT,
            revoked_at  TEXT
        );

        -- The right to connect one machine: minted by a page or the CLI,
        -- spent by `factory-publish connect`, which is when the keys it
        -- names come to exist. Short-lived where an invite is long-lived,
        -- because the person was just shown the command. Redeemed rows are
        -- kept: they are where a pairing's keys are traced to the moment
        -- somebody asks what this machine is.
        CREATE TABLE IF NOT EXISTS pairings (
            id              TEXT PRIMARY KEY,
            user_id         TEXT NOT NULL,
            code_hash       TEXT NOT NULL UNIQUE,
            created_at      TEXT NOT NULL,
            expires_at      TEXT NOT NULL,
            redeemed_at     TEXT,
            publish_key_id  TEXT,
            drain_key_id    TEXT
        );

        -- A sign-in link: the email half of a session, single-use and
        -- minutes-lived, stored as a hash like every other bearer token.
        CREATE TABLE IF NOT EXISTS signin_links (
            id          TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL,
            token_hash  TEXT NOT NULL UNIQUE,
            created_at  TEXT NOT NULL,
            expires_at  TEXT NOT NULL,
            used_at     TEXT
        );

        -- One instrument's availability, replaced wholesale on every push
        -- from home and never computed here: the box subtracts from this
        -- cache (its own holds and bookings), it may never add to it. The
        -- slots column is a JSON array the endpoint shape-validated on the
        -- way in; generated_at is home's stamp, served beside the slots so
        -- a page can state its own staleness.
        CREATE TABLE IF NOT EXISTS slot_cache (
            user_id       TEXT NOT NULL,
            instrument_id TEXT NOT NULL,
            generated_at  TEXT NOT NULL,
            horizon_days  INTEGER NOT NULL,
            slots         TEXT NOT NULL,
            received_at   TEXT NOT NULL,
            PRIMARY KEY (user_id, instrument_id)
        );

        -- One booking, from soft hold to its end state. The hold is the
        -- claim's first phase: it exists from the details POST until the
        -- magic-link click converts it or its expiry frees the slot. What
        -- blocks a slot is a *live* row — confirmed, or held and unexpired —
        -- and liveness is judged against the clock at query time, so an
        -- abandoned hold frees its slot with no sweeper needed. The
        -- stranger's details live on the queue row (queue_seq), not here:
        -- this table is time arithmetic, that one is quarantined prose.
        CREATE TABLE IF NOT EXISTS bookings (
            id               TEXT PRIMARY KEY,
            user_id          TEXT NOT NULL,
            instrument_id    TEXT NOT NULL,
            slot_start       TEXT NOT NULL,
            slot_end         TEXT NOT NULL,
            duration_minutes INTEGER NOT NULL,
            state            TEXT NOT NULL,
            hold_expires     TEXT,
            queue_seq        INTEGER,
            manage_hash      TEXT,
            ics_sequence     INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT NOT NULL,
            confirmed_at     TEXT,
            cancelled_at     TEXT
        );
        CREATE INDEX IF NOT EXISTS bookings_by_slot
            ON bookings (user_id, instrument_id, state, slot_start);

        -- A signed-in browser. The cookie holds the token; this holds its
        -- hash — reading the ledger off the box must not let anyone be
        -- somebody's browser, the same property every credential here has.
        CREATE TABLE IF NOT EXISTS sessions (
            id          TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL,
            token_hash  TEXT NOT NULL UNIQUE,
            created_at  TEXT NOT NULL,
            expires_at  TEXT NOT NULL,
            revoked_at  TEXT
        );

        -- The operator's way into a browser: a one-time link minted through
        -- the API by an operate key, and the session it becomes. Deliberately
        -- parallel to signin_links/sessions and deliberately not those
        -- tables: a tenant session joins on a user, an operator session
        -- joins on a key, and no query that answers one can be handed the
        -- other.
        CREATE TABLE IF NOT EXISTS operator_links (
            id          TEXT PRIMARY KEY,
            key_id      TEXT NOT NULL,
            token_hash  TEXT NOT NULL UNIQUE,
            created_at  TEXT NOT NULL,
            expires_at  TEXT NOT NULL,
            used_at     TEXT
        );

        CREATE TABLE IF NOT EXISTS operator_sessions (
            id          TEXT PRIMARY KEY,
            key_id      TEXT NOT NULL,
            token_hash  TEXT NOT NULL UNIQUE,
            created_at  TEXT NOT NULL,
            expires_at  TEXT NOT NULL,
            revoked_at  TEXT
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

    /// The migration the deployed box actually takes: a schema-3 ledger with
    /// data in it opens under a schema-4 binary, keeps its rows, and gains
    /// the invites table. "Delete the database and start again" stopped
    /// being a printable instruction the day the box went live, so from 3 on
    /// an upgrade has to be additive — this is the test that holds it to
    /// that.
    #[test]
    fn a_deployed_ledger_upgrades_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("factory.db");
        {
            let db = Db::open(&path).unwrap();
            db.user_create("alice", "a@example.org", "t").unwrap();
            // Wind it back to what the box was running before invites, minus
            // the table this migration adds.
            db.with(|conn| {
                // DROP COLUMN would be shorter, but SQLite rewrites the
                // stored DDL to do it and trips over the comments in ours —
                // so the v3 table is rebuilt the long way.
                conn.execute_batch(
                    "DROP TABLE invites; DROP TABLE pairings; DROP TABLE sessions; \
                     DROP TABLE signin_links; \
                     CREATE TABLE keys_v3 (id TEXT PRIMARY KEY, \
                       user_id TEXT NOT NULL DEFAULT '', scope TEXT NOT NULL, \
                       hash TEXT NOT NULL, label TEXT NOT NULL DEFAULT '', \
                       created_at TEXT NOT NULL, revoked_at TEXT); \
                     INSERT INTO keys_v3 SELECT id, user_id, scope, hash, label, \
                       created_at, revoked_at FROM keys; \
                     DROP TABLE keys; ALTER TABLE keys_v3 RENAME TO keys; \
                     PRAGMA user_version = 3;",
                )?;
                Ok(())
            })
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.user_by_handle("alice").unwrap().unwrap().email,
            "a@example.org"
        );
        db.invite_create("b@example.org", "", "hash", "t", "2027-01-01T00:00:00Z")
            .unwrap();
        let version: i64 = db
            .with(|conn| Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(version, SCHEMA);

        // The half-run case: the ALTER landed but the version bump did not.
        // The retry must go through rather than wedging on a duplicate
        // column — a one-off transient failure must not brick the ledger.
        db.with(|conn| {
            conn.pragma_update(None, "user_version", 5)?;
            Ok(())
        })
        .unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert!(db.user_by_handle("alice").unwrap().is_some());
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

        assert_eq!(db.queue_ack(&u, &[a]).unwrap().0, 1);
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

    /// Blob lifetime is row lifetime: attachment rows ride the completion
    /// transaction in, and every queue deletion carries them out, naming the
    /// blob ids so the caller can take the files with them.
    #[test]
    fn attachment_rows_live_and_die_with_their_queue_row() {
        let (db, u) = db_with_user();
        let submission = Submission {
            user_id: u.clone(),
            type_id: "letterish".into(),
            payload: "{}".into(),
            created_at: "2026-08-07T00:00:00Z".into(),
            retain_until: Some("2026-09-01T00:00:00Z".into()),
            verify_hash: "vh".into(),
            verify_expires: "2999-01-01T00:00:00Z".into(),
            recipient_hash: "rh".into(),
        };
        let seq = db.submission_add(&submission).unwrap();

        // The upload step: verified into `awaiting_upload`, which drain does
        // not see, then completed into `queued`, which it does.
        db.submission_verify(
            &u,
            "vh",
            "2026-08-07T00:01:00Z",
            VerifyNext::AwaitingUpload {
                upload_hash: "uh".into(),
                upload_expires: "2999-01-01T00:00:00Z".into(),
            },
        )
        .unwrap()
        .expect("verifies");
        assert!(db.drain(&u, 0, 10).unwrap().is_empty(), "not yet uploaded");

        let row = AttachmentRow {
            id: "blob1".into(),
            user_id: u.clone(),
            seq,
            field: "cv".into(),
            filename: "cv.pdf".into(),
            content_type: "application/pdf".into(),
            size: 5,
            sha256: format!("sha256:{}", "cd".repeat(32)),
            created_at: now(),
        };
        let completed = db
            .upload_complete(&u, "uh", "2026-08-07T00:02:00Z", r#"{"done":1}"#, &[row])
            .unwrap();
        assert_eq!(completed, Some(seq));
        assert_eq!(
            db.upload_complete(&u, "uh", "2026-08-07T00:02:00Z", "{}", &[])
                .unwrap(),
            None,
            "the token spends once"
        );
        assert_eq!(db.drain(&u, 0, 10).unwrap().len(), 1);
        assert_eq!(db.attachments_for(&u, seq).unwrap().len(), 1);
        assert!(db.attachment_get(&u, "blob1").unwrap().is_some());
        assert!(
            db.attachment_get("someone-else", "blob1").unwrap().is_none(),
            "someone else's id and a nonexistent one are the same absence"
        );

        // Retention expiry takes the row, the attachment row, and names the
        // blob with its owner.
        let (removed, blobs) = db.expire_retained("2030-01-01T00:00:00Z").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(blobs, vec![(u.clone(), "blob1".to_string())]);
        assert!(db.attachment_get(&u, "blob1").unwrap().is_none());
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
            last_used_at: None,
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
