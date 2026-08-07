//! What the box is told, and the one thing it is never told.
//!
//! The configuration names three hostnames, a data directory, and the ACME
//! contact. It does **not** name a key, a model, or anything that reaches home
//! — the credential-isolation claim is meant to be checkable by reading the
//! deployed config, and a field that could hold a secret is a field somebody
//! eventually puts one in.
//!
//! `[mail]` is the one place a provider is named, and it names a **path** where
//! the others name values, for exactly the reason above: a region and a sender
//! describe a deployment, while the key that can send mail as the domain lives
//! beside the box's other secrets and never in here.
//!
//! Two decisions here that are load-bearing:
//!
//! - **The three origins are separate registrable names, and the server
//!   resolves a request's origin by `Host`.** There is deliberately no default:
//!   a request arriving under a name we do not serve gets nothing, rather than
//!   being answered by whichever origin happens to be first. An artifact served
//!   under the compute policy is a static report that suddenly permits
//!   WebAssembly, which is the silently-degrading-sandbox shape one origin wide.
//! - **Dev mode is the same code path.** It maps three loopback *ports* to the
//!   same three roles, so a local run exercises real `Host` resolution, real
//!   headers and real routing — everything but TLS. A dev mode that took a
//!   different path through the router would verify a program nobody deploys.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Which of the three origins a request arrived on.
///
/// The class of a bundle decides which origin may serve it, and that mapping is
/// [`Role::for_class`] — one function, so "a compute bundle is never served
/// under the artifact policy" is a thing to test rather than a thing to
/// remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The API, the forms, and later the capability check. Nothing static.
    Gate,
    /// Published bundles that do not execute WebAssembly.
    Artifacts,
    /// Notebooks. `wasm-unsafe-eval` lives here and nowhere else.
    Compute,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Gate => "gate",
            Role::Artifacts => "artifacts",
            Role::Compute => "compute",
        }
    }

    /// The only origin allowed to serve a bundle of this class.
    pub fn for_class(class: mecha_manifest::ContentClass) -> Role {
        match class {
            mecha_manifest::ContentClass::Compute => Role::Compute,
            _ => Role::Artifacts,
        }
    }
}

/// Names nobody may take, because something else already answers to them or
/// will need to.
///
/// A user who claimed `www`, `api` or `abuse` would be serving their own
/// content at a name people reasonably assume is ours — and `_acme-challenge`
/// is how certificates are issued. Reserved before the first signup, because
/// afterwards the fix is taking a name off somebody.
const RESERVED_HANDLES: &[&str] = &[
    "www",
    "api",
    "gate",
    "admin",
    "abuse",
    "security",
    "support",
    "help",
    "mail",
    "smtp",
    "mx",
    "ns",
    "ns1",
    "ns2",
    "dns",
    "static",
    "assets",
    "cdn",
    "app",
    "auth",
    "login",
    "status",
    "_acme-challenge",
    "acme-challenge",
    "localhost",
    "test",
    "example",
    "factory",
    "mecha",
    // The documentation site answers at `docs.<domain>`. A user holding this
    // handle would serve at `docs.<artifacts-origin>`, which is close enough to
    // read as ours — and reserving it is only free before the first signup.
    "docs",
];

/// A handle is a DNS label, so the rule is DNS's and not ours.
///
/// Stricter than a bundle id, which may contain `_`: an underscore is not
/// legal in a hostname, and a handle that cannot be a hostname is a handle
/// that cannot be served.
pub fn valid_handle(handle: &str) -> anyhow::Result<()> {
    if handle.is_empty() || handle.len() > 63 {
        anyhow::bail!("a handle is 1–63 characters (it is a DNS label)");
    }
    if !handle
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        anyhow::bail!("a handle may hold lowercase letters, digits and `-` only");
    }
    if handle.starts_with('-') || handle.ends_with('-') {
        anyhow::bail!("a handle may not start or end with `-`");
    }
    // Would collide with the `xn--` form of an internationalised name.
    if handle.len() > 4 && handle[2..4] == *"--" {
        anyhow::bail!("`{handle}` looks like a punycode label, which is reserved");
    }
    if RESERVED_HANDLES.contains(&handle) {
        anyhow::bail!("`{handle}` is reserved");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Origins {
    /// The authority as it appears in `Host`: a hostname in production,
    /// `127.0.0.1:8400` in dev.
    pub gate: String,
    pub artifacts: String,
    pub compute: String,
}

/// What a `Host` header resolved to: which origin, and whose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub role: Role,
    /// The leading label, when there is one: `alice` in
    /// `alice.artifacts.example.org`. `None` on the bare origin name and on
    /// the gate, which is shared.
    pub handle: Option<String>,
}

