//! One certificate per user, ordered while the server runs.
//!
//! The old shape ordered **one** certificate covering every name — the three
//! base origins plus one hostname per active user — and said so on every start:
//!
//! ```text
//! ordering certificates; a user created after this needs a restart to get one
//! ```
//!
//! That was not a bug. `AcmeConfig::state()` consumes its domain list and there
//! is no setter afterwards, so the list is fixed at startup by construction. A
//! second person could not arrive without an operator, which is the whole of
//! what `docs/SELF-SERVE.md` exists to remove.
//!
//! **The certificate set is derived from the ledger, not from a command.**
//! `factory user create` runs in another process — over SSH, today — so an
//! in-process notification would only ever fire for a signup endpoint that does
//! not exist yet, and the SSH path would keep needing its restart. A
//! reconciliation loop covers both with no code of its own, and it is the same
//! decision the front door already made: a state that is only correct after
//! somebody runs a command is a state nobody can trust.
//!
//! Three pieces, and each is here for a reason the other two cannot serve:
//!
//! - **[`Registry`] dispatches by SNI.** `ResolvesServerCertAcme::resolve`
//!   returns *its* certificate for any name it is asked about — it never looks
//!   at SNI for real traffic, because a single state only ever holds one
//!   certificate. With one state per user that is exactly wrong, so the
//!   dispatch is ours. A name nobody has claimed resolves to nothing and the
//!   handshake fails, which keeps the property §14.3 asked us not to throw
//!   away: an unclaimed handle dies at TLS, and the 404 the router would have
//!   returned is a *second* line of defence rather than the only one.
//! - **HTTP-01, not TLS-ALPN-01.** A TLS-ALPN-01 challenge arrives as a
//!   connection that must be answered with a throwaway certificate and then
//!   **not** handed to the application. `AcmeAcceptor` is the only thing that
//!   does that, it is `pub(crate)`, and it takes the library's concrete
//!   resolver rather than a `dyn ResolvesServerCert` — so the one type that
//!   would have to know about every certificate cannot be handed the wrapper
//!   that does. HTTP-01 has no such type: the challenge is an ordinary GET on
//!   port 80 and both halves are public API.
//! - **One group per user, not one certificate for everyone.** A shared
//!   certificate would reintroduce the fixed list — adding a name means
//!   reordering for everybody — and Let's Encrypt caps a SAN list at 100.
//!   Per-user orders are affordable because **renewals are exempt from the rate
//!   limit**: only new certificates count against the 50 per registered domain
//!   per week, so the ceiling is on signups and not on the fleet.
//!
//! The cost is one property, and it is stated here rather than discovered:
//! **port 80 is now load-bearing for issuance.** Whoever can answer on it can
//! obtain certificates for these names. The mitigation is that anybody who can
//! answer on port 80 of this host has already won and 443 was never less
//! exposed — but `[listen] http` stops being optional, and `Config::check`
//! refuses the combination rather than letting a box come up serving TLS and
//! quietly unable to renew.

use anyhow::Result;
use futures_util::StreamExt;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, ResolvesServerCertAcme, UseChallenge};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::config::{Config, Role};
use crate::db::{Db, UserRow};

/// What ordering a certificate needs, and nothing else.
#[derive(Debug, Clone)]
pub struct Acme {
    /// Where Let's Encrypt sends expiry warnings. A `mailto:` URI.
    pub contact: String,
    /// The staging directory, whose certificates nobody trusts and whose limits
    /// are large.
    pub staging: bool,
    /// The account key and the certificates, so a restart re-issues nothing.
    pub cache: PathBuf,
}

impl Acme {
    /// The `[tls]` block as this module needs it, or nothing when the box is
    /// serving plain HTTP on loopback.
    pub fn from_config(config: &Config) -> Option<Acme> {
        config.tls.as_ref().map(|tls| Acme {
            contact: tls.contact.clone(),
            staging: tls.staging,
            cache: config.acme_cache(),
        })
    }
}

