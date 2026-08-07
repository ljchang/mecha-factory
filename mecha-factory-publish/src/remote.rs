//! Home's side of the wire: sending a version to the box.
//!
//! Everything here is push. The origin never dials home — that is the whole
//! shape of the security model — so this is the only code in the project that
//! opens a connection between the two, and it holds the only credential that
//! crosses.
//!
//! Three decisions:
//!
//! - **A remote is optional, and its absence is not an error.** `publish` is a
//!   local act that works on a laptop with no server at all; pushing is what
//!   happens *as well*, when `~/.mecha/factory/config.toml` names a gate. A
//!   publisher that failed without a server would make the local store useless
//!   for exactly the case it was built for first.
//! - **`sources` is stripped before it is sent, not only when it arrives.** The
//!   array holds absolute paths inside the user's home directory; the server
//!   strips it too, and neither side relies on the other. This side is where
//!   the paths actually live, so this is where not sending them is cheapest.
//! - **A push failure never rewrites local history.** The version is already in
//!   the local store, immutable, with the alias where it belongs; a failed push
//!   leaves all of that alone and says what to retry. `push` is idempotent at
//!   the far end — identical bytes return the version that already holds them —
//!   so retrying is free and safe.

use anyhow::{bail, Context, Result};
use mecha_manifest::{BundleManifest, Visibility};
use std::path::{Path, PathBuf};

use crate::store::{mecha_home, BundleStore};

/// Which credential a call presents, and therefore what it may do.
///
/// Two keys rather than one, and they live in different files: publishing
/// writes the public surface, draining reads other people's submissions. A
/// compromise of one is not a compromise of the other, and the box enforces it
/// from the database row rather than from the token's prefix — the prefix is a
/// label for humans, so a drain key spelled `mk_pub_` still cannot publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Publish,
    /// Move an alias, or serve a form. Held by a person, not by an agent —
    /// see `Scope::Release` on the box for why the review moved off the
    /// client's config and onto the credential.
    Release,
    Drain,
    /// Run the box. Held by the operator's own machine and never installed
    /// by pairing — `connect` mints publish and drain, full stop. The verbs
    /// behind it are CLI-only, never MCP tools: operator power stays off the
    /// agent surface for the same reason drain's prose does.
    Operate,
}

impl Scope {
    /// The human label a minted key carries. Checked when reading a key file,
    /// so pointing `drain.key` at a publish key is caught here with a sentence
    /// rather than at the far end with a 403.
    fn prefix(&self) -> &'static str {
        match self {
            Scope::Publish => "mk_pub_",
            Scope::Release => "mk_rel_",
            Scope::Drain => "mk_drn_",
            Scope::Operate => "mk_opr_",
        }
    }

    fn file(&self) -> &'static str {
        match self {
            Scope::Publish => "publish.key",
            Scope::Release => "release.key",
            Scope::Drain => "drain.key",
            Scope::Operate => "operate.key",
        }
    }

    /// The environment variable that overrides the file, which is what makes a
    /// test or a second box a one-line change.
    fn env(&self) -> &'static str {
        match self {
            Scope::Publish => "FACTORY_PUBLISH_KEY",
            Scope::Release => "FACTORY_RELEASE_KEY",
            Scope::Drain => "FACTORY_DRAIN_KEY",
            Scope::Operate => "FACTORY_OPERATE_KEY",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Publish => "publish",
            Scope::Release => "release",
            Scope::Drain => "drain",
            Scope::Operate => "operate",
        }
    }
}

/// Where the box is, and what to present to it.
pub struct Remote {
    gate: String,
    key: String,
    scope: Scope,
}

/// Written by hand rather than derived, because a derived one would print the
/// key — into a log, a panic message, or an error somebody pastes into a chat.
/// The one struct in this repository holding a live credential is the one place
/// that is worth eleven lines.
impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("gate", &self.gate)
            .field("scope", &self.scope)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// What the far end said about a push.
#[derive(Debug)]
pub struct Pushed {
    pub version: u32,
    pub existing: bool,
    pub url: String,
    pub version_url: String,
}

#[derive(serde::Deserialize)]
struct Config {
    /// `https://gate.example.org`, with no trailing slash.
    gate: String,
}

