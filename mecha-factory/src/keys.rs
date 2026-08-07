//! Two keys, two scopes, and a box that stores neither of them.
//!
//! A token reads `mk_pub_<id>.<secret>`. The parts do different jobs and it is
//! worth being precise about which, because getting either wrong is the whole
//! authentication story:
//!
//! - **`<id>` is a lookup handle, not a credential.** Argon2id is deliberately
//!   expensive, so verifying a presented token against every stored key would
//!   make authentication cost O(keys) — and would tempt whoever noticed into
//!   replacing it with something cheap. The id selects one row; the secret is
//!   then verified against that row's hash, once.
//! - **`mk_pub_` is a label for humans, and the server never reads it.** The
//!   scope comes from the database row. A prefix is text the presenter controls,
//!   and a drain key spelled `mk_pub_` must not publish.
//! - **The secret is stored as an Argon2id hash and never as bytes.** The box is
//!   assumed lost; what an attacker finds there has to be a verifier, not a
//!   token they can replay against a box that has *not* been lost.
//!
//! Minting happens here, on the server, because that is where the hash has to
//! land — and the token is printed exactly once. There is deliberately no way
//! to read a key back: a "show me that key again" verb is a plaintext key at
//! rest with extra steps.

use anyhow::{bail, Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use crate::db::{Db, KeyRow, Scope};

/// Why a request was refused. The distinction never reaches the wire — every
/// arm answers 401 or 403 with the same body — but it is what gets logged, and
/// "revoked key used" and "wrong scope" are different incidents.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// No `Authorization: Bearer …` at all.
    Missing,
    /// Present, but not a token this server could have minted.
    Malformed,
    /// Well-formed, and matched nothing.
    Unknown,
    Revoked,
    /// A real key, used on an endpoint its scope does not cover.
    WrongScope {
        has: Scope,
        needs: Scope,
    },
}

impl AuthError {
    /// 401 for "who are you", 403 for "you, but not here". The difference
    /// matters to a client deciding whether to retry with another key.
    pub fn status(&self) -> u16 {
        match self {
            AuthError::WrongScope { .. } => 403,
            _ => 401,
        }
    }

    pub fn public_message(&self) -> &'static str {
        match self {
            AuthError::WrongScope { .. } => "this key does not cover this endpoint",
            _ => "a valid bearer token is required",
        }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Missing => write!(f, "no bearer token"),
            AuthError::Malformed => write!(f, "malformed token"),
            AuthError::Unknown => write!(f, "unknown key"),
            AuthError::Revoked => write!(f, "revoked key"),
            AuthError::WrongScope { has, needs } => {
                write!(
                    f,
                    "key is `{}`, endpoint needs `{}`",
                    has.as_str(),
                    needs.as_str()
                )
            }
        }
    }
}

/// A freshly minted key: the token, which is shown once and then unrecoverable,
/// and the row that outlives it.
pub struct Minted {
    pub token: String,
    pub row: KeyRow,
}

/// An opaque identifier: a user id, and the lookup half of a key.
///
/// Opaque on purpose. A user id that encoded a handle or an email would be a
/// foreign key on a mutable fact, and this is the value every row points at.
pub fn random_id() -> String {
    // A failure here is a system with no entropy source, which is not a
    // condition this program can sensibly continue under.
    hex(&random::<16>().expect("the system random source is readable"))
}

/// Mint a key for a user, store its hash, and hand back the token.
pub fn mint(db: &Db, user_id: &str, scope: Scope, label: &str) -> Result<Minted> {
    let id = hex(&random::<8>()?);
    let secret = hex(&random::<32>()?);
    let salt = SaltString::encode_b64(&random::<16>()?)
        .map_err(|e| anyhow::anyhow!("encoding a salt: {e}"))?;
    let hash = Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing a key: {e}"))?
        .to_string();

    let row = KeyRow {
        id: id.clone(),
        user_id: user_id.to_string(),
        scope,
        hash,
        label: label.to_string(),
        created_at: crate::db::now(),
        revoked_at: None,
    };
    db.key_insert(&row)?;
    Ok(Minted {
        token: format!("{}{id}.{secret}", scope.prefix()),
        row,
    })
}

