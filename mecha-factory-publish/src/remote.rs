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
    /// Replace an instrument's slot cache. Held by the slot-refresh timer —
    /// a scheduled command with no model and no human — which is exactly why
    /// it exists apart from `Publish`: the credential that lives in a
    /// systemd unit's environment should open one narrow door.
    Slots,
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
            Scope::Slots => "mk_slt_",
            Scope::Operate => "mk_opr_",
        }
    }

    fn file(&self) -> &'static str {
        match self {
            Scope::Publish => "publish.key",
            Scope::Release => "release.key",
            Scope::Drain => "drain.key",
            Scope::Slots => "slots.key",
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
            Scope::Slots => "FACTORY_SLOTS_KEY",
            Scope::Operate => "FACTORY_OPERATE_KEY",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Publish => "publish",
            Scope::Release => "release",
            Scope::Drain => "drain",
            Scope::Slots => "slots",
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
    /// The gate's viewer page for this bundle — chrome, version menu, owner
    /// controls, the bytes framed inside it.
    ///
    /// `None` against a box that predates the field, which is the whole
    /// reason it is an `Option` rather than an empty string: every caller has
    /// to decide what to say when there is no page to name, and a blank one
    /// smuggled into a sentence reads as a broken link rather than an old
    /// server.
    pub viewer_url: Option<String>,
}

/// Who the box will serve a mirrored bundle to, as far as *this* operation
/// actually decided it.
///
/// Who the box will serve a mirrored bundle to, as far as *this* operation
/// actually decided it.
///
/// Five states rather than a `Visibility`, because three of them are not
/// visibilities at all and collapsing them is how a report comes to assert
/// something nobody established. A push that could not move the alias, and a
/// `--no-alias` push, both leave the question exactly as they found it; a
/// takedown answers it in a way neither "public" nor "private" says; and a
/// caller who was never told a visibility moves the alias without deciding
/// who reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serves {
    /// The alias names a version and this operation made it readable.
    Everyone,
    /// The alias names a version and this operation made it private.
    OwnerOnly,
    /// The alias now points at nothing: a takedown.
    Nothing,
    /// The alias now names a version and who may read it was **left alone**,
    /// because nobody asked for a visibility.
    ///
    /// Distinct from [`Serves::Unchanged`] on purpose: there the alias itself
    /// never moved, here it did. Both refuse to name a reader, and that is
    /// the point — the box holds the answer and this side was not told it.
    AsBefore,
    /// The alias was not touched at all, so who may read it is whatever it
    /// already said — and guessing is the same lie in whichever direction.
    Unchanged,
}

/// Where a bundle on the box can be read, in both spellings, and what did
/// not happen.
///
/// The two URLs are for different readers and the difference is not cosmetic:
/// `viewer` is the page a **person** is sent, and `bare` is what a machine
/// fetches, a citation names, and a projector shows. Everything that reports
/// a mirror goes through this so the choice is made once — before, each caller
/// printed whichever single URL it happened to be holding, which is how a
/// private bundle's 404 came to be announced as "it is live at".
#[derive(Debug)]
pub struct Mirrored {
    pub viewer: Option<String>,
    pub bare: String,
    pub serves: Serves,
    /// What the caller must be told about what did *not* happen — an alias
    /// that could not move for want of a release key on this machine, or
    /// bytes the box already had.
    pub note: String,
}

impl Mirrored {
    /// The page to hand a person, when the box named one.
    ///
    /// **Deliberately not a fallback to the bytes URL.** That is what this
    /// returned first, and it reinstated the exact lie the type exists to
    /// remove: with no page, `page` and `bare` became the same string, and
    /// the private arm then read "only you can open it: <URL>. <the same URL>
    /// serves nobody" about an origin that holds no session and answers 404.
    /// A missing page is a thing to say less about, never a different URL to
    /// name in its place.
    pub fn page(&self) -> Option<&str> {
        self.viewer.as_deref()
    }

