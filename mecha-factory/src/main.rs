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
    /// The people this box serves.
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// Take a published version out of service, on a report.
    ///
    /// Reversible and destroys nothing: the bytes stay, so an accusation that
    /// turns out to be wrong costs nothing and one that turns out to be right
    /// leaves the evidence. Destroying bytes is a different verb, and it is
    /// deliberately not automated.
    Withhold {
        /// The user's handle.
        handle: String,
        /// The bundle id.
        id: String,
        version: u32,
        #[arg(long)]
        reason: Option<String>,
        /// Put it back.
        #[arg(long)]
        undo: bool,
    },
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
    /// Delete what we are no longer entitled to hold.
    ///
    /// Two sweeps, and both are policy rather than tidying. An unverified
    /// submission past its link's expiry is a stranger's data with no consent
    /// behind it — the one state where keeping the record serves nobody. And a
    /// record past its type's `retain_days` is one we said we would not keep.
    /// Run it nightly; it says what it removed.
    Sweep,
    /// Parse the configuration, report what it would serve, and exit. What a
    /// deploy runs before restarting anything.
    Check,
}

#[derive(Subcommand)]
enum UserAction {
    /// Create a user and claim their handle.
    ///
    /// Today this is how an account comes to exist; a signup flow later calls
    /// exactly this. The front door is new, the mechanism is not — a parallel
    /// path is how two ways of creating a user come to disagree about what a
    /// valid one is.
    Create {
        /// The label in their hostname: `alice` in `alice.artifacts.example.org`.
        /// Claimed once and never issued again, even if the account closes.
        handle: String,
        #[arg(long, default_value = "")]
        email: String,
        /// Mint their first publish key at the same time, and print it.
        #[arg(long)]
        with_key: bool,
    },
    List,
    /// Stop an account serving and publishing. Never a delete.
    Suspend {
        handle: String,
    },
    /// Undo a suspension.
    Restore {
        handle: String,
    },
    /// Set how many bytes of published artifacts this user may hold.
    Quota {
        handle: String,
        bytes: i64,
    },
}

