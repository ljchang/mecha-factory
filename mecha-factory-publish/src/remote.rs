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
    Drain,
}

impl Scope {
    /// The human label a minted key carries. Checked when reading a key file,
    /// so pointing `drain.key` at a publish key is caught here with a sentence
    /// rather than at the far end with a 403.
    fn prefix(&self) -> &'static str {
        match self {
            Scope::Publish => "mk_pub_",
            Scope::Drain => "mk_drn_",
        }
    }

    fn file(&self) -> &'static str {
        match self {
            Scope::Publish => "publish.key",
            Scope::Drain => "drain.key",
        }
    }

    /// The environment variable that overrides the file, which is what makes a
    /// test or a second box a one-line change.
    fn env(&self) -> &'static str {
        match self {
            Scope::Publish => "FACTORY_PUBLISH_KEY",
            Scope::Drain => "FACTORY_DRAIN_KEY",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Publish => "publish",
            Scope::Drain => "drain",
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

    fn request(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<serde_json::Value> {
        self.send(method, path, body, "application/gzip")
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
    // The alias moves second, and only after the bytes are there: the share URL
    // must never point at a version the box does not hold.
    if let Some(target) = alias_to {
        remote.alias(id, Some(target), visibility)?;
    }
    Ok(Some(if pushed.existing {
        format!("{} (identical bytes; the box already had them)", pushed.url)
    } else {
        pushed.url
    }))
}

/// Move the alias on the box without pushing anything, when there is one.
pub fn mirror_alias(
    id: &str,
    version: Option<u32>,
    visibility: Visibility,
) -> Result<Option<String>> {
    let Some(remote) = Remote::configured()? else {
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
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("MECHA_HOME", dir.path());
        std::env::set_var("FACTORY_GATE", "http://gate.example.org");
        let err = Remote::configured().unwrap_err().to_string();
        assert!(err.contains("not https"), "{err}");
        std::env::remove_var("FACTORY_GATE");
        std::env::remove_var("MECHA_HOME");
    }
}
