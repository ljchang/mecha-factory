//! The MCP surface: how mecha — or any MCP-speaking agent — reaches the
//! factory.
//!
//! **Why MCP rather than a native tool in mecha**, restated because it is the
//! decision the whole boundary rests on: mecha's outbox routes by tool *name*
//! in the dispatch path, so naming `factory__bundle_publish` in
//! `[outbox] publish_tools` gives "an agent drafts a page, a human releases it"
//! with zero changes to mecha-core. The agent loop never learns that a public
//! surface exists, which is that project's founding invariant. And anything
//! that speaks MCP can publish here without knowing mecha exists at all.
//!
//! ### The annotations, and the one thing they cannot express
//!
//! mecha derives capabilities from two hints: `readOnlyHint`, and
//! `openWorldHint` — which sets **both** `untrusted_input` and `external_send`,
//! because a tool that talks to the wider world is both a source of
//! attacker-influenced content and a way for data to leave. They cannot be set
//! independently from here.
//!
//! That matters for one row of the design's table. `bundle_list` and
//! `bundle_status` are meant to be `untrusted_input` without being
//! `external_send`: the query goes only to our own origin, but that origin is a
//! box we have agreed to assume is lost, so what comes back is third-party
//! text. **Today they read the local store and there is no origin at all**, so
//! `private_data` alone is honest and `openWorldHint` would be a lie in the
//! restrictive direction. When the server exists, the mechanism is the one the
//! design already names: `[[mcp]] capabilities` overrides in mecha's config,
//! which only ever widen. That is a real to-do, not a subtlety — a read that
//! stops being local without gaining the marking is exactly the silent
//! degradation this project keeps naming.
//!
//! Where over-claiming is the safe direction, we over-claim. `bundle_publish`,
//! `bundle_alias` and `bundle_unpublish` carry `openWorldHint` even though a
//! publish is still local, because they are what a share URL resolves through
//! and they must route through the outbox from the first day rather than from
//! the day a VPS appears.
//!
//! ### Paths the model supplies are confined here, not only by the operator
//!
//! `bundle_render` and `bundle_fetch` take an output directory, and
//! `bundle_publish` takes one to read. Those are **paths chosen by a model**,
//! and mecha's path jail does not reach them: `ToolCtx::resolve` guards mecha's
//! own tools, while an MCP server's arguments are its own business. Measured —
//! `bundle_fetch` with an `out` anywhere on the filesystem wrote there.
//!
//! The design's answer is `sandbox = true` on the `[[mcp]]` block, and that is
//! the real enforcement. It is not sufficient on its own: mecha only sets the
//! server's working directory *when* it confines it, so an unconfined server
//! inherits mecha's, and an operator who forgets the flag gets no boundary at
//! all. A guard that depends on remembering a config line is the
//! silently-degrading-sandbox shape this project keeps naming.
//!
//! So every model-supplied path is resolved and proved to be inside `--root`
//! before anything touches it — the same rule, and the same containment proof,
//! that mecha applies to its own tools. `--root` defaults to the working
//! directory, which is the workspace when mecha confines the server, and it is
//! printed on stderr at startup so an operator can see what it actually is.
//!
//! ### The transport
//!
//! Newline-delimited JSON-RPC on stdin/stdout, hand-rolled — the surface is
//! `initialize`, `tools/list` and `tools/call`, and a dependency for three
//! methods would be more code to audit than the three methods. **Nothing is
//! ever written to stdout except a response**: stdout is the protocol, and a
//! stray `println!` corrupts the stream in a way that reads as the server
//! having crashed. Diagnostics go to stderr.

use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// One tool, with the annotations that decide what mecha lets it do.
struct ToolSpec {
    name: &'static str,
    description: &'static str,
    read_only: bool,
    open_world: bool,
    schema: fn() -> Value,
}

fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "bundle_render",
            description:
                "Render a markdown file into a publishable bundle directory. Cheap and local: \
                 nothing leaves the machine and no review is needed. Render, read the output \
                 back, fix it, and publish once.",
            read_only: false,
            open_world: false,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string", "description": "Path to the markdown file."},
                        "out": {"type": "string", "description": "Directory to write the bundle into."},
                        "title": {"type": "string", "description": "Overrides the first `# heading`."}
                    },
                    "required": ["source", "out"],
                    "additionalProperties": false
                })
            },
        },
        ToolSpec {
            name: "bundle_publish",
            description:
                "Publish an already-rendered bundle directory as a new immutable version and \
                 point its share URL at it. Publishing identical bytes returns the existing \
                 version rather than making a new one.",
            read_only: false,
            open_world: true,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Stable bundle id; the share URL is built on it."},
                        "bundle": {"type": "string", "description": "A directory bundle_render produced."},
                        "title": {"type": "string"},
                        "description": {"type": "string"},
                        "sources": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "What this was rendered from. Recorded so retention never removes a published report's input."
                        },
                        "visibility": {
                            "type": "string",
                            "enum": ["public", "private"],
                            "description": "Who may read it on the origin. Omitted keeps what this bundle already was; a bundle that has never been anything is private, and the origin serves a private bundle to nobody."
                        }
                    },
                    "required": ["id", "bundle"],
                    "additionalProperties": false
                })
            },
        },
        ToolSpec {
            name: "bundle_alias",
            description:
                "Point a bundle's share URL at a specific version. This changes what every \
                 existing share link resolves to, which is a publication rather than \
                 bookkeeping.",
            read_only: false,
            open_world: true,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "version": {"type": "integer", "minimum": 1},
                        "visibility": {
                            "type": "string",
                            "enum": ["public", "private"],
                            "description": "Who may read it on the origin. Omitted keeps what this bundle already was; a bundle that has never been anything is private, and the origin serves a private bundle to nobody."
                        }
                    },
                    "required": ["id", "version"],
                    "additionalProperties": false
                })
            },
        },
        ToolSpec {
            name: "bundle_unpublish",
            description:
                "Take a bundle down: its share URL stops resolving. Destroys nothing — every \
                 version stays on disk and can be aliased again.",
            read_only: false,
            open_world: true,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {"id": {"type": "string"}},
                    "required": ["id"],
                    "additionalProperties": false
                })
            },
        },
        ToolSpec {
            name: "bundle_fetch",
            description:
                "Copy a published bundle out of the local mirror into a directory, by id. Use \
                 this to read back what an earlier run published.",
            read_only: true,
            open_world: false,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "out": {"type": "string"},
                        "version": {"type": "integer", "minimum": 1, "description": "Defaults to whatever the share URL points at."}
                    },
                    "required": ["id", "out"],
                    "additionalProperties": false
                })
            },
        },
        ToolSpec {
            name: "bundle_list",
            description: "Every published bundle, how many versions each has, and which \
                          version its share URL currently resolves to.",
            read_only: true,
            open_world: false,
            schema: || json!({"type": "object", "properties": {}, "additionalProperties": false}),
        },
        ToolSpec {
            name: "bundle_status",
            description: "One bundle: its versions, what its share URL points at, and what each \
                          version was rendered from.",
            read_only: true,
            open_world: false,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {"id": {"type": "string"}},
                    "required": ["id"],
                    "additionalProperties": false
                })
            },
        },
    ]
}

/// Resolve a model-supplied path and prove it is inside `root`.
///
/// The target usually does not exist yet, so containment is proved on the
/// nearest existing ancestor and the remainder is checked for traversal — the
/// same shape as canonicalize-and-compare, minus the requirement that the leaf
/// already be there.
/// `visibility` off a tool call, or nothing.
///
/// An unrecognised value is an error rather than a fallback to private: a model
/// that wrote `"visible"` meant something, and quietly doing the safe thing
/// would leave it believing it had published to the world.
fn visibility_arg(args: &Value) -> Result<Option<mecha_manifest::Visibility>> {
    match args.get("visibility").and_then(Value::as_str) {
        None => Ok(None),
        Some("public") => Ok(Some(mecha_manifest::Visibility::Public)),
        Some("private") => Ok(Some(mecha_manifest::Visibility::Private)),
        Some(other) => anyhow::bail!("visibility `{other}` is not `public` or `private`"),
    }
}

fn confined(root: &Path, supplied: &str) -> Result<PathBuf> {
    let candidate = {
        let p = PathBuf::from(supplied);
        if p.is_absolute() {
            p
        } else {
            root.join(p)
        }
    };
    // Walk the path, refusing `..` outright rather than normalising it away.
    let mut built = PathBuf::new();
    for part in candidate.components() {
        use std::path::Component;
        match part {
            Component::ParentDir => {
                anyhow::bail!("`{supplied}` contains `..`, which is refused rather than resolved")
            }
            Component::CurDir => {}
            other => built.push(other.as_os_str()),
        }
    }
    // Prove containment on the deepest part that exists.
    let mut existing = built.clone();
    while !existing.exists() {
        match existing.parent() {
            Some(parent) if parent != existing => existing = parent.to_path_buf(),
            _ => break,
        }
    }
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let real = existing.canonicalize().unwrap_or(existing);
    anyhow::ensure!(
        real.starts_with(&real_root),
        "`{supplied}` resolves outside {} — this server only reads and writes \
         inside the directory it was given",
        real_root.display()
    );
    Ok(built)
}