#[derive(Subcommand)]
enum QueueAction {
    /// What is waiting, and what state it is in.
    List {
        #[arg(long)]
        handle: String,
    },
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
        /// Whose queue it goes in.
        #[arg(long)]
        handle: String,
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
        /// Whose key it is.
        #[arg(long)]
        handle: String,
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
        Command::User { action } => user(&cli, action),
        Command::Withhold {
            handle,
            id,
            version,
            reason,
            undo,
        } => withhold(&cli, handle, id, *version, reason.as_deref(), *undo),
        Command::Key { action } => key(&cli, action),
        Command::Serve { dev, port } => serve(&cli, *dev, *port),
        Command::Queue { action } => queue(&cli, action),
        Command::Sweep => sweep(&cli),
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

/// Look a user up by handle, or say so.
fn find_user(db: &Db, handle: &str) -> Result<mecha_factory::db::UserRow> {
    db.user_by_handle(handle)?
        .ok_or_else(|| anyhow::anyhow!("no user with the handle `{handle}`"))
}

fn user(cli: &Cli, action: &UserAction) -> Result<()> {
    let db = open_db(cli)?;
    match action {
        UserAction::Create {
            handle,
            email,
            with_key,
        } => {
            mecha_factory::config::valid_handle(handle)?;
            let user = db.user_create(handle, email, &db::now())?;
            println!("{}  {}  {}", user.id, user.handle, user.email);
            if *with_key {
                let minted = keys::mint(&db, &user.id, Scope::Publish, "first")?;
                println!("{}", minted.token);
                eprintln!(
                    "that is {}'s publish key, shown once. A drain key is \
                     `factory key create --handle {} --scope drain`.",
                    user.handle, user.handle
                );
            }
            Ok(())
        }
        UserAction::List => {
            let users = db.users()?;
            if users.is_empty() {
                println!("no users");
                return Ok(());
            }
            for user in users {
                println!(
                    "{}  {:<20} {:<10} {:<28} quota {}",
                    user.id, user.handle, user.status, user.email, user.quota_bytes
                );
            }
            Ok(())
        }
        UserAction::Suspend { handle } => {
            let user = find_user(&db, handle)?;
            db.user_status(&user.id, "suspended")?;
            println!(
                "suspended {handle}: their artifacts stop being served and their keys stop working"
            );
            println!("nothing was deleted — `factory user restore {handle}` puts it back");
            Ok(())
        }
        UserAction::Restore { handle } => {
            let user = find_user(&db, handle)?;
            db.user_status(&user.id, "active")?;
            println!("restored {handle}");
            Ok(())
        }
        UserAction::Quota { handle, bytes } => {
            let user = find_user(&db, handle)?;
            db.user_quota(&user.id, *bytes)?;
            println!("{handle} may hold {bytes} bytes");
            Ok(())
        }
    }
}

fn withhold(
    cli: &Cli,
    handle: &str,
    id: &str,
    version: u32,
    reason: Option<&str>,
    undo: bool,
) -> Result<()> {
    let db = open_db(cli)?;
    let user = find_user(&db, handle)?;
    let now = db::now();
    let changed = db.bundle_withhold(
        &user.id,
        id,
        version,
        if undo { None } else { reason },
        if undo { None } else { Some(&now) },
    )?;
    if !changed {
        bail!("{handle} has no {id} version {version}");
    }
    if undo {
        println!("{handle}/{id} v{version} is served again");
    } else {
        println!("{handle}/{id} v{version} is withheld — served to nobody, and still on disk");
    }
    Ok(())
}

fn key(cli: &Cli, action: &KeyAction) -> Result<()> {
    let db = open_db(cli)?;
    match action {
        KeyAction::Create {
            scope,
            label,
            handle,
        } => {
            let scope = Scope::parse(scope)?;
            let user = find_user(&db, handle)?;
            let minted = keys::mint(&db, &user.id, scope, label).context("minting a key")?;
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
            let users = db.users()?;
            for row in rows {
                let handle = users
                    .iter()
                    .find(|u| u.id == row.user_id)
                    .map(|u| u.handle.as_str())
                    .unwrap_or("(none)");
                println!(
                    "{}  {:<14} {:<7}  {:<19}  {}{}",
                    row.id,
                    handle,
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
        QueueAction::List { handle } => {
            let user = find_user(&db, handle)?;
            for row in db.drain(&user.id, 0, 1000)? {
                println!(
                    "{:>5}  {:<12}  {}  {}",
                    row.seq, row.type_id, row.created_at, row.payload
                );
            }
            Ok(())
        }
        QueueAction::Add {
            handle,
            type_id,
            payload,
            state,
        } => {
            let user = find_user(&db, handle)?;
            let text = if payload.as_os_str() == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(payload)
                    .with_context(|| format!("reading {}", payload.display()))?
            };
            let raw: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text)
                .context("the payload must be a JSON object of field values")?;

            let Some(stored) = db.type_get(&user.id, type_id)? else {
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
                &user.id,
                type_id,
                state,
                &serde_json::to_string(&submission.values)?,
                &db::now(),
                None,
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

fn sweep(cli: &Cli) -> Result<()> {
    let db = open_db(cli)?;
    let now = db::now();
    let unverified = db.expire_unverified(&now)?;
    let retained = db.expire_retained(&now)?;
    println!("{unverified} unverified submission(s) past their link's expiry, removed");
    println!("{retained} record(s) past their retention window, removed");
    if unverified + retained == 0 {
        println!("(nothing was due)");
    }
    Ok(())
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
    println!("tls       {}", mecha_factory::tls::describe(&config));
    // Forms are refused outright without a way to send a link, so what the
    // mailer is belongs in the same breath as what the certificate is.
    // Resolved from the config rather than assumed. This printed `LogMailer`
    // unconditionally, so a box with SES wired would still have reported that
    // links were being written to the journal — a check that reassures you
    // about a deployment it never looked at.
    println!(
        "mail      {}",
        match mecha_factory::mail::configured(&config) {
            Ok(mailer) => mailer.describe(),
            Err(e) => format!("MISCONFIGURED — the box will refuse to start: {e:#}"),
        }
    );
    println!(
        "ledger    {} users, {} bundles, {} queued, {} keys",
        db.users()?.len(),
        db.bundle_count()?,
        db.queue_depth(None)?,
        db.keys()?.iter().filter(|k| k.revoked_at.is_none()).count()
    );
    Ok(())
}