impl Remote {
    /// `~/.mecha/factory/`.
    pub fn dir() -> Result<PathBuf> {
        Ok(mecha_home()?.join("factory"))
    }

    /// The configured remote, or nothing.
    ///
    /// `$FACTORY_GATE` overrides the file, which is what makes a test or a
    /// second box a one-line change rather than an edit to a file the real
    /// deployment reads.
    pub fn configured() -> Result<Option<Remote>> {
        Self::configured_for(Scope::Publish)
    }

    /// The configured remote presenting one particular credential.
    pub fn configured_for(scope: Scope) -> Result<Option<Remote>> {
        let dir = Self::dir()?;
        let gate = match std::env::var("FACTORY_GATE") {
            Ok(gate) if !gate.is_empty() => gate,
            _ => {
                let path = dir.join("config.toml");
                let Ok(text) = std::fs::read_to_string(&path) else {
                    return Ok(None);
                };
                let config: Config =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                config.gate
            }
        };
        let gate = gate.trim_end_matches('/').to_string();
        if !gate.starts_with("https://") && !gate.starts_with("http://127.0.0.1") {
            bail!(
                "the factory gate `{gate}` is not https. The publish key travels \
                 on this connection."
            );
        }

        let key_path = match std::env::var(scope.env()) {
            Ok(path) if !path.is_empty() => PathBuf::from(path),
            _ => dir.join(scope.file()),
        };
        let key = read_key(&key_path, scope)?;
        Ok(Some(Remote { gate, key, scope }))
    }

    /// The configured remote presenting this credential — or `None` when the
    /// key simply is not on this machine.
    ///
    /// The distinction from [`Remote::configured_for`] is which absence is
    /// ordinary. A missing *publish* key on a machine with a gate configured
    /// is a broken setup and stays a loud error — a push that silently did
    /// not happen is the degrading shape. A missing *release* key is the
    /// designed state of every paired agent machine, and the callers that
    /// look one up want "not here" as an answer, not a failure: the alias
    /// stays put and the caller says where to release from. Before this, the
    /// graceful arm existed and the lookup could not reach it — the first
    /// machine to hold publish-without-release found that out.
    pub fn installed(scope: Scope) -> Result<Option<Remote>> {
        let has_env = std::env::var(scope.env()).is_ok_and(|p| !p.is_empty());
        if !has_env && !Self::dir()?.join(scope.file()).exists() {
            return Ok(None);
        }
        Self::configured_for(scope)
    }

    pub fn gate(&self) -> &str {
        &self.gate
    }

