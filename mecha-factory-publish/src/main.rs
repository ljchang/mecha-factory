//! `factory-publish` — render a bundle, publish it, move its alias.
//!
//! The CLI exists before the MCP server on purpose: every verb is testable from
//! a shell, and the server is the same library with a different front end. The
//! split between the cheap verb and the expensive one is real here too — `render`
//! writes a directory you look at, and `publish` is the one that costs a review.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use mecha_factory_publish::{notebook, render, store::BundleStore, vendor};
use mecha_manifest::Visibility;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "factory-publish",
    version,
    about = "Render and publish bundles"
)]
struct Cli {
    /// The bundle store. Defaults to `~/.mecha/bundles` (or `$MECHA_HOME`).
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Render a source into a directory, locally. Cheap, and nothing leaves.
    Render {
        /// The markdown to render.
        source: PathBuf,
        /// Where to write the bundle.
        #[arg(long)]
        out: PathBuf,
        /// Overrides the first `# heading`.
        #[arg(long)]
        title: Option<String>,
    },
    /// Take a rendered directory in as a new immutable version, and point the
    /// share URL at it.
    Publish {
        /// The bundle id — stable across versions; the share URL is built on it.
        id: String,
        /// A directory `render` produced.
        bundle: PathBuf,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// What this was rendered from. Recorded so `mecha work clean` never
        /// removes the input of a published report.
        #[arg(long = "source")]
        sources: Vec<PathBuf>,
        /// Publish the version without moving the share URL to it.
        #[arg(long)]
        no_alias: bool,
    },
    /// Render a marimo notebook as a `compute`-class bundle. Executes the
    /// notebook, so it is bounded by a timeout.
    Notebook {
        /// The notebook `.py`.
        source: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        title: Option<String>,
        /// Seconds the export may take. It runs the notebook.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Build it even though the Python runtime is still CDN-loaded. A
        /// diagnostic; the result cannot boot on an origin that enforces the
        /// policy and must never be published.
        #[arg(long)]
        allow_unvendored_runtime: bool,
        /// Fetch and embed Pyodide at this version, from the pinned allowlist.
        /// Without it the bundle keeps marimo's CDN loader and will not boot.
        #[arg(long)]
        vendor_runtime: Option<String>,
    },
    /// Speak MCP on stdin/stdout. This is how mecha reaches the factory:
    /// wire it as an `[[mcp]]` server and name the publishing tools in
    /// `[outbox] publish_tools`.
    Mcp,
    /// Serve a rendered bundle locally with the real headers for its class.
    ///
    /// Loopback only. A bundle checked without its Content-Security-Policy is
    /// checked against something the world never sees.
    Serve {
        bundle: PathBuf,
        /// `static` | `interactive` | `compute`.
        #[arg(long, default_value = "static")]
        class: String,
        /// 0 asks the OS for a free port.
        #[arg(long, default_value_t = 8347)]
        port: u16,
        /// Serve without the policy. A diagnostic for finding out what a
        /// bundle needs — never a verification, since a bundle that works this
        /// way has been told nothing.
        #[arg(long)]
        no_csp: bool,
    },
    /// Run the external-reference gate over a rendered directory without
    /// publishing. The same check `publish` runs, so a bundle that passes here
    /// is one that will publish.
    Check {
        bundle: PathBuf,
        /// A third-party subtree reviewed as a unit, as `path=sha256:…`.
        /// Repeatable. Its contents are not scanned line by line; its digest
        /// is checked instead, so a tree that changed since it was reviewed is
        /// a finding rather than a pass.
        ///
        /// With no `=digest`, the tree is still scanned strictly and the digest
        /// it *would* pin at is printed — reviewing it is what turns that into
        /// a declaration, and having the number to hand is not the same as
        /// having reviewed it.
        #[arg(long = "vendored")]
        vendored: Vec<String>,
    },
    /// Point the share URL at a specific version.
    Alias { id: String, version: u32 },
    /// Point the share URL at nothing. Destroys no version.
    Unpublish { id: String },
    /// What is published, and at which version.
    List,
    /// One bundle: its versions, its alias, and who can reach it.
    Status { id: String },
    /// Copy a published bundle out of the store, by id — never by path.
    Fetch {
        id: String,
        #[arg(long)]
        out: PathBuf,
        /// A specific version. Defaults to whatever the alias points at.
        #[arg(long)]
        version: Option<u32>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = match &cli.store {
        Some(path) => BundleStore::open(path)?,
        None => BundleStore::open_default()?,
    };
    let now = chrono::Utc::now().to_rfc3339();

    match cli.command {
        Command::Render { source, out, title } => {
            let rendered = render::report(&source, &out, title.as_deref())?;
            // Run at render time too, so the cheap verb is where you find out.
            // Discovering it at publish — the expensive one, the one that costs
            // a review — would mean the review queue is where broken bundles
            // surface.
            vendor::gate_rendered(&rendered.dir, &source)?;
            println!("{} → {}", rendered.template, rendered.dir.display());
            println!("  title  {}", rendered.title);
            println!("  class  {}", rendered.class.as_str());
            println!("  open   {}", rendered.dir.join("index.html").display());
            println!(
                "\nLook at it, then publish it:\n  factory-publish publish <id> {} --source {}",
                rendered.dir.display(),
                source.display()
            );
        }

        Command::Publish {
            id,
            bundle,
            title,
            description,
            sources,
            no_alias,
        } => {
            // The manifest a previous render left behind is the best source of
            // the title and class, and re-deriving them from flags would let a
            // publish disagree with what was rendered.
            let title = title.unwrap_or_else(|| id.clone());
            let mut absolute = Vec::new();
            for source in &sources {
                absolute.push(
                    source
                        .canonicalize()
                        .with_context(|| format!("--source {} does not exist", source.display()))?,
                );
            }
            // The gate, before anything is written. A version is immutable, so
            // a bundle that reaches the store with an external reference in it
            // is one that can only be superseded, never fixed.
            vendor::gate(&bundle)?;
            let published = store.publish(
                &id,
                &bundle,
                &title,
                description,
                "report",
                mecha_manifest::ContentClass::Static,
                absolute,
                &now,
            )?;
            if published.existing {
                println!(
                    "{id} v{} — identical bytes, so nothing new was minted",
                    published.version
                );
            } else {
                println!("{id} v{} published", published.version);
            }
            println!("  digest {}", published.digest);
            println!("  path   {}", published.path.display());
            if !no_alias {
                let visibility = store
                    .alias(&id)?
                    .map(|a| a.visibility)
                    .unwrap_or(Visibility::Private);
                store.set_alias(&id, Some(published.version), visibility, &now)?;
                println!("  alias  → v{}", published.version);
            }
            reach(&store, &id)?;
        }

        Command::Notebook {
            source,
            out,
            title,
            timeout,
            allow_unvendored_runtime,
            vendor_runtime,
        } => {
            let options = notebook::NotebookOptions {
                title,
                timeout: std::time::Duration::from_secs(timeout),
                allow_unvendored_runtime,
                vendor_runtime: match vendor_runtime {
                    Some(v) => Some((v, mecha_factory_publish::pyodide::default_cache()?)),
                    None => None,
                },
                ..notebook::NotebookOptions::default()
            };
            let bundle = notebook::notebook(&source, &out, &options)?;
            println!(
                "{} → {}",
                bundle.rendered.template,
                bundle.rendered.dir.display()
            );
            println!("  title  {}", bundle.rendered.title);
            println!("  class  {}", bundle.rendered.class.as_str());
            println!("  inlined {} script(s) moved into files", bundle.inlined);
            if let Some(r) = &bundle.runtime {
                println!(
                    "  runtime pyodide {} — {} files, {} package(s), {:.1} MB",
                    r.version,
                    r.files,
                    r.packages,
                    r.bytes as f64 / 1e6
                );
            }
            for (name, why) in &bundle.removed {
                println!("  pruned {name} — {why}");
            }
            for tree in &bundle.vendored {
                println!("  pinned {}  {}", tree.path.display(), tree.digest);
            }
            vendor::gate_with(&bundle.rendered.dir, &bundle.vendored)?;
            println!(
                "  open   {}",
                bundle.rendered.dir.join("index.html").display()
            );
        }

        Command::Mcp => mecha_factory_publish::mcp::serve(cli.store.clone())?,

        Command::Serve {
            bundle,
            class,
            port,
            no_csp,
        } => {
            let class = match class.as_str() {
                "static" => mecha_manifest::ContentClass::Static,
                "interactive" => mecha_manifest::ContentClass::Interactive,
                "compute" => mecha_manifest::ContentClass::Compute,
                other => bail!("unknown class `{other}` (static | interactive | compute)"),
            };
            let mut preview = mecha_factory_publish::serve::Preview::bind(&bundle, class, port)?;
            preview.without_policy = no_csp;
            println!("{}  ({})", preview.url()?, class.as_str());
            if no_csp {
                println!("  ⚠ NO POLICY — a diagnostic. Nothing served this way is verified.");
            }
            // Printed, because the whole point of this server is that the
            // policy is real — and a policy nobody read is one nobody checked.
            for (name, value) in class.headers() {
                println!("  {name}: {value}");
            }
            preview.serve_forever()?;
        }

        Command::Check { bundle, vendored } => {
            let mut pinned = Vec::new();
            for spec in &vendored {
                match spec.split_once('=') {
                    Some((path, digest)) => pinned.push(vendor::Vendored {
                        path: PathBuf::from(path),
                        digest: digest.to_string(),
                        description: path.to_string(),
                    }),
                    None => {
                        let dir = bundle.join(spec);
                        anyhow::ensure!(dir.is_dir(), "{} is not a directory", dir.display());
                        println!(
                            "--vendored {spec}={}    ← after reviewing it",
                            mecha_factory_publish::store::digest_tree(&dir)?
                        );
                    }
                }
            }
            let findings = vendor::scan_with(&bundle, &pinned)?;
            if findings.is_empty() {
                // "Self-contained" is a conclusion; a pin is a *claim* somebody
                // made. Absorbing the second into the first is how a bundle
                // that cannot boot under the CSP comes to be described as
                // clean — so the claim stays on screen next to the conclusion.
                if pinned.is_empty() {
                    println!("{} is self-contained", bundle.display());
                } else {
                    println!(
                        "{} is self-contained outside its pinned tree(s)",
                        bundle.display()
                    );
                    for tree in &pinned {
                        println!(
                            "  pinned as reviewed, not scanned: {}  {}",
                            tree.path.display(),
                            tree.digest
                        );
                    }
                }
                return Ok(());
            }
            // Printed *and* returned as a failure: the exit code is what a
            // script reads, and the list is what a person needs.
            for finding in &findings {
                println!("{finding}");
            }
            bail!(
                "{} external reference(s) — this bundle would not publish",
                findings.len()
            );
        }

        Command::Alias { id, version } => {
            let visibility = store
                .alias(&id)?
                .map(|a| a.visibility)
                .unwrap_or(Visibility::Private);
            store.set_alias(&id, Some(version), visibility, &now)?;
            println!("{id} → v{version}");
            reach(&store, &id)?;
        }

        Command::Unpublish { id } => {
            let before = store.alias(&id)?.and_then(|a| a.version);
            store.set_alias(&id, None, Visibility::Private, &now)?;
            match before {
                Some(v) => println!("{id}: the share URL no longer resolves (was v{v})"),
                None => println!("{id}: already unpublished"),
            }
            // Said every time, because "unpublish" reads like "delete" and the
            // difference is the whole point: the record survives.
            println!(
                "  {} version(s) remain on disk — nothing here deletes one",
                store.versions(&id)?.len()
            );
        }

        Command::List => {
            let bundles = store.bundles()?;
            if bundles.is_empty() {
                println!("nothing published yet — {}", store.root().display());
                return Ok(());
            }
            for id in bundles {
                let versions = store.versions(&id)?;
                let alias = store.alias(&id)?;
                let at = match alias.as_ref().and_then(|a| a.version) {
                    Some(v) => format!("→ v{v}"),
                    None => "→ (taken down)".into(),
                };
                println!(
                    "{id:<28} {at:<16} {} version(s), latest v{}",
                    versions.len(),
                    versions.last().copied().unwrap_or(0)
                );
            }
        }

        Command::Status { id } => {
            let versions = store.versions(&id)?;
            if versions.is_empty() {
                bail!("no bundle `{id}` in {}", store.root().display());
            }
            let alias = store.alias(&id)?;
            println!("{id}");
            for version in &versions {
                let m = store.manifest(&id, *version)?;
                let marker = if alias.as_ref().and_then(|a| a.version) == Some(*version) {
                    "→"
                } else {
                    " "
                };
                println!(
                    "{marker} v{version}  {}  {}  {}",
                    m.published_at.as_deref().unwrap_or("—"),
                    m.class.as_str(),
                    m.digest.as_deref().unwrap_or("—")
                );
                for source in &m.sources {
                    println!("      source {}", source.display());
                }
            }
            if alias.as_ref().and_then(|a| a.version).is_none() {
                println!("  the share URL resolves to nothing (taken down)");
            }
            reach(&store, &id)?;
        }

        Command::Fetch { id, out, version } => {
            // The caller names a bundle id, never a path. The store resolves it
            // internally, so no path from outside is ever joined onto the root
            // — the same pattern as naming an account rather than a provider.
            let version = match version {
                Some(v) => v,
                None => store
                    .alias(&id)?
                    .and_then(|a| a.version)
                    .context("that bundle has no aliased version; name one with --version")?,
            };
            let from = store.version_dir(&id, version);
            if !from.is_dir() {
                bail!("{id} has no version {version}");
            }
            copy_dir(&from, &out)?;
            println!("{id} v{version} → {}", out.display());
        }
    }
    Ok(())
}

/// Say who can actually reach this right now.
///
/// Printed rather than assumed, because at this stage there is no gate origin:
/// the tailnet is the boundary, and `visibility` is recorded metadata that
/// nothing yet enforces. A flag that reads as enforcement and is not would be
/// exactly the wrong thing to leave unsaid.
fn reach(store: &BundleStore, id: &str) -> Result<()> {
    let visibility = store
        .alias(id)?
        .map(|a| a.visibility)
        .unwrap_or(Visibility::Private);
    println!(
        "  reach  whoever can read {} — recorded visibility is `{}`, which nothing \
         enforces until there is a gate origin",
        store.root().display(),
        match visibility {
            Visibility::Private => "private",
            Visibility::Public => "public",
        }
    );
    Ok(())
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