    /// One honest sentence about who can open it — for an agent's answer, and
    /// anything else with room for prose rather than columns.
    ///
    /// Two variants of every arm, because a box that named no page has no
    /// page, and the versions without one are the arms real traffic
    /// exercises until the origin is redeployed. The rule they all obey:
    /// **the bare URL is named only where it actually serves somebody.**
    /// Private, taken down, and alias-untouched all mean it does not, so
    /// those arms describe it rather than offering it.
    pub fn sentence(&self) -> String {
        let bare = &self.bare;
        let body = match (self.serves, self.page()) {
            (Serves::Everyone, Some(page)) => format!(
                "Anyone with the link can read it at {page}. The bytes on their \
                 own — for a machine, a citation, or a projector — are at {bare}."
            ),
            (Serves::Everyone, None) => {
                format!("Anyone with the link can read it at {bare}.")
            }
            (Serves::OwnerOnly, Some(page)) => format!(
                "It is private, so only you can open it: {page}, signed in at the \
                 gate. {bare} serves nobody until it is released."
            ),
            (Serves::OwnerOnly, None) => format!(
                "It is private, so {bare} serves nobody until it is released. \
                 Until then it is yours alone, from your account page at the gate."
            ),
            (Serves::Nothing, Some(page)) => format!(
                "Its share URL now resolves to nothing. Every version is still on \
                 the box, and {page} still opens for you."
            ),
            (Serves::Nothing, None) => format!(
                "Its share URL now resolves to nothing, so {bare} serves nobody. \
                 Every version is still on the box."
            ),
            // The alias moved but nobody asked who may read it, so the box
            // holds that answer and this side was not told it. The bare URL
            // stays unnamed for the same reason: whether it serves anyone is
            // exactly the thing not established.
            (Serves::AsBefore, Some(page)) => format!(
                "Its share URL now names this version, and the page is {page}. \
                 Who may read it is whatever it already was — this did not \
                 change it."
            ),
            (Serves::AsBefore, None) => "Its share URL now names this version. Who may read it \
                 is whatever it already was — this did not change it."
                .to_string(),
            // The bare URL is deliberately absent from both: this arm is
            // reached when the alias was never moved, which for an agent's
            // first publish means it names no version and that URL 404s.
            (Serves::Unchanged, Some(page)) => format!(
                "The page is {page}. Who may read it is whatever the alias \
                 already said — this did not change it."
            ),
            (Serves::Unchanged, None) => "Who may read it is whatever the alias already said — \
                 this did not change it."
                .to_string(),
        };
        format!("{body}{}", self.note)
    }

    /// The publisher's two-column spelling, aligned with the `reach` line
    /// printed under it.
    pub fn columns(&self) -> String {
        let mut out = String::new();
        if let Some(viewer) = &self.viewer {
            out.push_str(&format!("  page   {viewer}\n"));
        }
        out.push_str(&format!("  bytes  {}", self.bare));
        if !self.note.is_empty() {
            out.push_str(&self.note);
        }
        out
    }
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

    /// Which box this machine is pointed at, with no credential involved.
    ///
    /// Separate from [`Remote::configured_for`] because the messages that say
    /// *where to go instead* need the gate precisely when a key is missing —
    /// and looking it up through a constructor that reads a key file would
    /// fail on exactly the machines those messages exist for.
    pub fn gate_configured() -> Result<Option<String>> {
        let gate = match std::env::var("FACTORY_GATE") {
            Ok(gate) if !gate.is_empty() => gate,
            _ => {
                let path = Self::dir()?.join("config.toml");
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
        Ok(Some(gate))
    }

    /// The configured remote presenting one particular credential.
    pub fn configured_for(scope: Scope) -> Result<Option<Remote>> {
        let dir = Self::dir()?;
        let Some(gate) = Self::gate_configured()? else {
            return Ok(None);
        };

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
            viewer_url: body["viewer_url"].as_str().map(String::from),
        })
    }

