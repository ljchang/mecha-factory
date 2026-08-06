//! `factory` — the public surface, as one binary on one box.
//!
//! What it is allowed to do is deliberately short: serve published bytes under
//! the policy their class declares, accept publishes from a key it can verify,
//! and hold typed requests until home comes and takes them. It renders nothing,
//! runs no model, initiates no connection, and holds no credential that reaches
//! home. That list is the security posture, and it is meant to be checkable by
//! reading this file's module list.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use mecha_factory::config::{self, Config};
use mecha_factory::db::{self, Db, Scope};
use mecha_factory::keys;

#[derive(Parser)]
#[command(name = "factory", version, about = "The mecha public surface")]
struct Cli {
    /// The server's TOML configuration. Defaults to `$FACTORY_CONFIG`, then
    /// `./factory.toml`.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Operate directly on a data directory, without a configuration file.
    /// What the tests use, and what makes `key create` work on a box before
    /// the origins are decided.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mint, list and revoke the keys mecha authenticates with.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Serve the three origins.
    Serve {
        /// Three loopback ports instead of three names, plain HTTP, and the
        /// same router. Everything but TLS.
        #[arg(long)]
        dev: bool,
        /// Dev only: the gate's port. Artifacts and compute take the next two.
        #[arg(long, default_value_t = 8400)]
        port: u16,
    },
    /// The queue of typed requests, from the box's side.
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    /// Parse the configuration, report what it would serve, and exit. What a
    /// deploy runs before restarting anything.
    Check,
}

#[derive(Subcommand)]
enum QueueAction {
    /// What is waiting, and what state it is in.
    List,
    /// Put a record in by hand.
    ///
    /// The inbound form and its verification are step 7; until they exist this
    /// is the only writer, and it is how the drain path at home is exercised
    /// end to end before there is a stranger to exercise it. It is not a back
    /// door: it runs on the box, as whoever already owns the box, and it
    /// **validates against the uploaded type** exactly as the form endpoint
    /// will — a queue that could hold a record no schema accepts would break
    /// the one property that makes draining safe.
    Add {
        /// The request type id, which must already be uploaded.
        #[arg(long = "type")]
        type_id: String,
        /// A JSON object of field values. `-` reads stdin.
        #[arg(long)]
        payload: PathBuf,
        /// `queued` is drainable; `submitted` is what an unverified request
        /// looks like and is never drained.
        #[arg(long, default_value = "queued")]
        state: String,
    },
}

#[derive(Subcommand)]
enum KeyAction {
    /// Mint a key and print it **once**. There is no way to read it back.
    Create {
        /// `publish` (types, bundles, aliases) or `drain` (the queue).
        #[arg(long)]
        scope: String,
        /// Free text, so `key list` says which machine holds it.
        #[arg(long, default_value = "")]
        label: String,
    },
    /// Every key, with its scope and whether it still works.
    List,
    /// Stop a key working. The row stays: it is the record that the key
    /// existed and when it stopped.
    Revoke { id: String },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FACTORY_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match &cli.command {
        Command::Key { action } => key(&cli, action),
        Command::Serve { dev, port } => serve(&cli, *dev, *port),
        Command::Queue { action } => queue(&cli, action),
        Command::Check => check(&cli),
    }
}

/// Where the ledger is, from whichever of the two the operator gave us.
fn open_db(cli: &Cli) -> Result<Db> {
    if let Some(dir) = &cli.data_dir {
        return Db::open(&dir.join("factory.db"));
    }
    Db::open(&load_config(cli)?.db_path())
}

fn load_config(cli: &Cli) -> Result<Config> {
    let path = cli
        .config
        .clone()
        .or_else(|| std::env::var("FACTORY_CONFIG").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("factory.toml"));
    if !path.exists() {
        bail!(
            "no configuration at {} — pass --config, set FACTORY_CONFIG, or \
             use --data-dir for commands that only need the ledger",
            path.display()
        );
    }
    Config::load(&path)
}