/// Split a presented token into its lookup id and its secret.
///
/// The prefix is not checked against anything: it is a human label, and
/// treating it as a claim would be treating attacker-supplied text as
/// authorisation.
fn split(token: &str) -> Option<(&str, &str)> {
    // Derived from `Scope::ALL` rather than listed here. This named its two
    // prefixes inline, so the day a third scope arrived it minted tokens
    // nothing could parse — and the symptom was a 401 saying no token was
    // presented, which reads as a client bug rather than a server one.
    let body = Scope::ALL
        .iter()
        .find_map(|scope| token.strip_prefix(scope.prefix()))?;
    let (id, secret) = body.split_once('.')?;
    let hexish = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
    (hexish(id) && hexish(secret)).then_some((id, secret))
}

/// Authenticate a bearer token for an endpoint that needs `needs`.
///
/// Returns the row so the caller can log *which* key acted, which is the only
/// thing that makes rotation and revocation reviewable after the fact.
pub fn authenticate(
    db: &Db,
    header: Option<&str>,
    needs: Scope,
) -> std::result::Result<KeyRow, AuthError> {
    let header = header.ok_or(AuthError::Missing)?;
    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .ok_or(AuthError::Malformed)?
        .trim();
    let (id, secret) = split(token).ok_or(AuthError::Malformed)?;

    let row = db
        .key_by_id(id)
        .map_err(|_| AuthError::Unknown)?
        .ok_or(AuthError::Unknown)?;
    let parsed = PasswordHash::new(&row.hash).map_err(|_| AuthError::Unknown)?;
    Argon2::default()
        .verify_password(secret.as_bytes(), &parsed)
        .map_err(|_| AuthError::Unknown)?;

    // Checked *after* the secret, so a revoked key's id cannot be used to probe
    // which ids ever existed.
    if row.revoked_at.is_some() {
        return Err(AuthError::Revoked);
    }
    if row.scope != needs {
        return Err(AuthError::WrongScope {
            has: row.scope,
            needs,
        });
    }
    Ok(row)
}