impl Origins {
    /// Which origin this `Host` names, and whose, or nothing.
    ///
    /// **A published URL has to stay resolvable forever**, which is why the
    /// per-user shape is here from the first row rather than added later: a
    /// path prefix (`/u/alice/b/…`) would have to become a hostname the day
    /// notebooks need real isolation, and every URL published in between would
    /// break. A hostname also *is* the isolation — origin is the only boundary
    /// a browser enforces, and a path is not one.
    ///
    /// The gate is deliberately shared: it serves the API and server-rendered
    /// forms, which execute nothing, so there is nothing for an origin to
    /// separate.
    pub fn role_of(&self, host: &str) -> Option<Origin> {
        let host = host.trim().to_ascii_lowercase();
        // A default port is allowed to be absent or present in `Host`, and both
        // spellings mean the same origin.
        let bare = host
            .strip_suffix(":443")
            .or_else(|| host.strip_suffix(":80"))
            .unwrap_or(&host)
            .to_string();

        for (candidate, role) in [
            (&self.gate, Role::Gate),
            (&self.artifacts, Role::Artifacts),
            (&self.compute, Role::Compute),
        ] {
            let candidate = candidate.to_ascii_lowercase();
            if candidate == host || candidate == bare {
                return Some(Origin { role, handle: None });
            }
            // `<handle>.<origin>`, one label deep. Deeper is refused rather
            // than walked: `a.b.artifacts.example.org` is not `a.b`'s, and
            // treating it as `b`'s would let a name nobody owns serve
            // somebody's content.
            if role != Role::Gate {
                if let Some(label) = bare
                    .strip_suffix(&format!(".{candidate}"))
                    .filter(|l| !l.is_empty() && !l.contains('.') && valid_handle(l).is_ok())
                {
                    return Some(Origin {
                        role,
                        handle: Some(label.to_string()),
                    });
                }
            }
        }
        None
    }

    /// A user's own hostname for a role.
    pub fn host_for(&self, role: Role, handle: &str) -> String {
        match role {
            Role::Gate => self.gate.clone(),
            _ => format!("{handle}.{}", self.authority(role)),
        }
    }

    pub fn authority(&self, role: Role) -> &str {
        match role {
            Role::Gate => &self.gate,
            Role::Artifacts => &self.artifacts,
            Role::Compute => &self.compute,
        }
    }

    /// Every distinct name, for the certificate order.
    pub fn names(&self) -> Vec<String> {
        let mut out = vec![
            self.gate.clone(),
            self.artifacts.clone(),
            self.compute.clone(),
        ];
        out.sort();
        out.dedup();
        out
    }