    /// Send one stored version to the box.
    pub fn push(&self, store: &BundleStore, id: &str, version: u32) -> Result<Pushed> {
        let dir = store.version_dir(id, version);
        if !dir.is_dir() {
            bail!("{id} has no version {version} locally");
        }
        let archive = archive(&dir)?;
        let body = self
            .request("POST", "/v1/bundles", Some(&archive))
            .with_context(|| format!("publishing {id} v{version}"))?;

        Ok(Pushed {
            version: body["version"].as_u64().unwrap_or(version as u64) as u32,
            existing: body["existing"].as_bool().unwrap_or(false),
            url: body["url"].as_str().unwrap_or_default().to_string(),
            version_url: body["version_url"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// Move the share URL on the box, and set who may read it.
    pub fn alias(&self, id: &str, version: Option<u32>, visibility: Visibility) -> Result<String> {
        let payload = serde_json::json!({
            "version": version,
            "visibility": match visibility {
                Visibility::Public => "public",
                Visibility::Private => "private",
            },
        });
        let body = self
            .request(
                "POST",
                &format!("/v1/bundles/{id}/alias"),
                Some(payload.to_string().as_bytes()),
            )
            .with_context(|| format!("aliasing {id}"))?;
        Ok(body["url"].as_str().unwrap_or_default().to_string())
    }

    /// Upload a request type, which is what makes a form exist at all.
    ///
    /// The manifest travels as **TOML**, exactly as written here, and the box
    /// parses it with the same `mecha_manifest` code this side used to check
    /// it. Sending a JSON rendering instead would make the two ends agree only
    /// as long as two serialisations agreed; sending the source means the form,
    /// the JSON Schema and both validators are all derived from one text.
    pub fn type_push(&self, manifest: &str, id: &str) -> Result<serde_json::Value> {
        self.send(
            "PUT",
            &format!("/v1/types/{id}"),
            Some(manifest.as_bytes()),
            "text/plain; charset=utf-8",
        )
        .with_context(|| format!("uploading the `{id}` request type"))
    }

    /// What request types the box is serving forms for.
    pub fn type_list(&self) -> Result<serde_json::Value> {
        self.request("GET", "/v1/types", None)
    }

    /// Take everything verified and not yet acknowledged, from `since` on.
    ///
    /// A **pure read**: the box marks nothing, so a response that never
    /// arrives costs a repeat rather than a stranger's request. Which is the
    /// right way round — see [`Remote::ack`].
    pub fn drain(&self, since: i64) -> Result<serde_json::Value> {
        self.request("GET", &format!("/v1/queue?since={since}"), None)
            .context("draining the queue")
    }

    /// Delete what home has safely stored. The *only* thing that removes a
    /// record from the box.
    ///
    /// Acknowledge after writing, never before: a crash between the two means
    /// the record arrives twice, and the duplicate is caught by the local
    /// store's own id. A crash the other way round loses somebody's request
    /// with no trace anywhere, which is the failure this whole surface exists
    /// to prevent.
    pub fn ack(&self, seqs: &[i64]) -> Result<usize> {
        let payload = serde_json::json!({ "seqs": seqs });
        let body = self
            .send(
                "POST",
                "/v1/queue/ack",
                Some(payload.to_string().as_bytes()),
                "application/json",
            )
            .context("acknowledging drained records")?;
        Ok(body["deleted"].as_u64().unwrap_or(0) as usize)
    }

    /// Is it up, and what does it hold.
    pub fn health(&self) -> Result<serde_json::Value> {
        self.request("GET", "/v1/health", None)
    }

    /// One attachment's bytes, by the id the drain named.
    ///
    /// The byte-returning sibling of [`Remote::send`], which reads every
    /// response `to_string` and JSON-parses it and so structurally cannot
    /// carry a blob. Capped at the size the drained metadata declared plus
    /// one: the box is a machine we assume is lost, so its Content-Length is
    /// a claim like any other, and a body running past the declaration is
    /// refused rather than buffered. The caller verifies the digest; this
    /// only carries bytes.
    pub fn fetch_attachment(&self, id: &str, declared_size: u64) -> Result<Vec<u8>> {
        let url = format!("{}/v1/queue/attachments/{id}", self.gate);
        let mut response = ureq::get(&url)
            .header("Authorization", &format!("Bearer {}", self.key))
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .with_context(|| format!("GET {url}"))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let text = response
                .body_mut()
                .read_to_string()
                .unwrap_or_else(|_| String::new());
            bail!("{url} answered {status}: {text}");
        }
        let mut bytes = Vec::new();
        let limit = declared_size.saturating_add(1);
        use std::io::Read;
        response
            .body_mut()
            .as_reader()
            .take(limit)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {url}"))?;
        if bytes.len() as u64 > declared_size {
            bail!(
                "{url} sent more than the {declared_size} bytes its own \
                 metadata declared"
            );
        }
        Ok(bytes)
    }

    fn request(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<serde_json::Value> {
        self.send(method, path, body, "application/gzip")
    }

    /// One JSON call, for the operator verbs. Public within the crate so the
    /// CLI can drive the admin endpoints without re-owning HTTP.
    pub fn json(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        match body {
            Some(value) => self.send(
                method,
                path,
                Some(value.to_string().as_bytes()),
                "application/json",
            ),
            None => self.send(method, path, None, "application/json"),
        }
    }

    /// One HTTP call, with the credential attached and the far end's own error
    /// message preserved.
    ///
    /// `method` used to be decoration — the body decided GET or POST, and the
    /// content type was always gzip because the only body was an archive.
    /// Uploading a manifest (TOML, `PUT`) and acknowledging a drain (JSON,
    /// `POST`) both broke that, and a parameter that was ignored is worse than
    /// no parameter, because every call site read as though it meant something.
    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        content_type: &str,
    ) -> Result<serde_json::Value> {
        let url = format!("{}{path}", self.gate);
        let auth = format!("Bearer {}", self.key);
        // ureq 3 types a request by whether it carries a body, so the verb
        // cannot be a runtime string — which is just as well, since it makes
        // the (method, body) pairs this client actually speaks explicit, and
        // an unlisted pair a clear error rather than a malformed request.
        let mut response = match (method, body) {
            ("GET", None) => ureq::get(&url)
                .header("Authorization", &auth)
                .config()
                .http_status_as_error(false)
                .build()
                .call(),
            ("POST", Some(bytes)) => ureq::post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", content_type)
                .config()
                .http_status_as_error(false)
                .build()
                .send(bytes),
            ("PUT", Some(bytes)) => ureq::put(&url)
                .header("Authorization", &auth)
                .header("Content-Type", content_type)
                .config()
                .http_status_as_error(false)
                .build()
                .send(bytes),
            (method, body) => bail!(
                "this client speaks GET, POST and PUT; `{method}` with {} body is not one of them",
                if body.is_some() { "a" } else { "no" }
            ),
        }
        .with_context(|| format!("{method} {url}"))?;

        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .unwrap_or_else(|_| String::new());
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({
            "error": text
        }));
        if !(200..300).contains(&status) {
            // The server's message is the actionable part — which file had an
            // external reference, which version already exists — so it is
            // surfaced rather than replaced with the status code.
            bail!(
                "{url} answered {status}: {}",
                parsed["error"].as_str().unwrap_or(&text)
            );
        }
        Ok(parsed)
    }
}

/// Push a version and set the alias on the box, when there is one.
///
/// `Ok(None)` means no remote is configured, which is an ordinary state and not
/// a failure: publishing is a local act that works on a laptop with no server.
/// Every caller — the CLI, the MCP tool — goes through this, so "does the world
/// see it?" has one answer rather than three.
pub fn mirror(
    store: &BundleStore,
    id: &str,
    version: u32,
    alias_to: Option<u32>,
    visibility: Visibility,
) -> Result<Option<String>> {
    let Some(remote) = Remote::configured()? else {
        return Ok(None);
    };
    let pushed = remote.push(store, id, version)?;
    let mut note = String::new();
    // The alias moves second, and only after the bytes are there: the share URL
    // must never point at a version the box does not hold.
    //
    // And it moves under a *different* credential. Pushing writes a version
    // nobody can read; aliasing is what a reader sees. A caller holding only a
    // publish key gets the bytes up and is told plainly that they are not
    // published — which is the shape an agent should have: it can do all the
    // work and cannot be the one who decides the world sees it.
    if let Some(target) = alias_to {
        match Remote::installed(Scope::Release)? {
            Some(releaser) => {
                releaser.alias(id, Some(target), visibility)?;
            }
            None => {
                note = format!(
                    "\nversion {target} is on the box and is not published: no release \
                     key here, so the alias did not move. Release it with \
                     `factory-publish alias {id} --version {target}` from a machine \
                     that has one."
                );
            }
        }
    }
    Ok(Some(if pushed.existing {
        format!(
            "{} (identical bytes; the box already had them){note}",
            pushed.url
        )
    } else {
        format!("{}{note}", pushed.url)
    }))
}

/// Move the alias on the box without pushing anything, when there is one.
///
/// Release-scoped: this verb *is* the publication, so it is the one an agent's
/// key deliberately cannot perform.
pub fn mirror_alias(
    id: &str,
    version: Option<u32>,
    visibility: Visibility,
) -> Result<Option<String>> {
    let Some(remote) = Remote::installed(Scope::Release)? else {
        return Ok(None);
    };
    Ok(Some(remote.alias(id, version, visibility)?))
}

/// A key file, with the checks that make one a key file.
fn read_key(path: &Path, scope: Scope) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "reading {} — mint one on the box with `factory key create --scope {}`",
            path.display(),
            scope.as_str()
        )
    })?;
    let token = text.trim().to_string();
    if !token.starts_with(scope.prefix()) {
        // The box decides scope from the database row, not from this label, so
        // presenting the wrong key would fail there anyway — with a 403 that
        // says nothing about which file is wrong. Catching it here names the
        // file.
        bail!(
            "{} does not hold a {} key (expected `{}…`)",
            path.display(),
            scope.as_str(),
            scope.prefix()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            // A warning rather than a refusal: refusing to publish over a
            // permission bit is how somebody ends up moving the key somewhere
            // worse.
            eprintln!(
                "warning: {} is readable by others; chmod 600 it",
                path.display()
            );
        }
    }
    Ok(token)
}