/// Every certificate this process is serving or ordering, dispatched by SNI.
#[derive(Debug)]
pub struct Registry {
    acme: Acme,
    inner: RwLock<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// SNI → whatever resolves that name. `dyn` rather than the concrete ACME
    /// resolver so the dispatch can be tested without a handshake and without
    /// the network.
    by_name: HashMap<String, Arc<dyn ResolvesServerCert>>,
    /// Every ACME resolver once, for the challenge route. Concrete, because
    /// `get_http_01_key_auth` lives on the type and not on the trait.
    challengers: Vec<Arc<ResolvesServerCertAcme>>,
    /// Name sets already ordered. Reconciling twice must order once — an
    /// `AcmeState` that failed is backing off inside itself, and replacing it
    /// would restart that backoff and spend Let's Encrypt's failed-validation
    /// budget instead of waiting.
    ordered: HashSet<String>,
}

impl Registry {
    pub fn new(acme: Acme) -> Arc<Registry> {
        Arc::new(Registry {
            acme,
            inner: RwLock::new(Inner::default()),
        })
    }

    /// The `rustls` configuration that serves every name at once.
    ///
    /// `ring` rather than the default `aws-lc-rs`, so the box's binary needs no
    /// cmake and no C toolchain — the same choice `Cargo.toml` makes for
    /// `rustls-acme`, and it has to be the same one or the two would disagree
    /// about which provider signed what.
    pub fn server_config(self: &Arc<Self>) -> Arc<ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_no_client_auth()
            .with_cert_resolver(self.clone());
        Arc::new(config)
    }

    /// Order a certificate for `names` unless one is already on the way.
    ///
    /// Returns whether this call started an order. Spawns the state machine
    /// that does the work: `AcmeState` is a stream, and nothing is ordered or
    /// renewed unless somebody polls it.
    pub fn ensure(self: &Arc<Self>, names: Vec<String>) -> bool {
        let mut names = names;
        names.sort();
        names.dedup();
        if names.is_empty() {
            return false;
        }
        let group = names.join(",");

        // Claim the group before building anything, so two reconciles racing
        // produce one order. The window where the group is claimed and the
        // names are not yet resolvable costs a failed handshake on a name that
        // had no certificate a moment ago anyway.
        {
            let mut inner = self.inner.write().expect("certificate registry lock");
            if !inner.ordered.insert(group.clone()) {
                return false;
            }
        }

        let mut state = AcmeConfig::new(names.clone())
            .contact([self.acme.contact.clone()])
            .cache(DirCache::new(self.acme.cache.clone()))
            .directory_lets_encrypt(!self.acme.staging)
            .challenge_type(UseChallenge::Http01)
            .state();
        let resolver = state.resolver();

        {
            let mut inner = self.inner.write().expect("certificate registry lock");
            for name in &names {
                inner
                    .by_name
                    .insert(name.to_ascii_lowercase(), resolver.clone());
            }
            inner.challengers.push(resolver);
        }

        // Events are logged rather than swallowed: a certificate that silently
        // failed to renew is a site that goes down in sixty days for a reason
        // nobody wrote down. The group is named on every line, because with one
        // state per user "acme error" on its own does not say whose.
        tokio::spawn(async move {
            loop {
                match state.next().await {
                    Some(Ok(event)) => tracing::info!(%group, ?event, "acme"),
                    Some(Err(e)) => tracing::error!(%group, error = %e, "acme"),
                    None => {
                        tracing::error!(%group, "the acme state machine ended; this certificate will not renew");
                        break;
                    }
                }
            }
        });
        true
    }

    /// Which resolver serves this name, if any.
    ///
    /// Split out from [`ResolvesServerCert::resolve`] because a `ClientHello`
    /// can only be produced by a real handshake, and the dispatch is the part
    /// worth testing.
    fn lookup(&self, sni: Option<&str>) -> Option<Arc<dyn ResolvesServerCert>> {
        let sni = sni?.to_ascii_lowercase();
        let inner = self.inner.read().expect("certificate registry lock");
        inner.by_name.get(&sni).cloned()
    }

    /// The answer to an HTTP-01 challenge, from whichever order is waiting on
    /// it.
    ///
    /// Every resolver is asked because each holds the challenge data for its
    /// own order and there is no index from token to group. Tokens are
    /// high-entropy and per-authorization, so a wrong answer is not reachable —
    /// a resolver that does not know the token says nothing.
    pub fn http_01_key_auth(&self, token: &str) -> Option<String> {
        let challengers = {
            let inner = self.inner.read().expect("certificate registry lock");
            inner.challengers.clone()
        };
        challengers
            .iter()
            .find_map(|resolver| resolver.get_http_01_key_auth(token))
    }

    /// How many certificate groups are on the books. What `check` and the
    /// startup line report.
    pub fn groups(&self) -> usize {
        self.inner
            .read()
            .expect("certificate registry lock")
            .ordered
            .len()
    }

    /// Install a resolver for names directly. The seam [`Registry::ensure`]
    /// uses, and what lets the SNI dispatch be tested with a stub.
    #[cfg(test)]
    fn install(&self, names: &[&str], resolver: Arc<dyn ResolvesServerCert>) {
        let mut inner = self.inner.write().expect("certificate registry lock");
        for name in names {
            inner
                .by_name
                .insert(name.to_ascii_lowercase(), resolver.clone());
        }
    }
}