    /// Move the share URL on the box, and optionally set who may read it.
    ///
    /// **`None` omits the field, which is the box's spelling of "leave who
    /// may read it exactly as it was".** The server has always worked that
    /// way — its own comment calls moving the alias and changing visibility
    /// "separate acts, and neither does the other by accident" — and this
    /// client defeated it by always sending a value, derived from the *local*
    /// store. The local store learns nothing when a bundle is released from
    /// the account page in a browser, so a later push read a stale `private`
    /// and quietly took a public bundle down. Only a caller who was actually
    /// told a visibility may assert one.
    ///
    /// Answers with the bare share URL and the viewer page, in that order —
    /// the second is `None` against a box older than the field.
    pub fn alias(
        &self,
        id: &str,
        version: Option<u32>,
        visibility: Option<Visibility>,
    ) -> Result<(String, Option<String>)> {
        let mut payload = serde_json::json!({ "version": version });
        if let Some(visibility) = visibility {
            payload["visibility"] = serde_json::json!(match visibility {
                Visibility::Public => "public",
                Visibility::Private => "private",
            });
        }
        let body = self
            .request(
                "POST",
                &format!("/v1/bundles/{id}/alias"),
                Some(payload.to_string().as_bytes()),
            )
            .with_context(|| format!("aliasing {id}"))?;
        Ok((
            body["url"].as_str().unwrap_or_default().to_string(),
            body["viewer_url"].as_str().map(String::from),
        ))
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

    /// Send a profile or a board as its own source text.
    ///
    /// TOML on the wire for the same reason a request type is: the box parses
    /// it with the `mecha_manifest` code this side used to check it, so one
    /// text is what both validators see. It also means the box can store the
    /// file byte for byte when nothing needed merging, which is what keeps a
    /// person's comments alive across a round trip.
    pub fn record_push(&self, path: &str, source: &str) -> Result<serde_json::Value> {
        self.send(
            "PUT",
            path,
            Some(source.as_bytes()),
            "text/plain; charset=utf-8",
        )
        .with_context(|| format!("uploading {path}"))
    }

    /// Read a record back — the box's copy, which may carry edits made in a
    /// browser that this machine has never seen.
    pub fn record_get(&self, path: &str) -> Result<serde_json::Value> {
        self.request("GET", path, None)
    }

    /// What boards exist remotely. A machine cannot pull a file it has never
    /// heard of, which is every board created in the cockpit.
    pub fn board_list(&self) -> Result<serde_json::Value> {
        self.request("GET", "/v1/boards", None)
    }

    /// Create a poll on the box; the reply carries the capability URLs.
    pub fn poll_create(
        &self,
        instrument_id: &str,
        poll_id: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.send(
            "PUT",
            &format!("/v1/instruments/{instrument_id}/polls/{poll_id}"),
            Some(payload.to_string().as_bytes()),
            "application/json",
        )
        .with_context(|| format!("creating poll `{poll_id}`"))
    }

    /// The tally, typed: names, tri-state answers, state.
    pub fn poll_status(&self, instrument_id: &str, poll_id: &str) -> Result<serde_json::Value> {
        self.request(
            "GET",
            &format!("/v1/instruments/{instrument_id}/polls/{poll_id}"),
            None,
        )
        .with_context(|| format!("reading poll `{poll_id}`"))
    }

    pub fn poll_close(
        &self,
        instrument_id: &str,
        poll_id: &str,
        resolution: Option<&str>,
    ) -> Result<serde_json::Value> {
        let body = serde_json::json!({ "resolution": resolution }).to_string();
        self.send(
            "POST",
            &format!("/v1/instruments/{instrument_id}/polls/{poll_id}/close"),
            Some(body.as_bytes()),
            "application/json",
        )
        .with_context(|| format!("closing poll `{poll_id}`"))
    }

    /// Replace one instrument's slot cache on the box.
    pub fn slots_push(
        &self,
        instrument_id: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.send(
            "PUT",
            &format!("/v1/instruments/{instrument_id}/slots"),
            Some(payload.to_string().as_bytes()),
            "application/json",
        )
        .with_context(|| format!("pushing slots for `{instrument_id}`"))
    }

    /// Take everything verified and not yet acknowledged, from `since` on.
    ///
    /// A **pure read**: the box marks nothing, so a response that never
    /// arrives costs a repeat rather than a stranger's request. Which is the
    /// right way round — see [`Remote::ack`].
    /// `wait` holds the request open on the box for up to that many seconds,
    /// answering the moment a record lands — the drain loop's whole trick.
    /// Zero asks and answers immediately, which is every scheduled caller.
    pub fn drain(&self, since: i64, wait: u64) -> Result<serde_json::Value> {
        self.request("GET", &format!("/v1/queue?since={since}&wait={wait}"), None)
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

/// The command that publishes a version, as a person would type it.
///
/// One function because the string is *instructions* — the only way a bundle
/// staged by a publish-key-only machine ever reaches a reader — and it was
/// wrong: it spelled the version `--version {v}` where `alias` takes it
/// positionally, so clap refused the one command the note existed to give.
/// A string nobody can run is worse than no string, because it reads as a
/// route out and is not one. `main.rs`'s test module parses what this returns
/// through the real `Cli`, which is the check the prose could not have.
pub fn release_command(id: &str, version: u32) -> String {
    format!("factory-publish alias {id} {version} --visibility public")
}

/// Both doors onto releasing, with the one a tenant actually has first.
///
/// The note this feeds used to end "from a machine that has one" — a route
/// that does not exist for anybody who arrived through self-serve. `connect`
/// deliberately never installs a release key and only the operator can mint
/// one on the box, so "find a machine with a release key" is an instruction
/// whose completion requires SSH. The door that *is* theirs is the account
/// page, and it drives the same `alias_set` the release key drives — naming
/// it is not the lesser answer, it is the other front door onto the identical
/// act. The CLI form stays, second, for the machine that does hold one.
pub fn release_doors(gate: &str, id: &str, version: u32) -> String {
    format!(
        "Release it from your account page at {gate}/account, which is where release \
         authority lives for a paired machine — press \u{201c}Release v{version} \
         publicly\u{201d}. A machine holding a release key can instead run `{}`; \
         pairing never installs one.",
        release_command(id, version)
    )
}

/// Push a version and set the alias on the box, when there is one.
///
/// `Ok(None)` means no remote is configured, which is an ordinary state and not
/// a failure: publishing is a local act that works on a laptop with no server.
/// Every caller — the CLI, the MCP tool — goes through this, so "does the world
/// see it?" has one answer rather than three.
/// `visibility` is what the caller was *told*, never what it inferred:
/// `None` moves the alias and leaves who may read it alone. See
/// [`Remote::alias`] for the bug that rule exists to prevent.
pub fn mirror(
    store: &BundleStore,
    id: &str,
    version: u32,
    alias_to: Option<u32>,
    visibility: Option<Visibility>,
) -> Result<Option<Mirrored>> {
    let Some(remote) = Remote::configured()? else {
        return Ok(None);
    };
    let pushed = remote.push(store, id, version)?;
    let mut note = String::new();
    if pushed.existing {
        note.push_str("\nIdentical bytes, so the box already had them and minted nothing.");
    }
    // The push answers with the viewer page for the version just written; a
    // successful alias answers with the same page, and either is the same
    // string. Preferring the alias's keeps one rule — the last word about
    // where a bundle is comes from the call that last moved it.
    let mut viewer = pushed.viewer_url.clone();
    let mut serves = Serves::Unchanged;

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
                let (_, aliased) = releaser.alias(id, Some(target), visibility)?;
                viewer = aliased.or(viewer);
                serves = match visibility {
                    Some(Visibility::Public) => Serves::Everyone,
                    Some(Visibility::Private) => Serves::OwnerOnly,
                    // The alias moved; who reads it is the box's answer and
                    // nobody here asked to change it.
                    None => Serves::AsBefore,
                };
            }
            None => {
                // `Unchanged` stands, and that is the point: this side asked
                // for a visibility and did not get to apply it, so reporting
                // the one it *wanted* would describe a box that never
                // happened.
                note.push_str(&format!(
                    "\nversion {target} is on the box and is not published: no release \
                     key here, so the alias did not move and the share URL still serves \
                     nobody. {}",
                    release_doors(remote.gate(), id, target)
                ));
            }
        }
    }
    Ok(Some(Mirrored {
        viewer,
        bare: pushed.url,
        serves,
        note,
    }))
}