fn key(cli: &Cli, action: &KeyAction) -> Result<()> {
    let db = open_db(cli)?;
    match action {
        KeyAction::Create { scope, label } => {
            let scope = Scope::parse(scope)?;
            let minted = keys::mint(&db, scope, label).context("minting a key")?;
            // stdout, alone, so `factory key create … > publish.key` is the
            // whole installation procedure.
            println!("{}", minted.token);
            eprintln!(
                "minted {} key {} ({}). This is the only time it is shown — \
                 install it now, at mode 0600.",
                scope.as_str(),
                minted.row.id,
                if label.is_empty() { "no label" } else { label }
            );
            Ok(())
        }
        KeyAction::List => {
            let rows = db.keys()?;
            if rows.is_empty() {
                println!("no keys");
                return Ok(());
            }
            for row in rows {
                println!(
                    "{}  {:<7}  {:<19}  {}{}",
                    row.id,
                    row.scope.as_str(),
                    row.created_at,
                    if row.label.is_empty() {
                        "-"
                    } else {
                        &row.label
                    },
                    match &row.revoked_at {
                        Some(at) => format!("  revoked {at}"),
                        None => String::new(),
                    }
                );
            }
            Ok(())
        }
        KeyAction::Revoke { id } => {
            if db.key_revoke(id, &db::now())? {
                println!("revoked {id}");
            } else {
                println!("{id} is not a live key");
            }
            Ok(())
        }
    }
}

fn queue(cli: &Cli, action: &QueueAction) -> Result<()> {
    let db = open_db(cli)?;
    match action {
        QueueAction::List => {
            for row in db.drain(0, 1000)? {
                println!(
                    "{:>5}  {:<12}  {}  {}",
                    row.seq, row.type_id, row.created_at, row.payload
                );
            }
            Ok(())
        }
        QueueAction::Add {
            type_id,
            payload,
            state,
        } => {
            let text = if payload.as_os_str() == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(payload)
                    .with_context(|| format!("reading {}", payload.display()))?
            };
            let raw: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text)
                .context("the payload must be a JSON object of field values")?;

            let Some(stored) = db.type_get(type_id)? else {
                bail!(
                    "no type `{type_id}` is uploaded. The queue only ever holds \
                     records that validate against a schema mecha itself uploaded, \
                     which is what makes draining safe."
                );
            };
            let request_type = mecha_manifest::RequestType::from_toml(&stored.manifest)?;
            let submission = request_type.validate(&raw).map_err(|errors| {
                anyhow::anyhow!(
                    "the payload does not validate:\n{}",
                    errors
                        .iter()
                        .map(|e| format!("  {}: {}", e.field, e.message))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })?;

            let seq = db.queue_add(
                type_id,
                state,
                &serde_json::to_string(&submission.values)?,
                &db::now(),
            )?;
            println!("queued {seq} ({type_id}, {state})");
            Ok(())
        }
    }
}

/// A runtime is built here rather than by an attribute on `main`, so that
/// `key` and `check` — which are ordinary file operations — never start one.
fn serve(cli: &Cli, dev: bool, port: u16) -> Result<()> {
    let config = if dev {
        let data_dir = cli
            .data_dir
            .clone()
            .or_else(|| cli.config.as_ref().map(|_| PathBuf::new()))
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("factory-dev"));
        Config::dev(data_dir, port)
    } else {
        load_config(cli)?
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?
        .block_on(mecha_factory::serve::run(config, dev))
}

fn check(cli: &Cli) -> Result<()> {
    let config = load_config(cli)?;
    let db = Db::open(&config.db_path())?;
    println!("data      {}", config.data_dir.display());
    for role in [
        config::Role::Gate,
        config::Role::Artifacts,
        config::Role::Compute,
    ] {
        println!("{:<9} {}", role.as_str(), config.base_url(role));
    }
    println!(
        "tls       {}",
        match &config.tls {
            Some(tls) if tls.staging => "acme (staging directory — certificates nobody trusts)",
            Some(_) => "acme",
            None => "none (loopback only)",
        }
    );
    println!(
        "ledger    {} bundles, {} queued, {} keys",
        db.bundle_count()?,
        db.queue_depth()?,
        db.keys()?.iter().filter(|k| k.revoked_at.is_none()).count()
    );
    Ok(())
}