impl ResolvesServerCert for Registry {
    fn resolve(&self, hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        // Copied out first: `server_name` borrows the hello, and the inner
        // resolver takes it by value.
        let sni = hello.server_name().map(|name| name.to_string());
        self.lookup(sni.as_deref())?.resolve(hello)
    }
}

/// Every certificate this deployment should hold, one group per certificate.
///
/// The three base origins travel together — they are ordered once and never
/// change — and each active user gets their own group of two, because artifacts
/// and compute are separate origins and a user has a hostname on each.
///
/// A suspended account is left out, exactly as it always was: it serves
/// nothing, and ordering for it would spend issuance budget on a name with no
/// pages. Note what this does **not** do — a suspension does not *remove* a
/// resolver already installed. Dropping it would fail the handshake instead of
/// letting the router answer, and the router is where suspension is already
/// decided and already tested. Restoring an account therefore costs nothing.
pub fn groups(config: &Config, users: &[UserRow]) -> Vec<Vec<String>> {
    let mut out = vec![config.origins.names()];
    for user in users.iter().filter(|user| user.active()) {
        out.push(vec![
            config.origins.host_for(Role::Artifacts, &user.handle),
            config.origins.host_for(Role::Compute, &user.handle),
        ]);
    }
    out
}

/// Order whatever the ledger says we should have and do not. Returns how many
/// orders this pass started.
pub fn reconcile(registry: &Arc<Registry>, db: &Db, config: &Config) -> Result<usize> {
    let users = db.users()?;
    Ok(groups(config, &users)
        .into_iter()
        .filter(|names| registry.ensure(names.clone()))
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Limits, Listen, Origins, Tls};

    fn config() -> Config {
        Config {
            theme: "nocturne".into(),
            mail: None,
            data_dir: PathBuf::from("/tmp/factory-test"),
            origins: Origins {
                gate: "gate.example.org".into(),
                artifacts: "art.example.org".into(),
                compute: "compute.example.org".into(),
            },
            listen: Listen {
                https: ([0, 0, 0, 0], 443).into(),
                http: Some(([0, 0, 0, 0], 80).into()),
            },
            tls: Some(Tls {
                contact: "mailto:someone@example.org".into(),
                staging: true,
            }),
            limits: Limits::default(),
        }
    }

    /// A resolver that answers nothing, so the dispatch can be tested without a
    /// certificate. What we assert is *which* resolver a name reaches, and
    /// identity is enough for that.
    struct Stub(&'static str);
    // Written out rather than derived: the label *is* what the assertions read,
    // and a derived `Debug` does not count as a use for the dead-code lint.
    impl std::fmt::Debug for Stub {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Stub({:?})", self.0)
        }
    }
    impl ResolvesServerCert for Stub {
        fn resolve(&self, _hello: ClientHello) -> Option<Arc<CertifiedKey>> {
            None
        }
    }

    fn registry() -> Arc<Registry> {
        Registry::new(Acme {
            contact: "mailto:someone@example.org".into(),
            staging: true,
            cache: PathBuf::from("/tmp/factory-test/acme"),
        })
    }

    /// The whole reason this module exists. `ResolvesServerCertAcme` hands back
    /// its own certificate whatever it is asked about; with one state per user
    /// that would serve alice's certificate for bob's name, so the dispatch has
    /// to be ours and it has to be by SNI.
    #[test]
    fn a_name_reaches_its_own_resolver_and_nobody_elses() {
        let registry = registry();
        registry.install(
            &["alice.art.example.org", "alice.compute.example.org"],
            Arc::new(Stub("alice")),
        );
        registry.install(&["bob.art.example.org"], Arc::new(Stub("bob")));

        let whose = |name: &str| format!("{:?}", registry.lookup(Some(name)).unwrap());
        assert_eq!(whose("alice.art.example.org"), r#"Stub("alice")"#);
        assert_eq!(whose("alice.compute.example.org"), r#"Stub("alice")"#);
        assert_eq!(whose("bob.art.example.org"), r#"Stub("bob")"#);
        // SNI arrives lowercased in practice; a resolver that only matched the
        // exact bytes would fail a handshake nobody could reproduce by hand.
        assert_eq!(whose("ALICE.art.example.org"), r#"Stub("alice")"#);
    }

    /// An unclaimed handle resolves to nothing, so the connection dies at the
    /// handshake and never reaches the router. That is the second line of
    /// defence §14.3 asked us to keep, and it is a property of *this* function
    /// — path routing would have given it away.
    #[test]
    fn a_name_nobody_has_claimed_resolves_to_nothing() {
        let registry = registry();
        registry.install(&["alice.art.example.org"], Arc::new(Stub("alice")));
        assert!(registry.lookup(Some("mallory.art.example.org")).is_none());
        assert!(registry.lookup(Some("art.example.org")).is_none());
        // A client that sent no SNI is told apart from one that sent an unknown
        // name only in that there is nothing to look up. Both get nothing.
        assert!(registry.lookup(None).is_none());
    }

    /// The base names are one certificate; each user is their own. A shared
    /// certificate would put us back where we started — adding a name means
    /// reordering for everybody — and Let's Encrypt caps a SAN list at 100.
    #[test]
    fn the_bases_are_one_group_and_every_user_is_their_own() {
        let config = config();
        let db = crate::db::Db::open_in_memory().unwrap();
        db.user_create("alice", "a@example.org", "t").unwrap();
        let suspended = db.user_create("bob", "b@example.org", "t").unwrap();
        db.user_status(&suspended.id, "suspended").unwrap();

        let groups = groups(&config, &db.users().unwrap());
        assert_eq!(
            groups,
            vec![
                vec![
                    "art.example.org".to_string(),
                    "compute.example.org".to_string(),
                    "gate.example.org".to_string()
                ],
                vec![
                    "alice.art.example.org".to_string(),
                    "alice.compute.example.org".to_string()
                ],
            ]
        );
        // A suspended account serves nothing, so it needs no certificate — and
        // ordering one would spend issuance budget on a name with no pages.
        assert!(
            !groups.iter().flatten().any(|n| n.starts_with("bob.")),
            "{groups:?}"
        );
    }

    /// Reconciling twice must order once. An `AcmeState` whose order failed is
    /// backing off inside itself; replacing it would restart that backoff and
    /// spend the failed-validation budget instead of waiting it out.
    ///
    /// Driven through `ordered` rather than `ensure`, because `ensure` spawns
    /// onto a runtime and this is a claim about the bookkeeping.
    #[test]
    fn a_group_is_ordered_once_however_often_it_is_reconciled() {
        let registry = registry();
        let claim = |names: &[&str]| {
            let mut names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
            names.sort();
            let mut inner = registry.inner.write().unwrap();
            inner.ordered.insert(names.join(","))
        };
        assert!(claim(&[
            "alice.art.example.org",
            "alice.compute.example.org"
        ]));
        assert!(!claim(&[
            "alice.art.example.org",
            "alice.compute.example.org"
        ]));
        // Order of the names is not part of the identity of a group, or a
        // reordered ledger would re-order a certificate.
        assert!(!claim(&[
            "alice.compute.example.org",
            "alice.art.example.org"
        ]));
        assert_eq!(registry.groups(), 1);
    }
}