/// Serve MCP on stdin/stdout until the client closes the stream.
pub fn serve(store_root: Option<PathBuf>, root: Option<PathBuf>) -> Result<()> {
    let root = match root {
        Some(root) => root,
        None => std::env::current_dir()?,
    };
    // stderr, never stdout: stdout is the protocol.
    eprintln!("mecha-factory: paths are confined to {}", root.display());
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("factory-mcp: unparseable request: {e}");
                continue;
            }
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));

        // A notification has no id and takes no response. Replying to one is a
        // protocol error that some clients treat as a fatal desync.
        if id.is_none() {
            continue;
        }

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mecha-factory", "version": env!("CARGO_PKG_VERSION")},
            })),
            "tools/list" => Ok(json!({
                "tools": tools().iter().map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": (t.schema)(),
                    "annotations": {
                        "readOnlyHint": t.read_only,
                        "openWorldHint": t.open_world,
                    },
                })).collect::<Vec<_>>()
            })),
            "tools/call" => Ok(call(&params, store_root.clone(), &root)),
            "ping" => Ok(json!({})),
            other => Err(format!("unknown method `{other}`")),
        };

        let response = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(message) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": message}
            }),
        };
        // stdout is the protocol. Nothing else may be written here.
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

/// Dispatch one tool call.
///
/// **Every expected failure comes back as `isError: true` with the message,
/// never as a JSON-RPC error.** A protocol error tells the model its call was
/// malformed; a tool error tells it what went wrong so it can recover — and the
/// most common failure here, an external reference surviving the gate, is
/// precisely one the model *can* fix.
fn call(params: &Value, store_root: Option<PathBuf>, root: &Path) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match dispatch(name, &args, store_root, root) {
        Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
        Err(e) => json!({
            "content": [{"type": "text", "text": format!("{e:#}")}],
            "isError": true
        }),
    }
}