/// Authenticate a token for an endpoint that any live key may reach.
///
/// Exactly one thing uses this — `GET /v1/health`, which answers publicly with
/// a bare "up" and adds counts for a caller that holds a key. Written as its
/// own function rather than as an `Option<Scope>` parameter so that no
/// scope-checked endpoint can reach it by passing `None`.
pub fn authenticate_any(db: &Db, header: Option<&str>) -> std::result::Result<KeyRow, AuthError> {
    let row = authenticate(db, header, Scope::Publish);
    match row {
        Err(AuthError::WrongScope { has, .. }) => authenticate(db, header, has),
        other => other,
    }
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("reading from the system random source: {e}"))?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read a token out of a file, with the checks that make a key file a key file.
///
/// Used by the publisher-side tooling and by anything on the box that needs to
/// present one. A key with loose permissions is a warning rather than a
/// refusal — the file may be on a machine where the mode means nothing, and
/// refusing to publish over a permission bit is how a person turns the check
/// off entirely.
pub fn read_key_file(path: &std::path::Path) -> Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let token = text.trim().to_string();
    if split(&token).is_none() {
        bail!(
            "{} does not contain a factory key (expected `mk_pub_…` or `mk_drn_…`)",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o077;
        if mode != 0 {
            tracing::warn!(
                path = %path.display(),
                "key file is readable by others; `chmod 600` it"
            );
        }
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    /// Every test needs somebody to own the key.
    fn user(db: &Db) -> String {
        db.user_create("alice", "alice@example.org", "2026-08-06T00:00:00Z")
            .unwrap()
            .id
    }

    /// Every scope's token must survive `split`, which is what the prefix list
    /// inside it used to decide by hand.
    ///
    /// Adding `Release` minted `mk_rel_…` tokens that `split` returned `None`
    /// for, so a perfectly good key authenticated as nothing and the endpoint
    /// answered "a valid bearer token is required" — a 401 that blames the
    /// caller for a server-side omission. Iterating `Scope::ALL` here means a
    /// fourth scope cannot repeat it silently.
    #[test]
    fn a_token_of_every_scope_parses_and_authenticates() {
        let db = db();
        let user = user(&db);
        for scope in Scope::ALL {
            let minted = mint(&db, &user, scope, "test").unwrap();
            assert!(
                minted.token.starts_with(scope.prefix()),
                "{scope:?} minted {}",
                minted.token
            );
            assert!(
                split(&minted.token).is_some(),
                "{scope:?} minted a token `split` cannot parse: {}",
                minted.token
            );
            let row = authenticate(&db, Some(&format!("Bearer {}", minted.token)), scope)
                .unwrap_or_else(|e| {
                    panic!("{scope:?} did not authenticate for its own scope: {e}")
                });
            assert_eq!(row.scope, scope);
        }
    }

    #[test]
    fn a_minted_key_authenticates_once_and_only_for_its_own_scope() {
        let db = db();
        let minted = mint(&db, &user(&db), Scope::Publish, "laptop").unwrap();
        assert!(minted.token.starts_with("mk_pub_"));

        let header = format!("Bearer {}", minted.token);
        let row = authenticate(&db, Some(&header), Scope::Publish).unwrap();
        assert_eq!(row.id, minted.row.id);

        assert_eq!(
            authenticate(&db, Some(&header), Scope::Drain).unwrap_err(),
            AuthError::WrongScope {
                has: Scope::Publish,
                needs: Scope::Drain
            }
        );
    }

    /// The prefix is a label. A drain key that says `mk_pub_` is still a drain
    /// key, because the scope is read from the row.
    #[test]
    fn the_prefix_is_never_the_authorisation() {
        let db = db();
        let minted = mint(&db, &user(&db), Scope::Drain, "trigger").unwrap();
        let relabelled = minted.token.replace("mk_drn_", "mk_pub_");
        let header = format!("Bearer {relabelled}");
        assert_eq!(
            authenticate(&db, Some(&header), Scope::Publish).unwrap_err(),
            AuthError::WrongScope {
                has: Scope::Drain,
                needs: Scope::Publish
            }
        );
        // …and it still works for what it actually is.
        authenticate(&db, Some(&header), Scope::Drain).unwrap();
    }

    #[test]
    fn a_revoked_key_stops_working_and_the_row_stays() {
        let db = db();
        let minted = mint(&db, &user(&db), Scope::Publish, "old").unwrap();
        let header = format!("Bearer {}", minted.token);
        authenticate(&db, Some(&header), Scope::Publish).unwrap();

        db.key_revoke(&minted.row.id, "2026-08-06T12:00:00Z")
            .unwrap();
        assert_eq!(
            authenticate(&db, Some(&header), Scope::Publish).unwrap_err(),
            AuthError::Revoked
        );
        assert!(db.key_by_id(&minted.row.id).unwrap().is_some());
    }

    #[test]
    fn nothing_else_gets_in() {
        let db = db();
        let minted = mint(&db, &user(&db), Scope::Publish, "k").unwrap();
        let (id, secret) = split(&minted.token).unwrap();

        for header in [
            None,
            Some("".to_string()),
            Some("Bearer".to_string()),
            Some(format!("Bearer {}", minted.token.replace('.', ""))),
            Some("Bearer mk_pub_zz.zz".to_string()),
            // The right id with the wrong secret is the case the hash exists
            // for.
            Some(format!("Bearer mk_pub_{id}.{}", "0".repeat(secret.len()))),
            // A secret with no key.
            Some(format!("Bearer mk_pub_00000000.{secret}")),
            // Not a bearer scheme at all.
            Some(format!("Basic {}", minted.token)),
        ] {
            assert!(
                authenticate(&db, header.as_deref(), Scope::Publish).is_err(),
                "{header:?} authenticated"
            );
        }
    }

    #[test]
    fn health_takes_either_key_and_still_takes_nothing_else() {
        let db = db();
        let user = user(&db);
        for scope in [Scope::Publish, Scope::Drain] {
            let minted = mint(&db, &user, scope, "k").unwrap();
            let header = format!("Bearer {}", minted.token);
            assert_eq!(authenticate_any(&db, Some(&header)).unwrap().scope, scope);
        }
        assert!(authenticate_any(&db, None).is_err());
        assert!(authenticate_any(&db, Some("Bearer mk_pub_aa.bb")).is_err());
    }

    /// Two mints are two different keys — the obvious property, and the one a
    /// mistake in the random source would silently take away.
    #[test]
    fn two_keys_are_two_keys() {
        let db = db();
        let user = user(&db);
        let a = mint(&db, &user, Scope::Publish, "a").unwrap();
        let b = mint(&db, &user, Scope::Publish, "b").unwrap();
        assert_ne!(a.token, b.token);
        assert_ne!(a.row.id, b.row.id);
        assert_ne!(a.row.hash, b.row.hash);
        // And one key's secret does not open the other's row.
        let crossed = format!("Bearer mk_pub_{}.{}", a.row.id, split(&b.token).unwrap().1);
        assert!(authenticate(&db, Some(&crossed), Scope::Publish).is_err());
    }
}