/// A version directory as a gzipped tar, with `bundle.json` rewritten.
fn archive(dir: &Path) -> Result<Vec<u8>> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for (path, bytes) in files.iter_mut() {
        if path == mecha_manifest::MANIFEST_FILE {
            let mut manifest = BundleManifest::from_json(&String::from_utf8_lossy(bytes))?;
            // Never sent. The server strips it too; neither side relies on the
            // other, and the paths live on this side.
            manifest.sources.clear();
            *bytes = manifest.to_json().into_bytes();
        }
    }

    let mut builder = tar::Builder::new(Vec::new());
    for (path, bytes) in &files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, path, &bytes[..])?;
    }
    let tar = builder.into_inner()?;

    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar)?;
    Ok(encoder.finish()?)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &path, out)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("walked from root")
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, std::fs::read(&path)?));
        }
        // Symlinks are not followed and not sent: the server refuses them
        // anyway, and a link here would send bytes from outside the bundle.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mecha_manifest::ContentClass;

    /// The digest the server recomputes is taken over the files *excluding*
    /// `bundle.json`, so rewriting the manifest cannot change the address —
    /// which is what makes stripping the sources free rather than a reason for
    /// the two ends to disagree.
    #[test]
    fn stripping_the_sources_does_not_change_the_address() {
        let dir = tempfile::tempdir().unwrap();
        let version = dir.path().join("1");
        std::fs::create_dir_all(&version).unwrap();
        std::fs::write(version.join("index.html"), "<h1>Monday</h1>").unwrap();

        let files = [("index.html", b"<h1>Monday</h1>".as_slice())];
        let digest = mecha_manifest::digest_files(files);
        let manifest = BundleManifest {
            id: "brief".into(),
            version: 1,
            title: "t".into(),
            description: None,
            template: "report".into(),
            class: ContentClass::Static,
            visibility: Visibility::Private,
            digest: Some(digest.clone()),
            published_at: None,
            sources: vec![PathBuf::from("/home/someone/.mecha/work/morning/x.md")],
        };
        std::fs::write(version.join("bundle.json"), manifest.to_json()).unwrap();

        let archive = archive(&version).unwrap();
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&archive[..]), &mut raw)
            .unwrap();

        let mut sent = Vec::new();
        for entry in tar::Archive::new(&raw[..]).entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().display().to_string();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            sent.push((path, bytes));
        }

        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(
            !text.contains("/home/someone"),
            "home paths went on the wire"
        );
        assert_eq!(
            mecha_manifest::digest_files(sent.iter().map(|(p, b)| (p.as_str(), b.as_slice()))),
            digest,
            "the address changed in transit"
        );
    }

    /// The key travels on this connection, so the connection has to be one.
    #[test]
    fn a_plaintext_gate_is_refused() {
        let _env = crate::env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("MECHA_HOME", dir.path());
        std::env::set_var("FACTORY_GATE", "http://gate.example.org");
        let err = Remote::configured().unwrap_err().to_string();
        assert!(err.contains("not https"), "{err}");
        std::env::remove_var("FACTORY_GATE");
        std::env::remove_var("MECHA_HOME");
    }
}