    fn check(&self) -> Result<()> {
        let names = self.names();
        if names.len() != 3 {
            bail!(
                "the three origins must be three distinct names (got {names:?}) — \
                 sharing one would put a notebook and a report under one policy, \
                 which is the whole reason there are three"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tls {
    /// Where Let's Encrypt sends expiry warnings. An `mailto:` URI.
    pub contact: String,
    /// Use the staging directory, whose certificates no browser trusts and
    /// whose rate limits are generous. The way to find out whether the ACME
    /// path works without spending a week's issuance budget on finding out it
    /// does not.
    #[serde(default)]
    pub staging: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listen {
    /// Where TLS is served. Ignored in dev.
    pub https: SocketAddr,
    /// The ACME challenge, and a redirect to HTTPS.
    ///
    /// **Required beside a `[tls]` block**, and that is a change: issuance is
    /// HTTP-01 now, so this port is where certificates come from and not merely
    /// where a human typing a bare hostname lands. Optional only when there is
    /// no TLS at all, which is loopback.
    pub http: Option<SocketAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    /// The largest archive `POST /v1/bundles` will read. A vendored Pyodide
    /// tree is tens of megabytes, so this is generous — but it is a number
    /// rather than "whatever fits", because the endpoint is the one place a
    /// held key can fill the disk.
    pub max_bundle_bytes: u64,
    /// Files in one bundle.
    pub max_bundle_files: usize,
    /// Unauthenticated requests per minute per address.
    pub rate_per_minute: u32,
    /// How many records one drain returns.
    pub drain_batch: usize,
    /// The ceiling on one upload-page POST. The real cap is the request
    /// type's declared attachment budget; this is the route-level backstop
    /// above it. Per-field serde defaults on these three, so a deployed
    /// `[limits]` table written before they existed still parses.
    #[serde(default = "default_max_submission_bytes")]
    pub max_submission_bytes: u64,
    /// Attachment bytes accepted per address per day — the verified-email
    /// gate is the front line; this bounds a verified address that turns
    /// hostile.
    #[serde(default = "default_daily_upload_bytes")]
    pub daily_upload_bytes_per_ip: u64,
    /// Refuse uploads when the disk has less than this free: a 503 now beats
    /// a full disk taking down everything else later. No silent degradation —
    /// the refusal is loud and temporary.
    #[serde(default = "default_min_free_bytes")]
    pub min_free_bytes: u64,
}

fn default_max_submission_bytes() -> u64 {
    40 * 1024 * 1024
}

fn default_daily_upload_bytes() -> u64 {
    64 * 1024 * 1024
}

fn default_min_free_bytes() -> u64 {
    1024 * 1024 * 1024
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bundle_bytes: 256 * 1024 * 1024,
            max_bundle_files: 20_000,
            rate_per_minute: 120,
            drain_batch: 100,
            max_submission_bytes: default_max_submission_bytes(),
            daily_upload_bytes_per_ip: default_daily_upload_bytes(),
            min_free_bytes: default_min_free_bytes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The database, the published bytes, and the certificate cache.
    pub data_dir: PathBuf,
    pub origins: Origins,
    pub listen: Listen,
    /// Absent means plain HTTP, which is only ever right on loopback.
    #[serde(default)]
    pub tls: Option<Tls>,
    #[serde(default)]
    pub limits: Limits,
    /// Where the documentation lives, when there is any to point at. Shown in
    /// the gate's header and on its front page; absent means the links simply
    /// do not render — a deployment with no docs has no dead link.
    #[serde(default)]
    pub docs_url: Option<String>,
    /// Which built-in palette forms are served in. See `mecha_manifest::theme`.
    ///
    /// A deployment-wide setting rather than a per-type one: a manifest
    /// describes *a request*, and what it looks like is a property of whose
    /// front door it is. An unknown name falls back to the default rather than
    /// refusing to start — a typo here must not take the forms down.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// How verification links are sent. Absent means they go to the journal
    /// instead — a working development box, and an operator who can finish a
    /// verification by hand, rather than a silent failure.
    #[serde(default)]
    pub mail: Option<Mail>,
}

fn default_theme() -> String {
    "nocturne".into()
}

/// The mail path, for the one message this box sends.
///
/// The credential is a **path**, not a value. This file describes a deployment
/// and is the sort of thing that gets pasted into a bug report; the key that
/// can send mail as the domain belongs beside the box's other secrets at mode
/// 0600, exactly like the scoped keys.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Mail {
    /// The envelope sender. SES refuses anything but an identity it has
    /// verified, so this is not a free-text field.
    pub from: String,
    /// The SES region. It is part of both the endpoint host and the signing
    /// scope, so a wrong one is a 403 and never a misroute.
    pub region: String,
    /// A TOML file holding `access_key_id` and `secret_access_key`.
    pub credentials: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.check()?;
        Ok(config)
    }

    /// Three loopback ports, one per role, plain HTTP.
    ///
    /// The same router, the same `Host` resolution and the same headers as the
    /// deployed server — a request to `127.0.0.1:8401` carries
    /// `Host: 127.0.0.1:8401`, which is a name the origin table knows.
    pub fn dev(data_dir: PathBuf, base_port: u16) -> Self {
        Config {
            theme: default_theme(),
            mail: None,
            data_dir,
            origins: Origins {
                gate: format!("127.0.0.1:{base_port}"),
                artifacts: format!("127.0.0.1:{}", base_port + 1),
                compute: format!("127.0.0.1:{}", base_port + 2),
            },
            listen: Listen {
                https: ([127, 0, 0, 1], base_port).into(),
                http: None,
            },
            tls: None,
            limits: Limits::default(),
            docs_url: None,
        }
    }

    /// The `example.org` deployment the unit tests assert against.
    ///
    /// One definition rather than a hand-built literal per test module,
    /// because a fixture copied three times is three deployments that drift
    /// apart quietly — and because adding a field to `Config` should be one
    /// edit here, not an archaeology of test modules.
    #[cfg(test)]
    pub(crate) fn example() -> Self {
        Config {
            theme: default_theme(),
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
            docs_url: None,
        }
    }

    fn check(&self) -> Result<()> {
        self.origins.check()?;
        if self.tls.is_none() && !self.listen.https.ip().is_loopback() {
            bail!(
                "no [tls] block, but {} is not loopback — a public origin \
                 serving plain HTTP publishes every reader's request in the \
                 clear and cannot be what was meant",
                self.listen.https
            );
        }
        // Refused rather than warned about, and refused *here* rather than
        // discovered sixty days later. Certificates are issued over HTTP-01, so
        // a box with [tls] and no port 80 comes up serving whatever it has
        // cached and can never renew — it works for two months and then does
        // not, which is the worst shape of failure this project keeps finding.
        if self.tls.is_some() && self.listen.http.is_none() {
            bail!(
                "[tls] is set but [listen] http is not — certificates are \
                 issued over HTTP-01, which is answered on port 80, so this \
                 box could serve what it has cached and would never renew"
            );
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("factory.db")
    }

    pub fn bundle_root(&self) -> PathBuf {
        self.data_dir.join("bundles")
    }

    pub fn attachments_root(&self) -> PathBuf {
        self.data_dir.join("attachments")
    }

    pub fn acme_cache(&self) -> PathBuf {
        self.data_dir.join("acme")
    }

    /// `https://…` in production, `http://…` in dev. What a published URL
    /// actually reads as, which is a thing the API returns and therefore has to
    /// be right.
    pub fn base_url(&self, role: Role) -> String {
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        format!("{scheme}://{}", self.origins.authority(role))
    }

    /// Where a specific user's artifacts live.
    pub fn user_url(&self, role: Role, handle: &str) -> String {
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        format!("{scheme}://{}", self.origins.host_for(role, handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mecha_manifest::ContentClass;

    fn origins() -> Origins {
        Origins {
            gate: "gate.example.org".into(),
            artifacts: "art.example.org".into(),
            compute: "compute.example.org".into(),
        }
    }

    fn bare(role: Role) -> Option<Origin> {
        Some(Origin { role, handle: None })
    }

    fn owned(role: Role, handle: &str) -> Option<Origin> {
        Some(Origin {
            role,
            handle: Some(handle.into()),
        })
    }

    /// There is no default origin, and that is the point: a name we do not
    /// serve gets nothing rather than whichever policy happens to be first.
    #[test]
    fn an_unknown_host_names_no_origin() {
        let o = origins();
        assert_eq!(o.role_of("gate.example.org"), bare(Role::Gate));
        assert_eq!(o.role_of("ART.example.org"), bare(Role::Artifacts));
        assert_eq!(o.role_of("compute.example.org:443"), bare(Role::Compute));
        assert_eq!(o.role_of("example.org"), None);
        assert_eq!(o.role_of(""), None);
        assert_eq!(o.role_of("art.example.org.evil.test"), None);
    }

    /// The per-user shape is in the hostname from the first row, because a
    /// published URL has to stay resolvable forever — moving from a path
    /// prefix to a subdomain later would break every link published in
    /// between, and a path is not an isolation boundary anyway.
    #[test]
    fn a_users_artifacts_live_on_their_own_name() {
        let o = origins();
        assert_eq!(
            o.role_of("alice.art.example.org"),
            owned(Role::Artifacts, "alice")
        );
        assert_eq!(
            o.role_of("ALICE.compute.example.org:443"),
            owned(Role::Compute, "alice")
        );
        assert_eq!(
            o.host_for(Role::Artifacts, "alice"),
            "alice.art.example.org"
        );

        // The gate is shared: it serves an API and server-rendered forms,
        // which execute nothing, so there is nothing for an origin to separate.
        assert_eq!(o.role_of("alice.gate.example.org"), None);

        // Two labels deep is nobody's. Treating `a.b.art.example.org` as `b`'s
        // would let a name nobody owns serve somebody's content.
        assert_eq!(o.role_of("a.b.art.example.org"), None);
        // A label that could not be a handle is not one.
        assert_eq!(o.role_of("-nope.art.example.org"), None);
        assert_eq!(o.role_of("www.art.example.org"), None);
    }

    /// A handle is a DNS label and a piece of somebody's identity. Both halves
    /// of that show up here.
    #[test]
    fn handles_are_dns_labels_and_some_names_are_ours() {
        for good in ["alice", "luke-chang", "lab42", "a"] {
            valid_handle(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
        for bad in [
            "",
            "Alice",       // a hostname is lowercase
            "alice_chang", // legal in a bundle id, illegal in a hostname
            "-alice",
            "alice-",
            "xn--80ak6aa92e", // punycode, which is not ours to hand out
            "www",            // people assume this one is us
            "abuse",          // and this one has to reach a human
            "_acme-challenge",
            &"a".repeat(64),
        ] {
            assert!(valid_handle(bad).is_err(), "`{bad}` was accepted");
        }
    }

    /// The class decides the origin. A compute bundle served under the artifact
    /// policy would not boot; an artifact served under the compute policy would
    /// silently gain `wasm-unsafe-eval`, and that direction is the dangerous
    /// one.
    #[test]
    fn the_class_decides_the_origin_and_only_notebooks_get_compute() {
        assert_eq!(Role::for_class(ContentClass::Static), Role::Artifacts);
        assert_eq!(Role::for_class(ContentClass::Interactive), Role::Artifacts);
        assert_eq!(Role::for_class(ContentClass::Compute), Role::Compute);
    }

    #[test]
    fn three_origins_that_are_not_three_names_are_refused() {
        let mut o = origins();
        o.compute = o.artifacts.clone();
        let err = o.check().unwrap_err().to_string();
        assert!(err.contains("distinct"), "{err}");
    }

    /// Plain HTTP on a public address is a configuration nobody meant to write.
    #[test]
    fn a_public_address_without_tls_is_refused_and_loopback_is_not() {
        let mut config = Config::dev(PathBuf::from("/tmp/x"), 8400);
        config.check().unwrap();
        config.listen.https = ([0, 0, 0, 0], 443).into();
        let err = config.check().unwrap_err().to_string();
        assert!(err.contains("plain HTTP"), "{err}");
    }

    /// Issuance is HTTP-01, so port 80 is where certificates come from. A box
    /// without it serves its cached certificate for sixty days and then stops
    /// — which is why this is a refusal at startup and not a warning.
    #[test]
    fn tls_without_port_80_is_refused_because_nothing_could_ever_renew() {
        let mut config = Config::dev(PathBuf::from("/tmp/x"), 8400);
        config.listen.https = ([0, 0, 0, 0], 443).into();
        config.tls = Some(Tls {
            contact: "mailto:someone@example.org".into(),
            staging: true,
        });
        let err = config.check().unwrap_err().to_string();
        assert!(err.contains("HTTP-01"), "{err}");

        config.listen.http = Some(([0, 0, 0, 0], 80).into());
        config.check().unwrap();
    }

    #[test]
    fn dev_mode_gives_each_role_its_own_port_and_says_so_in_urls() {
        let config = Config::dev(PathBuf::from("/tmp/x"), 8400);
        assert_eq!(
            config.origins.role_of("127.0.0.1:8402"),
            bare(Role::Compute)
        );
        assert_eq!(config.base_url(Role::Artifacts), "http://127.0.0.1:8401");
    }
}