/// What did *not* reach the box, when an alias move stopped at this machine.
///
/// [`mirror_alias`] answers `None` for two different reasons and neither is
/// visible in the value: no box is configured at all, or this machine holds
/// no release key. Both mean **the box's share URL is exactly as it was** —
/// and both callers printed "the share URL now resolves to v3" or "no longer
/// resolves" over the top of that, describing a local file to somebody asking
/// about the internet. An `unpublish` that reports a takedown while the origin
/// keeps serving the bytes is the worse half: withdrawing something is the one
/// act where believing it worked has consequences.
///
/// `None` here means a release key is installed and the box really was told.
pub fn alias_stopped_here(id: &str, version: Option<u32>) -> Result<Option<String>> {
    if Remote::installed(Scope::Release)?.is_some() {
        return Ok(None);
    }
    let Some(gate) = Remote::gate_configured()? else {
        return Ok(Some(
            "No factory is configured, so this changed nothing beyond this machine's \
             store — there is no share URL anywhere else to change."
                .to_string(),
        ));
    };
    Ok(Some(match version {
        Some(version) => format!(
            "The box was not told: no release key here, so its share URL is exactly as it \
             was. {}",
            release_doors(&gate, id, version)
        ),
        None => format!(
            "The box was not told: no release key here, so anything already released is \
             still being served. Take it down from your account page at {gate}/account."
        ),
    }))
}