/// What `connect` was told, resolved by the CLI before anything travels.
pub struct Connect<'a> {
    /// `https://gate.example.org`.
    pub gate: &'a str,
    /// The pairing code, from the welcome page or `factory pair create`.
    pub code: &'a str,
    /// The handle the person expects this machine to publish for. **Sent to
    /// the server, which refuses a mismatch without spending the code** — the
    /// assertion is the defence against the reversed device-code phish
    /// (running a code Mallory sent means asserting `mallory`, which is
    /// exactly what the person typing their own handle will not do), and it
    /// lives in the protocol rather than in a prompt so no client, human or
    /// agent, can skip it.
    pub handle: &'a str,
    /// Free text for the box's `key list`; the CLI defaults it to the
    /// machine's hostname.
    pub label: &'a str,
    /// Overwrite keys already installed here. The old keys stay valid on the
    /// box until revoked, and the summary says so.
    pub replace: bool,
}

/// Pair this machine: spend the code, install the keys, and say what
/// happened. Returns the summary the CLI prints.
pub fn connect(args: &Connect) -> Result<String> {
    let gate = args.gate.trim_end_matches('/');
    if !gate.starts_with("https://") && !gate.starts_with("http://127.0.0.1") {
        bail!("the factory gate `{gate}` is not https. The keys travel on this connection.");
    }
    let handle = args.handle.trim().to_ascii_lowercase();
    if handle.is_empty() {
        bail!("an empty handle asserts nothing");
    }

    let dir = Remote::dir()?;
    let publish_path = dir.join(Scope::Publish.file());
    let drain_path = dir.join(Scope::Drain.file());
    if !args.replace {
        for path in [&publish_path, &drain_path] {
            if path.exists() {
                bail!(
                    "{} already exists — this machine is connected. Pass \
                     `--replace` to pair it afresh; the keys it holds now stay \
                     valid on the box until revoked there.",
                    path.display()
                );
            }
        }
    }

    // The one unauthenticated call this client makes: the code is the
    // credential. The server's refusal is a single message for every kind of
    // dead or mismatched code, and it is surfaced as-is.
    let url = format!("{gate}/v1/pair");
    let payload = serde_json::json!({
        "code": args.code.trim(),
        "handle": handle,
        "label": args.label,
    });
    let mut response = ureq::post(&url)
        .header("Content-Type", "application/json")
        .config()
        .http_status_as_error(false)
        .build()
        .send(payload.to_string().as_bytes())
        .with_context(|| format!("POST {url}"))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .unwrap_or_else(|_| String::new());
    let body: serde_json::Value =
        serde_json::from_str(&text).unwrap_or(serde_json::json!({ "error": text }));
    if !(200..300).contains(&status) {
        bail!(
            "{url} answered {status}: {}",
            body["error"].as_str().unwrap_or(&text)
        );
    }

    let granted = body["handle"].as_str().unwrap_or_default();
    if granted != handle {
        // The server enforces the assertion, so this firing means the far end
        // is not the protocol this client speaks — refuse rather than install
        // keys for an account nobody asserted.
        bail!("{url} paired `{granted}` where `{handle}` was asserted — keys not installed");
    }
    let publish_key = body["publish_key"].as_str().unwrap_or_default();
    let drain_key = body["drain_key"].as_str().unwrap_or_default();
    if publish_key.is_empty() || drain_key.is_empty() {
        bail!("{url} answered without keys — nothing was installed");
    }

    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    write_key(&publish_path, publish_key)?;
    write_key(&drain_path, drain_key)?;

    // The gate is remembered beside the keys, so the next `publish` needs no
    // environment. An existing config naming a *different* gate is refused
    // rather than silently rewritten: keys for one deployment beside a config
    // pointing at another is a machine that publishes somewhere surprising.
    let config_path = dir.join("config.toml");
    match std::fs::read_to_string(&config_path) {
        Ok(text) => {
            let existing: Option<Config> = toml::from_str(&text).ok();
            match existing {
                Some(config) if config.gate.trim_end_matches('/') == gate => {}
                _ if args.replace => std::fs::write(&config_path, format!("gate = \"{gate}\"\n"))?,
                _ => bail!(
                    "{} names a different gate — re-run with `--replace` if \
                     this machine is moving deployments",
                    config_path.display()
                ),
            }
        }
        Err(_) => std::fs::write(&config_path, format!("gate = \"{gate}\"\n"))?,
    }

    let mut summary = format!(
        "This machine now publishes for `{handle}`.\n\
         Artifacts: {}\n\
         Publish key {} and drain key {} installed in {} — each machine pairs \
         separately, and either key can be revoked on the box without touching \
         the others.",
        body["artifacts_url"].as_str().unwrap_or_default(),
        body["publish_key_id"].as_str().unwrap_or("?"),
        body["drain_key_id"].as_str().unwrap_or("?"),
        dir.display(),
    );
    // The review's standing constraint: a paired agent machine holds publish
    // and drain, never release. One already here is worth a loud line, not a
    // refusal — the person may be pairing the machine they release from.
    if dir.join(Scope::Release.file()).exists() {
        summary.push_str(
            "\nwarning: release.key is present on this machine. An agent here \
             can make artifacts public with no review — keep release keys on \
             the machine you review from, not the one that publishes.",
        );
    }
    Ok(summary)
}