fn dispatch(name: &str, args: &Value, store_root: Option<PathBuf>, root: &Path) -> Result<String> {
    use crate::store::BundleStore;

    let store = || -> Result<BundleStore> {
        match &store_root {
            Some(root) => BundleStore::open(root),
            None => BundleStore::open_default(),
        }
    };
    let string = |key: &str| -> Result<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("`{key}` is required"))
    };
    let now = chrono::Utc::now().to_rfc3339();

    match name {
        "bundle_render" => {
            let source = confined(root, &string("source")?)?;
            let out = confined(root, &string("out")?)?;
            let title = args.get("title").and_then(Value::as_str);
            let rendered = crate::render::report(&source, &out, title)?;
            crate::vendor::gate_rendered(&rendered.dir, &source)?;
            Ok(format!(
                "Rendered `{}` as a {} bundle in {}.\nOpen {} to read it. \
                 Nothing has been published; publish it when it looks right.",
                rendered.title,
                rendered.class.as_str(),
                rendered.dir.display(),
                rendered.dir.join("index.html").display()
            ))
        }
        "bundle_publish" => {
            let id = string("id")?;
            let bundle = confined(root, &string("bundle")?)?;
            let title = args
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            let mut sources = Vec::new();
            for source in args
                .get("sources")
                .and_then(Value::as_array)
                .unwrap_or(&vec![])
            {
                if let Some(path) = source.as_str() {
                    sources.push(
                        PathBuf::from(path)
                            .canonicalize()
                            .unwrap_or_else(|_| path.into()),
                    );
                }
            }
            crate::vendor::gate(&bundle)?;
            let store = store()?;
            let published = store.publish(
                &id,
                &bundle,
                &title,
                args.get("description")
                    .and_then(Value::as_str)
                    .map(String::from),
                "report",
                mecha_manifest::ContentClass::Static,
                sources,
                &now,
            )?;
            let visibility = visibility_arg(args)?.unwrap_or_else(|| {
                store
                    .alias(&id)
                    .ok()
                    .flatten()
                    .map(|a| a.visibility)
                    .unwrap_or(mecha_manifest::Visibility::Private)
            });
            store.set_alias(&id, Some(published.version), visibility, &now)?;

            // The box, if there is one. A failure here is reported rather than
            // swallowed: the human released this from the outbox expecting it
            // to reach the world, and "published" with nothing on the origin is
            // the one outcome nobody would notice.
            let reach = match crate::remote::mirror(
                &store,
                &id,
                published.version,
                Some(published.version),
                visibility,
            ) {
                Ok(Some(url)) => format!("\nIt is live at {url}."),
                Ok(None) => String::new(),
                Err(e) => {
                    anyhow::bail!(
                        "{id} is published locally as version {} and the alias points at it, \
                         but the origin did not take it: {e:#}\nNothing was lost. Retry with \
                         `factory-publish push {id} --version {}`.",
                        published.version,
                        published.version
                    )
                }
            };

            Ok(format!(
                "{} is published as version {}{}.\nIt is at {}, and its share URL now \
                 resolves to that version.{}",
                id,
                published.version,
                if published.existing {
                    " (identical bytes, so no new version was made)"
                } else {
                    ""
                },
                published.path.display(),
                reach
            ))
        }
        "bundle_alias" => {
            let id = string("id")?;
            let version = args
                .get("version")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("`version` is required"))?
                as u32;
            let store = store()?;
            let visibility = visibility_arg(args)?.unwrap_or_else(|| {
                store
                    .alias(&id)
                    .ok()
                    .flatten()
                    .map(|a| a.visibility)
                    .unwrap_or(mecha_manifest::Visibility::Private)
            });
            store.set_alias(&id, Some(version), visibility, &now)?;
            let reach = match crate::remote::mirror_alias(&id, Some(version), visibility)? {
                Some(url) => format!(" It is live at {url}."),
                None => String::new(),
            };
            Ok(format!(
                "{id}'s share URL now resolves to version {version}.{reach}"
            ))
        }
        "bundle_unpublish" => {
            let id = string("id")?;
            let store = store()?;
            let existing = store.alias(&id)?;
            let before = existing.as_ref().and_then(|a| a.version);
            // Kept rather than flipped to private: see the CLI's `unpublish`.
            // What a reader gets — "this has been taken down" versus "no such
            // thing" — is decided here.
            let visibility = existing
                .map(|a| a.visibility)
                .unwrap_or(mecha_manifest::Visibility::Private);
            store.set_alias(&id, None, visibility, &now)?;
            crate::remote::mirror_alias(&id, None, visibility)?;
            Ok(format!(
                "{id}'s share URL no longer resolves{}. {} version(s) remain on disk — \
                 nothing was deleted, and it can be aliased again.",
                match before {
                    Some(v) => format!(" (it pointed at version {v})"),
                    None => String::new(),
                },
                store.versions(&id)?.len()
            ))
        }
        "bundle_fetch" => {
            let id = string("id")?;
            let out = confined(root, &string("out")?)?;
            let store = store()?;
            let version = match args.get("version").and_then(Value::as_u64) {
                Some(v) => v as u32,
                None => store.alias(&id)?.and_then(|a| a.version).ok_or_else(|| {
                    anyhow::anyhow!("{id} has no aliased version; name one with `version`")
                })?,
            };
            // The caller names an id, never a path: the store resolves it, so
            // nothing from outside is ever joined onto the root.
            let from = store.version_dir(&id, version);
            anyhow::ensure!(from.is_dir(), "{id} has no version {version}");
            copy_dir(&from, &out)?;
            Ok(format!(
                "Copied {id} version {version} into {}. It came from the local mirror, \
                 not from the origin, so it is your own bytes rather than third-party \
                 content.",
                out.display()
            ))
        }
        "bundle_list" => {
            let store = store()?;
            let bundles = store.bundles()?;
            if bundles.is_empty() {
                return Ok("Nothing is published yet.".into());
            }
            let mut out = String::new();
            for id in bundles {
                let versions = store.versions(&id)?;
                let at = match store.alias(&id)?.and_then(|a| a.version) {
                    Some(v) => format!("share URL → version {v}"),
                    None => "taken down".into(),
                };
                out.push_str(&format!(
                    "{id}: {} version(s), latest {}, {at}\n",
                    versions.len(),
                    versions.last().copied().unwrap_or(0)
                ));
            }
            Ok(out)
        }
        "bundle_status" => {
            let id = string("id")?;
            let store = store()?;
            let versions = store.versions(&id)?;
            anyhow::ensure!(!versions.is_empty(), "there is no bundle `{id}`");
            let alias = store.alias(&id)?.and_then(|a| a.version);
            let mut out = format!("{id}\n");
            for version in versions {
                let m = store.manifest(&id, version)?;
                out.push_str(&format!(
                    "  version {version}{} — {} — {}\n",
                    if alias == Some(version) {
                        " (the share URL points here)"
                    } else {
                        ""
                    },
                    m.published_at.as_deref().unwrap_or("no timestamp"),
                    m.title
                ));
                for source in &m.sources {
                    out.push_str(&format!("    rendered from {}\n", source.display()));
                }
            }
            if alias.is_none() {
                out.push_str("  the share URL does not resolve (taken down)\n");
            }
            Ok(out)
        }
        other => anyhow::bail!("there is no tool called `{other}`"),
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The annotations are the whole security contract with mecha, and they are
    /// derived rather than declared on the far side: `openWorldHint` sets both
    /// `untrusted_input` and `external_send`, and `[outbox] publish_tools`
    /// keys on the name. A row changing here silently changes what a model may
    /// do without a human.
    #[test]
    fn every_tool_that_moves_a_share_url_declares_itself_open_world() {
        let by_name: std::collections::BTreeMap<_, _> =
            tools().into_iter().map(|t| (t.name, t)).collect();

        for name in ["bundle_publish", "bundle_alias", "bundle_unpublish"] {
            let t = &by_name[name];
            assert!(t.open_world, "{name} must route through the outbox");
            assert!(!t.read_only, "{name} changes what a share link resolves to");
        }
        // Cheap and local: rendering must not cost a human review, or every
        // iteration becomes a staged item somebody has to reject.
        assert!(!by_name["bundle_render"].open_world);
        // Reads of our own mirror. Not `untrusted_input` today because there is
        // no origin; when there is one, mecha's capability override is the
        // mechanism — see the module docs.
        for name in ["bundle_fetch", "bundle_list", "bundle_status"] {
            let t = &by_name[name];
            assert!(t.read_only, "{name}");
            assert!(!t.open_world, "{name}");
        }
    }

    #[test]
    fn every_tool_has_a_closed_schema_and_a_description() {
        for tool in tools() {
            let schema = (tool.schema)();
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{} accepts undeclared arguments",
                tool.name
            );
            assert!(
                tool.description.len() > 40,
                "{} needs a description a model can act on",
                tool.name
            );
        }
    }

    /// An expected failure has to come back as a tool error the model can
    /// recover from, not as a protocol error that says its call was malformed.
    #[test]
    fn an_expected_failure_is_a_tool_error_with_the_reason() {
        let result = call(
            &json!({"name": "bundle_status", "arguments": {"id": "nothing-here"}}),
            Some(std::env::temp_dir().join("factory-mcp-empty")),
            &std::env::temp_dir(),
        );
        assert_eq!(result["isError"], json!(true));
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no bundle"),
            "{result}"
        );
    }

    /// Measured before this existed: `bundle_fetch` with an `out` anywhere on
    /// the filesystem wrote there. mecha's path jail does not reach an MCP
    /// server's arguments, and mecha only sets the server's working directory
    /// when it confines it — so an operator who forgets `sandbox = true` had no
    /// boundary at all.
    #[test]
    fn a_model_supplied_path_cannot_leave_the_root() {
        let root = std::env::temp_dir().join(format!("factory-confine-{}", std::process::id()));
        std::fs::create_dir_all(root.join("inside")).unwrap();

        // Inside, existing or not, absolute or relative.
        for good in ["inside", "inside/new/deeper", "report.md"] {
            let path = confined(&root, good).unwrap();
            assert!(path.starts_with(&root), "{good} → {}", path.display());
        }
        let absolute_inside = root.join("inside").to_string_lossy().into_owned();
        assert!(confined(&root, &absolute_inside).is_ok());

        // Out, every way there is.
        for bad in ["/etc/passwd", "../escaped", "inside/../../up", "./../../up"] {
            let err = confined(&root, bad).unwrap_err().to_string();
            assert!(
                err.contains("refused") || err.contains("resolves outside"),
                "{bad} was allowed: {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `..` is refused rather than normalised away, because a path that
    /// *resolves* back inside after climbing out has still been interpreted,
    /// and interpreting is where these go wrong.
    #[test]
    fn a_traversal_that_lands_back_inside_is_still_refused() {
        let root = std::env::temp_dir().join(format!("factory-confine2-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(confined(&root, "a/../b").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_tool_is_an_error_the_model_can_read() {
        let result = call(
            &json!({"name": "bundle_delete", "arguments": {}}),
            None,
            &std::env::temp_dir(),
        );
        assert_eq!(result["isError"], json!(true));
        // There is deliberately no delete verb; the message has to say so
        // rather than looking like a transient failure.
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no tool called `bundle_delete`"));
    }
}