/// Move the alias on the box without pushing anything, when there is one.
///
/// Release-scoped: this verb *is* the publication, so it is the one an agent's
/// key deliberately cannot perform.
pub fn mirror_alias(
    id: &str,
    version: Option<u32>,
    visibility: Option<Visibility>,
) -> Result<Option<Mirrored>> {
    let Some(remote) = Remote::installed(Scope::Release)? else {
        return Ok(None);
    };
    let (bare, viewer) = remote.alias(id, version, visibility)?;
    // A `None` version is a takedown, and that is neither of the two
    // visibilities: the alias keeps whatever it said about *who* while
    // pointing at nothing at all.
    let serves = match version {
        None => Serves::Nothing,
        Some(_) => match visibility {
            Some(Visibility::Public) => Serves::Everyone,
            Some(Visibility::Private) => Serves::OwnerOnly,
            None => Serves::AsBefore,
        },
    };
    Ok(Some(Mirrored {
        viewer,
        bare,
        serves,
        note: String::new(),
    }))
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
    use super::connect_tests::one_shot_gate;
    use super::*;
    use mecha_manifest::ContentClass;

    const PAGE: &str = "https://gate.example.org/view/alice/brief";
    const BYTES: &str = "https://alice.art.example.org/b/brief/";

    fn reach(serves: Serves, viewer: Option<&str>, note: &str) -> Mirrored {
        Mirrored {
            viewer: viewer.map(String::from),
            bare: BYTES.into(),
            serves,
            note: note.into(),
        }
    }

    /// The sentence an agent repeats has to be true about a **private**
    /// bundle, which is the state a publish key leaves behind.
    ///
    /// This is the one that shipped wrong: `mirror` reported the bare
    /// artifact URL whatever the visibility, and the tool answer wrapped it
    /// in "It is live at …" — so a model told whoever asked to open a URL
    /// that answers 404 by design.
    #[test]
    fn a_private_bundle_is_never_described_as_live() {
        let said = reach(Serves::OwnerOnly, Some(PAGE), "").sentence();
        assert!(said.contains(PAGE), "the page it does open: {said}");
        assert!(said.contains("private"), "{said}");
        assert!(!said.contains("live at"), "the old lie: {said}");
        assert!(!said.contains("Anyone"), "{said}");
        assert!(
            said.contains("serves nobody until it is released"),
            "the bare URL has to be named as what it is: {said}"
        );
    }

    /// Every arm names the page before the bytes, because the page is the
    /// URL a person is meant to receive and a model quotes what it reads
    /// first.
    #[test]
    fn the_page_comes_before_the_bytes_in_every_arm() {
        for serves in [
            Serves::Everyone,
            Serves::OwnerOnly,
            Serves::Nothing,
            Serves::Unchanged,
        ] {
            let said = reach(serves, Some(PAGE), "").sentence();
            let page = said
                .find(PAGE)
                .unwrap_or_else(|| panic!("{serves:?} names no page: {said}"));
            if let Some(bytes) = said.find(BYTES) {
                assert!(page < bytes, "{serves:?} leads with the bytes: {said}");
            }
        }
    }

    /// An alias this side could not move must claim nothing about who may
    /// read the result — reporting the visibility we *asked* for would
    /// describe a box that never happened — and the note saying so has to
    /// survive into the sentence rather than being dropped on the floor.
    #[test]
    fn an_unmoved_alias_claims_nothing_and_keeps_its_note() {
        let said = reach(
            Serves::Unchanged,
            Some(PAGE),
            "\nversion 3 is on the box and is not published: no release key here.",
        )
        .sentence();
        assert!(!said.contains("Anyone"), "{said}");
        assert!(!said.contains("It is private"), "{said}");
        assert!(said.contains("this did not change it"), "{said}");
        assert!(
            said.contains("no release key here"),
            "the note rode along: {said}"
        );
    }

    /// With no page, no arm may offer the bytes URL as one that opens.
    ///
    /// This replaces a test that asserted the opposite and passed: it checked
    /// that a missing page "falls back to the bytes", on `Serves::Everyone`
    /// alone — the single arm where that substitution happens to be true. In
    /// the other three the same fallback made `page` and `bare` one string,
    /// so the private arm read "only you can open it: <URL>. <the same URL>
    /// serves nobody", about an origin that holds no session and answers 404.
    /// That is the "it is live at" lie this whole type exists to remove,
    /// reinstated in the arm today's deployed box exercises.
    #[test]
    fn with_no_page_no_arm_offers_the_bytes_as_one_that_opens() {
        // Phrases that promise the URL beside them can be opened. If a new
        // arm is written with one of these and no page, this fails.
        const PROMISES: [&str; 4] = [
            "only you can open it:",
            "still opens",
            "The page is",
            "read it at",
        ];

        for serves in [
            Serves::Everyone,
            Serves::OwnerOnly,
            Serves::Nothing,
            Serves::Unchanged,
        ] {
            let said = reach(serves, None, "").sentence();
            for promise in PROMISES {
                if let Some(at) = said.find(promise) {
                    // `Everyone` is the one state where the bare URL *is*
                    // openable, so it alone may be promised.
                    assert_eq!(
                        serves,
                        Serves::Everyone,
                        "{serves:?} promises \"{promise}\" with no page, and the only \
                         URL it can mean is one that serves nobody: {said}"
                    );
                    let _ = at;
                }
            }
        }
    }

    /// The columns degrade to the bytes line alone rather than printing an
    /// empty `page` row, which would read as a broken link rather than an old
    /// server.
    #[test]
    fn a_box_that_names_no_page_prints_no_page_line() {
        let at = reach(Serves::Everyone, None, "");
        assert_eq!(at.page(), None);
        let columns = at.columns();
        assert!(!columns.contains("page"), "no empty page line: {columns:?}");
        assert!(columns.contains(&format!("bytes  {BYTES}")), "{columns:?}");
    }

    /// An unasked-for visibility must never reach the box, because the box
    /// treats a value as an instruction.
    ///
    /// This is the whole of the silent-unpublish bug: `push` has no
    /// `--visibility`, read one out of the *local* store, and sent it. The
    /// local store never hears about a release made from the account page in
    /// a browser, so it kept saying `private` and the next push handed that
    /// to the box as a decision — taking a public bundle down with no prompt
    /// and no output saying so. Asserted on the wire body, because that is
    /// where the difference between "leave it" and "make it private" lives.
    #[test]
    fn a_visibility_nobody_asked_for_is_not_sent() {
        let answer = serde_json::json!({
            "id": "brief", "version": 3, "visibility": "public",
            "url": BYTES, "viewer_url": PAGE,
        });
        // What `alias` actually put on the wire, not a re-derivation of it:
        // the difference between "leave it" and "make it private" is a field
        // in a request body, so that is what has to be looked at.
        let sent = |visibility: Option<Visibility>| {
            let (gate, body) = capturing_gate(answer.clone());
            let remote = Remote {
                gate,
                key: "mk_rel_test".into(),
                scope: Scope::Release,
            };
            remote.alias("brief", Some(3), visibility).unwrap();
            serde_json::from_str::<serde_json::Value>(&body.recv().unwrap()).unwrap()
        };

        // Nobody asked: the field is absent, which is how the box is told to
        // keep whatever it already decided.
        let quiet = sent(None);
        assert_eq!(quiet["version"], 3);
        assert!(
            quiet.get("visibility").is_none(),
            "an unasked visibility went on the wire: {quiet}"
        );
        // Somebody asked: it travels, because that is a decision.
        assert_eq!(sent(Some(Visibility::Private))["visibility"], "private");
        assert_eq!(sent(Some(Visibility::Public))["visibility"], "public");
    }

    /// Moving the alias without being told a visibility is its own state, and
    /// it must not borrow either visibility's words.
    ///
    /// `Unchanged` would be wrong too — there the alias never moved — so a
    /// report that collapsed them would tell somebody their share URL had not
    /// moved when it had.
    #[test]
    fn moving_an_alias_without_a_visibility_claims_no_reader() {
        let said = reach(Serves::AsBefore, Some(PAGE), "").sentence();
        assert!(said.contains("now names this version"), "{said}");
        assert!(said.contains("whatever it already was"), "{said}");
        assert!(!said.contains("Anyone"), "{said}");
        assert!(!said.contains("private"), "{said}");
        assert!(
            !said.contains(BYTES),
            "unestablished reach was named: {said}"
        );
    }

    /// The one instruction that gets a publish-key-only bundle released has
    /// to be a command that runs. `main.rs`'s test module parses it through
    /// the real `Cli`; this pins the shape that clap rejected — the version
    /// is positional, never `--version`.
    #[test]
    fn the_release_command_puts_the_version_where_alias_wants_it() {
        let command = release_command("brief", 3);
        assert!(command.contains(" brief 3"), "{command}");
        assert!(!command.contains("--version"), "clap rejects it: {command}");
    }

    /// The advice a publish-key-only machine gets has to name a door that
    /// person can open.
    ///
    /// It used to say "from a machine that has one", and for everybody who
    /// arrived through self-serve there is no such machine and no way to make
    /// one: `connect` never installs a release key and minting one is an SSH
    /// session on the box. The account page drives the same `alias_set`, so
    /// it is not a workaround — it is the door.
    #[test]
    fn the_release_advice_names_the_account_page_first() {
        let said = release_doors("https://gate.example.org", "brief", 3);
        let Some(account) = said.find("https://gate.example.org/account") else {
            panic!("no account page named: {said}");
        };
        assert!(
            said.find("factory-publish alias").is_none_or(|cli| cli > account),
            "the door that needs SSH came first: {said}"
        );
        assert!(
            !said.contains("a machine that has one"),
            "the dead end survived: {said}"
        );
    }

    /// A gate is readable with no key on the machine at all.
    ///
    /// The messages that say *where to go instead* need it exactly when a key
    /// is missing, so a lookup that read one would fail on the machines those
    /// messages exist for.
    #[test]
    fn the_gate_is_readable_without_any_key() {
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
        let gate = Remote::gate_configured();
        let stopped = alias_stopped_here("brief", Some(3));
        std::env::remove_var("MECHA_HOME");
        assert_eq!(gate.unwrap().as_deref(), Some("https://gate.example.org"));
        let stopped = stopped.unwrap().expect("no release key here, so: a note");
        assert!(stopped.contains("/account"), "{stopped}");
    }

    /// With no box at all, an alias move is a local file edit, and saying
    /// anything about a share URL describes somewhere this machine has never
    /// spoken to.
    #[test]
    fn an_alias_with_no_box_says_nothing_left_the_machine() {
        let _env = crate::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("MECHA_HOME", home.path());
        let stopped = alias_stopped_here("brief", None);
        std::env::remove_var("MECHA_HOME");
        let stopped = stopped.unwrap().expect("no box, so: a note");
        assert!(stopped.contains("nothing beyond this machine"), "{stopped}");
    }

    /// A one-shot gate that hands back the request body it was sent.
    ///
    /// `one_shot_gate` reads the request and discards it, which is right for
    /// the tests that only care about the answer. Asserting what this client
    /// *asks for* needs the body kept.
    fn capturing_gate(answer: serde_json::Value) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            // Read to the end of the declared body, like `one_shot_gate` —
            // answering after one read races the client's write.
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
                        let _ =
                            tx.send(String::from_utf8_lossy(&request[split + 4..]).into_owned());
                        break;
                    }
                }
            }
            let text = answer.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        });
        (format!("http://{address}"), rx)
    }

    /// A store with one pushable version in it, rooted in a tempdir.
    fn store_with_a_version(dir: &Path) -> BundleStore {
        let store = BundleStore::open(dir).unwrap();
        let version = store.version_dir("brief", 1);
        std::fs::create_dir_all(&version).unwrap();
        std::fs::write(version.join("index.html"), "<h1>Monday</h1>").unwrap();
        store
    }

    /// The page the box named survives the trip into `Pushed`, and a box that
    /// names none answers `None` rather than an empty string.
    ///
    /// Both halves matter and only one of them can be checked against the
    /// live box: the deployed origin predates the field, so the fallback is
    /// the arm real traffic exercises today and the other is the arm every
    /// deploy from here on will.
    #[test]
    fn a_push_carries_the_viewer_page_when_the_box_names_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_a_version(dir.path());

        let mut answer = serde_json::json!({
            "id": "brief",
            "version": 1,
            "existing": false,
            "class": "static",
            "url": BYTES,
            "version_url": "https://alice.art.example.org/b/brief/v/1/",
            "viewer_url": PAGE,
        });
        let remote = Remote {
            gate: one_shot_gate(201, answer.clone()),
            key: "mk_pub_test".into(),
            scope: Scope::Publish,
        };
        let pushed = remote.push(&store, "brief", 1).unwrap();
        assert_eq!(pushed.viewer_url.as_deref(), Some(PAGE));
        assert_eq!(pushed.url, BYTES);

        // The same exchange against a box too old to know the field.
        answer.as_object_mut().unwrap().remove("viewer_url");
        let older = Remote {
            gate: one_shot_gate(201, answer),
            key: "mk_pub_test".into(),
            scope: Scope::Publish,
        };
        let pushed = older.push(&store, "brief", 1).unwrap();
        assert_eq!(
            pushed.viewer_url, None,
            "a missing page must not arrive as an empty URL"
        );
    }

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
    /// `pub(super)` so the `tests` module beside this one can drive a push
    /// against a scripted answer — one fake gate, not two.
    pub(super) fn one_shot_gate(status: u16, body: serde_json::Value) -> String {
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