/// Retire this machine's keys: each revokes itself on the box, then leaves
/// the disk. Returns the summary the CLI prints.
///
/// An already-dead key (revoked from the account page, say) still gets its
/// file removed — the box refusing it means it is exactly as retired as we
/// wanted, and a cleanup that failed because the cleanup was already done
/// would be the one wrong answer. The gate memory (`config.toml`) stays: it
/// is an address, not a credential, and reconnecting wants it.
pub fn disconnect() -> Result<String> {
    let dir = Remote::dir()?;
    let mut lines = Vec::new();
    let mut any = false;
    for scope in [Scope::Publish, Scope::Drain] {
        let path = dir.join(scope.file());
        if !path.exists() {
            continue;
        }
        any = true;
        match Remote::configured_for(scope) {
            Ok(Some(remote)) => match remote.request("POST", "/v1/disconnect", Some(b"")) {
                Ok(body) => lines.push(format!(
                    "{} key {} revoked on the box",
                    scope.as_str(),
                    body["revoked"].as_str().unwrap_or("?")
                )),
                Err(e) if e.to_string().contains("401") => lines.push(format!(
                    "{} key was already dead on the box",
                    scope.as_str()
                )),
                Err(e) => {
                    // A key we could not revoke stays on disk: deleting the
                    // local copy of a still-live credential is tidiness
                    // dressed as security, and it removes the only easy way
                    // to retry.
                    lines.push(format!(
                        "{} key could NOT be revoked ({e}); its file stays for a retry",
                        scope.as_str()
                    ));
                    continue;
                }
            },
            Ok(None) => lines.push(format!(
                "{} key has no gate configured; removing the file only",
                scope.as_str()
            )),
            Err(e) => {
                lines.push(format!(
                    "{} key unreadable ({e}); removing the file",
                    scope.as_str()
                ));
            }
        }
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    if !any {
        return Ok("nothing to disconnect — no keys are installed here".to_string());
    }
    lines.push(
        "This machine no longer publishes. `factory-publish connect` with a \
         fresh pairing code brings it back."
            .to_string(),
    );
    Ok(lines.join("\n"))
}

/// A credential hits the disk at 0600 from its first byte — written through
/// a file created with the mode already set, never chmodded after.
fn write_key(path: &Path, token: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    // An existing file keeps its old mode, so pairing over a leaky one is
    // tightened rather than trusted.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    writeln!(file, "{token}")?;
    Ok(())
}

#[cfg(test)]
mod connect_tests {
    use super::*;

    /// A one-shot gate: answers the next request with the given status and
    /// JSON, then goes away. `connect` needs nothing more from the far end,
    /// and a stub keeps the test about *this* side — the server's half is
    /// tested in the server's own suite.
    fn one_shot_gate(status: u16, body: serde_json::Value) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // The whole request, not the first segment: answering after one
            // read and closing races the client's body write under load, and
            // the reset reads as a flaky connection error in whichever test
            // drew the slow lane.
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buffer[..n]),
                }
                if let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..split]).to_lowercase();
                    let expected: usize = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if request.len() >= split + 4 + expected {
                        break;
                    }
                }
            }
            let text = body.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        });
        format!("http://{address}")
    }

    fn paired_body() -> serde_json::Value {
        serde_json::json!({
            "handle": "alice",
            "publish_key": "mk_pub_id.secret",
            "publish_key_id": "pubid",
            "drain_key": "mk_drn_id.secret",
            "drain_key_id": "drnid",
            "artifacts_url": "https://alice.art.example.org",
        })
    }

    /// The install: keys land at 0600, the gate is remembered, and the
    /// summary names the handle the machine now publishes for.
    #[test]
    fn connect_installs_keys_at_0600_and_remembers_the_gate() {
        let _env = crate::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("MECHA_HOME", home.path());
        let gate = one_shot_gate(200, paired_body());

        let summary = connect(&Connect {
            gate: &gate,
            code: "code",
            handle: "Alice", // case is the client's to normalise
            label: "rig",
            replace: false,
        })
        .unwrap();
        std::env::remove_var("MECHA_HOME");

        assert!(summary.contains("`alice`"), "{summary}");
        let dir = home.path().join("factory");
        let key = std::fs::read_to_string(dir.join("publish.key")).unwrap();
        assert_eq!(key.trim(), "mk_pub_id.secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in ["publish.key", "drain.key"] {
                let mode = std::fs::metadata(dir.join(file))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{file} is {mode:o}");
            }
        }
        let config = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(config.contains(&gate), "{config}");
    }

    /// A refusal installs nothing, and the server's one message is surfaced
    /// as-is — it is the actionable half of the error.
    #[test]
    fn a_refused_code_installs_nothing() {
        let _env = crate::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("MECHA_HOME", home.path());
        let gate = one_shot_gate(
            404,
            serde_json::json!({"error": "that code is not valid for that handle"}),
        );
        let err = connect(&Connect {
            gate: &gate,
            code: "code",
            handle: "alice",
            label: "",
            replace: false,
        })
        .unwrap_err()
        .to_string();
        std::env::remove_var("MECHA_HOME");
        assert!(err.contains("not valid for that handle"), "{err}");
        assert!(!home.path().join("factory").join("publish.key").exists());
    }

    /// A server that pairs a handle nobody asserted is not this protocol:
    /// keys are refused rather than installed for a surprise account.
    #[test]
    fn keys_for_an_unasserted_handle_are_refused() {
        let _env = crate::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("MECHA_HOME", home.path());
        let mut body = paired_body();
        body["handle"] = serde_json::json!("mallory");
        let gate = one_shot_gate(200, body);
        let err = connect(&Connect {
            gate: &gate,
            code: "code",
            handle: "alice",
            label: "",
            replace: false,
        })
        .unwrap_err()
        .to_string();
        std::env::remove_var("MECHA_HOME");
        assert!(err.contains("`mallory`"), "{err}");
        assert!(!home.path().join("factory").join("publish.key").exists());
    }

    /// A machine that is already connected is told so before any network
    /// happens, and `--replace` is the deliberate way over it.
    #[test]
    fn existing_keys_refuse_a_second_pairing_without_replace() {
        let _env = crate::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("MECHA_HOME", home.path());
        let dir = home.path().join("factory");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("publish.key"), "mk_pub_old.key\n").unwrap();

        // A gate that would refuse the connection — reaching it would fail
        // the test, which is the point: the refusal happens first.
        let err = connect(&Connect {
            gate: "http://127.0.0.1:1",
            code: "code",
            handle: "alice",
            label: "",
            replace: false,
        })
        .unwrap_err()
        .to_string();
        std::env::remove_var("MECHA_HOME");
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("--replace"), "{err}");
    }
}

#[cfg(test)]
mod installed_tests {
    use super::*;

    /// The paired-machine state: gate configured, publish key present, no
    /// release key. `installed` answers "not here" where `configured_for`
    /// answers with an error — and the error stays right for the publish
    /// key, whose absence on a configured machine is a broken setup.
    #[test]
    fn a_missing_release_key_is_an_answer_and_a_missing_publish_key_is_an_error() {
        let _env = crate::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("MECHA_HOME", home.path());
        let dir = home.path().join("factory");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "gate = \"https://gate.example.org\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("publish.key"), "mk_pub_id.secret\n").unwrap();

        assert!(Remote::installed(Scope::Release).unwrap().is_none());
        assert!(Remote::installed(Scope::Publish).unwrap().is_some());
        assert!(Remote::configured_for(Scope::Release).is_err());

        // The push path deliberately keeps `configured_for`: a machine with
        // a gate and no publish key is broken, not unpaired, and `mirror`
        // must keep saying so loudly rather than skipping the push.
        std::fs::remove_file(dir.join("publish.key")).unwrap();
        assert!(Remote::configured_for(Scope::Publish).is_err());
        std::env::remove_var("MECHA_HOME");
    }
}
