//! `factory-publish` — render a bundle, publish it, move its alias.
//!
//! The CLI exists before the MCP server on purpose: every verb is testable from
//! a shell, and the server is the same library with a different front end. The
//! split between the cheap verb and the expensive one is real here too — `render`
//! writes a directory you look at, and `publish` is the one that costs a review.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use mecha_factory_publish::{
    notebook, records::Record, remote, render, store::BundleStore, vendor,
};
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
        /// The palette to bake in. Match the box's `theme` so a report and the
        /// forms beside it read as one surface. An unknown name falls back to
        /// the default rather than refusing, exactly as the box's config does.
        #[arg(long, default_value = "nocturne")]
        theme: String,
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
        /// Store it locally and do not send it to the box, even though one is
        /// configured. What you want when the box is down and you would rather
        /// retry later with `push` than fail the publish.
        #[arg(long)]
        no_push: bool,
        /// `public` or `private`. Omitted keeps whatever this bundle already
        /// was, and a bundle that has never been anything is **private** — the
        /// origin serves a private bundle to nobody, so the default of getting
        /// this wrong points the safe way.
        #[arg(long)]
        visibility: Option<String>,
    },
    /// Render a marimo notebook as a `compute`-class bundle. The cells run in
    /// the reader's browser under Pyodide, not here.
    Notebook {
        /// The notebook `.py`.
        source: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        title: Option<String>,
        /// Seconds the export may take. It does not run your cells — that
        /// happens in the reader's browser — but any subprocess can hang.
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
    Mcp {
        /// Every path a model supplies must resolve inside this directory.
        /// Defaults to the working directory — which is the workspace when
        /// mecha confines the server, and mecha's own when it does not.
        #[arg(long)]
        root: Option<PathBuf>,
    },
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
    /// Send a stored version to the box, and point the share URL at it.
    ///
    /// `publish` already does this when a remote is configured; this is the
    /// retry, and it is safe to run twice — identical bytes return the version
    /// the box already holds.
    Push {
        id: String,
        /// Defaults to the version the local alias points at.
        #[arg(long)]
        version: Option<u32>,
        /// Send the bytes without moving the share URL.
        #[arg(long)]
        no_alias: bool,
    },
    /// Ask the box whether it is up, and what it holds.
    Remote,
    /// Point the share URL at a specific version.
    Alias {
        id: String,
        version: u32,
        /// `public` or `private`. Omitted keeps what it was.
        #[arg(long)]
        visibility: Option<String>,
    },
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
    /// Request types: the manifests that become forms strangers fill in.
    #[command(subcommand)]
    Type(TypeAction),
    /// Take what the box has verified and not yet handed over.
    ///
    /// Pair this machine: spend a pairing code, install its keys.
    ///
    /// A CLI and deliberately not a tool: pairing decides what an agent on
    /// this machine may do, so it is not something an agent should do to
    /// itself as a side effect of conversation.
    Connect {
        /// The code from the signup page or `factory pair create`.
        code: String,
        /// `https://gate.example.org`. Falls back to `$FACTORY_GATE`.
        #[arg(long)]
        gate: Option<String>,
        /// The handle this machine should publish for. Required when stdin
        /// is not a terminal; prompted for otherwise. The server refuses a
        /// mismatch without spending the code — typing it is the whole
        /// confirmation.
        #[arg(long)]
        handle: Option<String>,
        /// What the box's `key list` shows for these keys. Defaults to this
        /// machine's hostname.
        #[arg(long)]
        label: Option<String>,
        /// Pair afresh over keys already installed here. The old keys stay
        /// valid on the box until revoked there.
        #[arg(long)]
        replace: bool,
    },
    /// Run the box, from here: users, invites, keys, withholds.
    ///
    /// Behind `operate.key` — minted once on the box, held on the machine
    /// the human chooses, and never installed by pairing. Deliberately
    /// CLI-only, never MCP tools: suspending users and minting invites is
    /// power an agent should not wield as a side effect of conversation,
    /// the same rule that keeps drain's prose off the tool surface.
    Operator {
        #[command(subcommand)]
        action: OperatorAction,
    },
    /// Retire this machine's keys: each revokes itself, then leaves the disk.
    ///
    /// Needs no extra privilege — a credential may always retire itself —
    /// which is what makes a compromised laptop recoverable by its owner
    /// rather than only by whoever holds root on the box.
    Disconnect,
    /// A CLI and deliberately not a tool: the common case is "nothing new",
    /// and it has to cost zero tokens. A trigger runs this on a schedule and
    /// only spawns an agent when something actually arrived.
    Drain {
        /// Where records are written. Defaults to `~/.mecha/requests`.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Fetch and report, writing nothing and acknowledging nothing.
        #[arg(long)]
        dry_run: bool,
        /// One JSON object, for a trigger's `if` rather than a human's eye.
        #[arg(long)]
        json: bool,
        /// Hold the request open up to this many seconds, answering the
        /// moment a record lands (the box caps it near 25). What lets a
        /// loop over this command react in seconds while still never being
        /// dialled by the box — home initiates every connection.
        #[arg(long, default_value_t = 0)]
        wait: u64,
    },
    /// An instrument's availability: computed here, served by the box.
    #[command(subcommand)]
    Slots(SlotsAction),
    /// Group polls: seeded candidates out, tri-state answers back.
    #[command(subcommand)]
    Polls(PollsAction),
    /// Who you are, at the top of your public pages.
    ///
    /// The record lives as TOML on this machine and is pushed to the box. It
    /// can also be edited in the cockpit, so `pull` is not optional
    /// bookkeeping: an edit made in a browser that nobody pulls is an edit a
    /// later push may overwrite.
    #[command(subcommand)]
    Profile(RecordAction),
    /// The hangar's own heading and intro. Its lines are wired from what is
    /// public, so it declares none.
    #[command(subcommand)]
    Hangar(RecordAction),
    /// A switchboard: a page of hand-patched lines, named by you.
    #[command(subcommand)]
    Switchboard(SwitchboardAction),
}

// `Create` carries every optional the meeting poll takes; the enum lives for
// one command and is never stored, so the size difference costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum PollsAction {
    /// Create a poll from the availability pipeline:
    ///
    ///   mecha-mail freebusy --days 60 --json |     ///     factory-publish polls create book lab-feb --policy …     ///     --title "Lab meeting" --duration 60     ///     --participant "Priya=priya@example.edu" --participant "Tal=tal@w.edu"
    ///
    /// The candidates are the engine's earliest feasible slots — already
    /// minus the user's real busy time — capped small, because a poll a
    /// colleague answers in ten seconds is the one that gets answered. The
    /// box mints one capability URL per participant; addresses never leave
    /// this machine. For a meeting poll the links are not printed: the
    /// record queues the invitations and `mecha-mail polls` sends each
    /// person theirs on the timer. A `--spec` poll's links are printed (or
    /// land in `links.csv`) for the agent's outbox-reviewed mail or your own.
    Create {
        /// The instrument whose policy seeds this.
        instrument: String,
        /// A new poll id (lowercase, digits, -, _).
        poll_id: String,
        /// A general poll's questions, as a TOML spec (POLL-DESIGN.md's
        /// shape: choice, ranking, likert, vas, text). Replaces the
        /// availability pipeline entirely — no stdin, no policy, and the
        /// title and deadline come from the spec.
        #[arg(long, conflicts_with_all = ["policy", "title", "duration", "deadline", "max_candidates", "earliest", "latest", "message", "account"])]
        spec: Option<PathBuf>,
        /// The availability policy TOML. With it, stdin is `mecha-mail
        /// freebusy --json` output (the pipeline contract); without it, both
        /// come from what `slots push` last saw for the instrument.
        #[arg(long)]
        policy: Option<PathBuf>,
        #[arg(long, required_unless_present = "spec")]
        title: Option<String>,
        /// Meeting length in minutes; must be one the policy offers.
        #[arg(long, required_unless_present = "spec")]
        duration: Option<u32>,
        /// `Name=email`, repeatable.
        #[arg(long = "participant")]
        participants: Vec<String>,
        /// A `name,email` CSV — the class-section shape. Rows join any
        /// `--participant` flags; `links.csv` lands beside the record for
        /// the LMS mail-merge. A `link`-audience spec takes neither.
        #[arg(long)]
        roster: Option<PathBuf>,
        /// RFC 3339, or a date that closes at the policy's `deadline_hour`.
        /// Default: `[poll] deadline_days` out. Answers freeze on their own.
        #[arg(long)]
        deadline: Option<String>,
        /// The earliest date a candidate may fall on.
        #[arg(long)]
        earliest: Option<String>,
        /// The latest date a candidate may fall on.
        #[arg(long)]
        latest: Option<String>,
        /// Your sentence, above the invitation's default block.
        #[arg(long)]
        message: Option<String>,
        /// The mail account the invitations and the event come from.
        #[arg(long)]
        account: Option<String>,
        /// How many candidate slots to offer (5–15 is the point).
        #[arg(long, default_value_t = 10)]
        max_candidates: usize,
    },
    /// Advance every open meeting poll by one tick: observe the tally on the
    /// box, queue the nudge, close on the poll's own terms, decide the
    /// verdict, and close on the box once the outcome exists. Sends nothing
    /// and books nothing — that is `mecha-mail polls`, on the same timer.
    Sweep,
    /// The tally: ranked with the auto-book verdict for a times poll,
    /// per-question tallies for a general one.
    Status {
        instrument: String,
        poll_id: String,
        /// One JSON object for the agent. Times polls: candidates, answers,
        /// ranked, clean_winner. General polls: the spec, per-question
        /// tallies, and text answers in their own clearly-marked prose
        /// section — never mixed into the typed one.
        #[arg(long)]
        json: bool,
    },
    /// Freeze the answers.
    Close {
        instrument: String,
        poll_id: String,
        /// What happened — rendered at the top of the closed page, so the
        /// links people hold answer "so what happened?".
        #[arg(long)]
        resolution: Option<String>,
    },
    /// Ballots as CSV, one row per ballot, one column per question —
    /// injection-hardened, and nameless when the poll is anonymous.
    Export {
        instrument: String,
        poll_id: String,
        /// Write here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// The PowerPoint content add-in's manifest: write it, sideload it
    /// once per machine, and any deck can carry a live poll object that
    /// asks for a projector URL. The GUID derives from the gate, so
    /// regenerating upgrades rather than multiplying add-ins.
    Addin {
        /// Where to write the manifest (default: mecha-polls.xml here).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SlotsAction {
    /// Compute bookable slots and replace the instrument's cache on the box.
    ///
    /// The write half of the slot-refresh pipeline:
    ///
    ///   mecha-mail freebusy --days 60 --json | factory-publish slots push book --policy …
    ///
    /// stdin is `mecha-mail freebusy --json` output. A scheduled command
    /// with no model anywhere — a systemd timer runs it every fifteen
    /// minutes, and everything it refuses (a stale answer, busy data that
    /// stops short of the horizon) it refuses loudly, because the failure
    /// mode of pushing anyway is offering strangers time the user does not
    /// have.
    Push {
        /// The instrument whose cache to replace.
        id: String,
        /// The availability policy TOML (the `[availability]` vocabulary).
        #[arg(long)]
        policy: PathBuf,
        /// Compute and print the slots, pushing nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum OperatorAction {
    /// A one-time browser link for the admin panel at /admin.
    ///
    /// The operate key never enters a browser: this asks the box for a URL
    /// that works once and expires in minutes, and the session it becomes
    /// lives half a day.
    Signin,
    /// Everyone, with status and queue depth.
    Users,
    /// Stop an account serving and publishing. Never a delete.
    Suspend { handle: String },
    /// Undo a suspension.
    Restore { handle: String },
    /// Mint an invite. The box mails it; the link is also printed.
    Invite {
        email: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Every invite and what became of it.
    Invites,
    /// Stop an unclaimed invite working.
    RevokeInvite { id: String },
    /// Every key, live and dead, with whose it is and when it last worked.
    Keys,
    /// Break-glass: stop any key working, including another operator's.
    RevokeKey { id: String },
    /// Take a version out of service, reversibly, on a report.
    Withhold {
        handle: String,
        id: String,
        version: u32,
        #[arg(long)]
        reason: Option<String>,
        /// Put it back.
        #[arg(long)]
        undo: bool,
    },
}

#[derive(Subcommand)]
enum TypeAction {
    /// Upload a manifest, which is what makes its form exist.
    Push {
        /// The `.toml` manifest. Its own `id` names the type.
        manifest: PathBuf,
    },
    /// What the box is serving forms for.
    List,
    /// Check a manifest without uploading it, and print the form's URL.
    Check {
        manifest: PathBuf,
        /// The handle the form would be served under.
        #[arg(long)]
        handle: Option<String>,
    },
}

#[derive(Subcommand)]
enum RecordAction {
    /// Send the local file to the box.
    Push {
        /// Defaults to the standard path for this record.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Write the box's copy back over the local file.
    Pull {
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SwitchboardAction {
    /// Send one switchboard.
    Push {
        /// The slug: the URL segment, and the filename.
        slug: String,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Write one back over its local file, creating it if this machine has
    /// never seen it.
    Pull {
        slug: String,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// What boards the box holds, and which have drifted from their files.
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = match &cli.store {
        Some(path) => BundleStore::open(path)?,
        None => BundleStore::open_default()?,
    };
    let now = chrono::Utc::now().to_rfc3339();

    match cli.command {
        Command::Render {
            source,
            out,
            title,
            theme,
        } => {
            let rendered = render::report(
                &source,
                &out,
                title.as_deref(),
                mecha_manifest::Theme::by_name(&theme),
            )?;
            // Run at render time too, so the cheap verb is where you find out.
            // Discovering it at publish — the expensive one, the one that costs
            // a review — would mean the review queue is where broken bundles
            // surface.
            vendor::gate_rendered(&rendered.dir, &source)?;
            println!("{} → {}", rendered.template, rendered.dir.display());
            println!("  title  {}", rendered.title);
            println!("  class  {}", rendered.class.as_str());
            println!("  open   {}", rendered.dir.join("index.html").display());
            // The title is in the hint because `publish` cannot recover it:
            // rendering and publishing are separate invocations by design, and
            // a person following this line without it gets a report titled
            // after its id.
            println!(
                "\nLook at it, then publish it:\n  \
                 factory-publish publish <id> {} --title {:?} --source {}",
                rendered.dir.display(),
                rendered.title,
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
            no_push,
            visibility,
        } => {
            let requested = parse_visibility(visibility.as_deref())?;
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
            // What the renderer wrote down, rather than what this command used
            // to assume: a notebook published as `static` reaches the artifact
            // origin, whose policy has no `wasm-unsafe-eval`, and cannot boot.
            let record = render::read_record(&bundle);
            vendor::gate_for_publish(&bundle, record.class)?;
            let published = store.publish(
                &id,
                &bundle,
                &title,
                description,
                &record.template,
                record.class,
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
            let visibility = requested.unwrap_or_else(|| {
                store
                    .alias(&id)
                    .ok()
                    .flatten()
                    .map(|a| a.visibility)
                    .unwrap_or(Visibility::Private)
            });
            if !no_alias || requested.is_some() {
                store.set_alias(&id, Some(published.version), visibility, &now)?;
                println!("  alias  → v{}", published.version);
            }
            let mut mirrored = None;
            if !no_push {
                // Local first, always. The store is the record; the box is a
                // copy of it that the world can read, and a push that fails
                // leaves the record intact and retryable.
                match remote::mirror(
                    &store,
                    &id,
                    published.version,
                    (!no_alias).then_some(published.version),
                    // `requested`, not the local fallback above: the store
                    // needs a concrete value and the box must be told only
                    // what somebody actually asked for.
                    requested,
                ) {
                    Ok(Some(at)) => {
                        println!("{}", at.columns());
                        mirrored = Some(at);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("\nthe box did not take it: {e:#}");
                        eprintln!(
                            "It is published locally as v{}. Retry with:\n  \
                             factory-publish push {id} --version {}",
                            published.version, published.version
                        );
                        std::process::exit(1);
                    }
                }
            }
            reach(&store, &id, mirrored.as_ref())?;
        }

        Command::Push {
            id,
            version,
            no_alias,
        } => {
            let version = match version {
                Some(version) => version,
                None => store
                    .alias(&id)?
                    .and_then(|a| a.version)
                    .or_else(|| store.versions(&id).ok().and_then(|v| v.last().copied()))
                    .ok_or_else(|| anyhow::anyhow!("{id} has no versions locally"))?,
            };
            // `push` has no `--visibility`, so it asserts none. It used to
            // read one out of the local store and send it, which is how a
            // bundle released publicly from the account page — a change the
            // local store never hears about — was silently taken down again
            // by the next push. Omitting the field is the box's own spelling
            // of "leave who may read it alone".
            match remote::mirror(&store, &id, version, (!no_alias).then_some(version), None)? {
                Some(at) => {
                    println!("{id} v{version}");
                    println!("{}", at.columns());
                    // The caveat used to be inline and conditional on the
                    // visibility this side asked for, which meant `--no-alias`
                    // — where nothing was asked and `serves` is `Unchanged` —
                    // printed a bare bytes URL with no caveat at all. It goes
                    // through `reach` now, like `publish` always did, so every
                    // state says something and no state has to be remembered
                    // in two places.
                    reach(&store, &id, Some(&at))?;
                }
                None => bail!(
                    "no factory is configured. Write ~/.mecha/factory/config.toml with \
                     `gate = \"https://…\"` and put a publish key in \
                     ~/.mecha/factory/publish.key"
                ),
            }
        }

        Command::Type(action) => type_command(action)?,

        Command::Connect {
            code,
            gate,
            handle,
            label,
            replace,
        } => connect_command(code, gate, handle, label, replace)?,

        Command::Operator { action } => operator_command(action)?,

        Command::Disconnect => println!("{}", remote::disconnect()?),

        Command::Drain {
            out,
            dry_run,
            json,
            wait,
        } => drain_command(out, dry_run, json, wait)?,
        Command::Slots(SlotsAction::Push {
            id,
            policy,
            dry_run,
        }) => slots_push_command(&id, &policy, dry_run)?,
        Command::Polls(action) => polls_command(action)?,
        Command::Profile(action) => record_command(Record::Profile, action)?,
        Command::Hangar(action) => record_command(Record::Hangar, action)?,
        Command::Switchboard(action) => switchboard_command(action)?,

        Command::Remote => match remote::Remote::configured()? {
            Some(remote) => {
                let health = remote.health()?;
                println!("gate      {}", remote.gate());
                println!("status    {}", health["status"].as_str().unwrap_or("?"));
                println!("version   {}", health["version"].as_str().unwrap_or("?"));
                if let Some(bundles) = health["bundles"].as_i64() {
                    println!("bundles   {bundles}");
                    println!("queued    {}", health["queued"].as_i64().unwrap_or(0));
                }
            }
            None => println!("no factory is configured (~/.mecha/factory/config.toml)"),
        },

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

        Command::Mcp { root } => mecha_factory_publish::mcp::serve(cli.store.clone(), root)?,

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

        Command::Alias {
            id,
            version,
            visibility,
        } => {
            let requested = parse_visibility(visibility.as_deref())?;
            let visibility = requested.unwrap_or_else(|| {
                store
                    .alias(&id)
                    .ok()
                    .flatten()
                    .map(|a| a.visibility)
                    .unwrap_or(Visibility::Private)
            });
            store.set_alias(&id, Some(version), visibility, &now)?;
            println!("{id} → v{version}");
            // `requested`: omitting `--visibility` means "move the alias and
            // leave who reads it alone", which is what the box does with an
            // absent field and what this command's own help promises.
            let mirrored = remote::mirror_alias(&id, Some(version), requested)?;
            if let Some(at) = &mirrored {
                println!("{}", at.columns());
            }
            // Printed before `reach`, because it is the correction to the line
            // already printed above: `{id} → v{version}` is a local fact, and
            // on a paired machine it is the *only* thing that happened.
            if let Some(stopped) = remote::alias_stopped_here(&id, Some(version))? {
                println!("  {stopped}");
            }
            reach(&store, &id, mirrored.as_ref())?;
        }

        Command::Unpublish { id } => {
            let existing = store.alias(&id)?;
            let before = existing.as_ref().and_then(|a| a.version);
            // The visibility is *kept*, not flipped to private, and that is
            // what decides what a reader sees. A public bundle taken down
            // answers 410 with "this has been taken down" — which is what
            // somebody who followed a link that used to work needs. Flipping it
            // would make every takedown answer 404 instead, so the honest page
            // would exist and never be reachable, and a bundle that was never
            // public would gain an existence oracle it does not have today.
            let visibility = existing
                .map(|a| a.visibility)
                .unwrap_or(Visibility::Private);
            store.set_alias(&id, None, visibility, &now)?;
            // The box first would be wrong the other way round: if this fails
            // half way, the state to be left in is "locally down, remotely up",
            // which the next `unpublish` fixes — not "locally up, remotely
            // down", which reads as done and is not.
            // `None`, which is precisely "keep who may read it" — the rule
            // the comment above states, now said to the box in its own terms
            // rather than by echoing a locally-read value back at it.
            remote::mirror_alias(&id, None, None)?;
            let stopped = remote::alias_stopped_here(&id, None)?;
            match (&stopped, before) {
                // The takedown reached the box: the unqualified sentence is
                // true and stays.
                (None, Some(v)) => println!("{id}: the share URL no longer resolves (was v{v})"),
                (None, None) => println!("{id}: already unpublished"),
                // It did not. Saying "no longer resolves" here would be the
                // one wrong answer that nobody goes back to check.
                (Some(why), Some(v)) => {
                    println!("{id}: taken down on this machine only (was v{v})");
                    println!("  {why}");
                }
                (Some(why), None) => {
                    println!("{id}: already unpublished here");
                    println!("  {why}");
                }
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
            reach(&store, &id, None)?;
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
/// `public`, `private`, or nothing — where nothing means "leave it alone".
///
/// Parsed rather than taken as a boolean flag, because `--public` and its
/// absence would make "make this private again" unsayable, and the one
/// direction that must always be expressible is the one that takes something
/// away from the world.
fn parse_visibility(text: Option<&str>) -> Result<Option<Visibility>> {
    match text {
        None => Ok(None),
        Some("public") => Ok(Some(Visibility::Public)),
        Some("private") => Ok(Some(Visibility::Private)),
        Some(other) => bail!("visibility `{other}` is not `public` or `private`"),
    }
}

/// Who can actually read this, said out loud.
///
/// Visibility used to be recorded and unenforced, and this line said so. It is
/// enforced now — the origin serves a private bundle to nobody, and answers
/// exactly what it answers for a bundle that never existed — so what this
/// prints depends on whether there is a box at all.
/// The `reach` line: who can actually read this, on the origin.
///
/// Takes the [`remote::Mirrored`] the box answered with when this command got
/// one, because the *local* store's visibility is not evidence about the
/// origin's — see the comment inside. `None` is for the read-only commands
/// that never asked.
fn reach(store: &BundleStore, id: &str, at: Option<&remote::Mirrored>) -> Result<()> {
    let visibility = store
        .alias(id)?
        .map(|a| a.visibility)
        .unwrap_or(Visibility::Private);
    let remote = remote::Remote::configured().ok().flatten();
    if remote.is_none() {
        println!(
            "  reach  whoever can read {} — no factory is configured, so it is \
             published locally and nowhere else",
            store.root().display()
        );
        return Ok(());
    }
    // What the box was actually told, when this command told it something.
    // The local visibility is not evidence about the origin: a machine with
    // only a publish key sets the local alias and never moves the box's, so
    // this line used to print "anyone with the link — it is public on the
    // origin" directly beneath a note saying the alias had not moved.
    match at.map(|at| at.serves) {
        Some(remote::Serves::Everyone) => {
            println!("  reach  anyone with the link — it is public on the origin")
        }
        Some(remote::Serves::OwnerOnly) => {
            println!(
                "  reach  nobody else: it is on the box and marked private, which \
                 the origin enforces by serving the bytes to no one but you."
            );
            // Named only when one was printed — `columns()` omits the page
            // line against a box that reported none, and pointing at a line
            // that is not there is how a reader ends up opening the bytes URL
            // expecting a page.
            match at.and_then(|at| at.page()) {
                Some(page) => println!("         Open it at {page}, signed in at the gate."),
                None => println!("         Open it from your account page at the gate."),
            }
            println!(
                "         `factory-publish alias {id} <version> --visibility public` \
                 publishes it."
            );
        }
        Some(remote::Serves::Nothing) => println!(
            "  reach  nobody: the share URL points at nothing. Every version is \
             still on the box."
        ),
        // Three ways to arrive at the same honest answer: the alias was not
        // touched, or it moved without anybody naming a visibility, or this
        // is a read-only command that never asked. In all three the origin
        // holds the answer and the local record is not evidence about it —
        // so it is offered as a parenthesis, never as the claim.
        Some(remote::Serves::AsBefore | remote::Serves::Unchanged) | None => println!(
            "  reach  whatever the alias on the box already said — this did not \
             change it{}",
            match visibility {
                Visibility::Public => " (locally it is marked public)",
                Visibility::Private => " (locally it is marked private)",
            }
        ),
    }
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

/// The request types the box serves forms from.
///
/// `check` before `push` is the same split as `render` before `publish`: one
/// is local and free, the other changes what the world can reach.
fn type_command(action: TypeAction) -> Result<()> {
    match action {
        TypeAction::Check { manifest, handle } => {
            let text = std::fs::read_to_string(&manifest)
                .with_context(|| format!("reading {}", manifest.display()))?;
            let parsed = mecha_manifest::RequestType::from_toml(&text)?;
            println!("{} — {}", parsed.id, parsed.title);
            println!("  version  {}", parsed.version);
            println!("  fields   {}", parsed.fields.len());
            let free: Vec<&str> = parsed.free_text_fields().map(|f| f.name.as_str()).collect();
            // Named rather than counted, because these are the fields a
            // stranger writes prose into — the ones the quarantine exists for,
            // and the ones a person editing a manifest should see listed back.
            println!(
                "  prose    {}",
                if free.is_empty() {
                    "none — every field is a choice, a date or a number".to_string()
                } else {
                    free.join(", ")
                }
            );
            match parsed.servable() {
                Ok(verification) => println!(
                    "  verify   {}, expiring after {}h",
                    verification.field, verification.expires_hours
                ),
                Err(e) => println!("  verify   NOT SERVABLE: {e}"),
            }
            if let Some(handle) = handle {
                println!(
                    "\nIt would be served at:\n  {}/f/{handle}/{}",
                    remote::Remote::configured()?
                        .map(|r| r.gate().to_string())
                        .unwrap_or_else(|| "https://<gate>".into()),
                    parsed.id
                );
            }
        }

        TypeAction::Push { manifest } => {
            let text = std::fs::read_to_string(&manifest)
                .with_context(|| format!("reading {}", manifest.display()))?;
            // Parsed here as well as at the far end. The box would reject a bad
            // manifest anyway, but a round trip is a slow way to learn about a
            // typo, and the local copy below must never be one the box refused.
            let parsed = mecha_manifest::RequestType::from_toml(&text)?;
            parsed.servable().with_context(|| {
                format!(
                    "`{}` cannot be served as a form, so pushing it would publish \
                     something nobody can submit",
                    parsed.id
                )
            })?;

            // Release-scoped, like the alias: pushing a type makes a form live
            // at a public URL that strangers can submit to. That is a
            // publication, not bookkeeping, and it is not an agent's to make.
            let Some(remote) = remote::Remote::configured_for(remote::Scope::Release)? else {
                bail!(
                    "no factory is configured, or there is no release key. Write \
                     ~/.mecha/factory/config.toml with `gate = \"https://…\"` and put a \
                     release key in ~/.mecha/factory/release.key — serving a form is a \
                     publication, so it needs one"
                );
            };
            let body = remote.type_push(&text, &parsed.id)?;

            // Only after the box accepted it. The local copy is what a later
            // drain validates against, so it must be the manifest actually
            // being served — not one that failed on the way there.
            let local = mecha_factory_publish::requests::remember_type(&text, &parsed.id)?;

            println!(
                "{} — {} field(s) — is live at",
                body["id"].as_str().unwrap_or(&parsed.id),
                body["fields"]
                    .as_i64()
                    .unwrap_or(parsed.fields.len() as i64),
            );
            println!("  {}", form_url(remote.gate(), &parsed.id));
            println!(
                "\nkept locally at {} so a drain can validate against it",
                local.display()
            );
        }

        TypeAction::List => {
            let Some(remote) = remote::Remote::configured()? else {
                println!("no factory is configured (~/.mecha/factory/config.toml)");
                return Ok(());
            };
            let body = remote.type_list()?;
            let types = body["types"].as_array().cloned().unwrap_or_default();
            if types.is_empty() {
                println!(
                    "the box is serving no forms. Push one:\n  \
                     factory-publish type push <manifest.toml>"
                );
                return Ok(());
            }
            for t in &types {
                println!(
                    "{:<20} {}",
                    t["id"].as_str().unwrap_or("?"),
                    t["title"].as_str().unwrap_or("")
                );
            }
        }
    }
    Ok(())
}

/// Best-effort form URL. The handle is the user's, and the box is the authority
/// on it, so this is a convenience rather than a claim.
fn form_url(gate: &str, type_id: &str) -> String {
    format!("{gate}/f/<handle>/{type_id}")
}

/// Take what the box has verified, write it down, then acknowledge it.
///
/// The order is the whole correctness of this command, and it is written out
/// in `requests.rs`: acknowledging is what deletes, so it happens after the
/// bytes are on disk here. A record whose acknowledgement is lost comes back on
/// the next drain and is recognised by its sequence number.
fn operator_command(action: OperatorAction) -> Result<()> {
    let Some(remote) = remote::Remote::configured_for(remote::Scope::Operate)? else {
        bail!(
            "no gate configured — `factory-publish connect` a machine first, or \
             set FACTORY_GATE. The operate key is minted on the box: \
             `factory key create --scope operate --label <where it lives>`."
        );
    };
    match action {
        OperatorAction::Signin => {
            let body = remote.json("POST", "/v1/admin/signin", Some(&serde_json::json!({})))?;
            println!("{}", body["url"].as_str().unwrap_or("?"));
            eprintln!(
                "open it in a browser — it works once and expires {}",
                body["expires_at"].as_str().unwrap_or("soon"),
            );
            Ok(())
        }
        OperatorAction::Users => {
            let body = remote.json("GET", "/v1/admin/users", None)?;
            for user in body["users"].as_array().into_iter().flatten() {
                println!(
                    "{:<20} {:<10} {:<28} queued {:>3}  quota {}",
                    user["handle"].as_str().unwrap_or("?"),
                    user["status"].as_str().unwrap_or("?"),
                    user["email"].as_str().unwrap_or(""),
                    user["queued"].as_i64().unwrap_or(0),
                    user["quota_bytes"].as_i64().unwrap_or(0),
                );
            }
            Ok(())
        }
        OperatorAction::Suspend { ref handle } | OperatorAction::Restore { ref handle } => {
            // One arm: the verb is the status.
            let status = if matches!(action, OperatorAction::Suspend { .. }) {
                "suspended"
            } else {
                "active"
            };
            let handle = handle.clone();
            let body = remote.json(
                "POST",
                &format!("/v1/admin/users/{handle}/status"),
                Some(&serde_json::json!({ "status": status })),
            )?;
            println!(
                "{} is now {}",
                body["handle"].as_str().unwrap_or(&handle),
                body["status"].as_str().unwrap_or(status)
            );
            Ok(())
        }
        OperatorAction::Invite { email, note } => {
            let body = remote.json(
                "POST",
                "/v1/admin/invites",
                Some(&serde_json::json!({ "email": email, "note": note })),
            )?;
            println!("{}", body["link"].as_str().unwrap_or("?"));
            eprintln!(
                "invite {} for {email}: expires {}, mailed via {}",
                body["id"].as_str().unwrap_or("?"),
                body["expires_at"].as_str().unwrap_or("?"),
                body["mailed_via"].as_str().unwrap_or("?"),
            );
            Ok(())
        }
        OperatorAction::Invites => {
            let body = remote.json("GET", "/v1/admin/invites", None)?;
            for invite in body["invites"].as_array().into_iter().flatten() {
                println!(
                    "{}  {:<28} {:<8} expires {}{}",
                    invite["id"].as_str().unwrap_or("?"),
                    invite["email"].as_str().unwrap_or(""),
                    invite["status"].as_str().unwrap_or("?"),
                    invite["expires_at"].as_str().unwrap_or("?"),
                    match invite["note"].as_str() {
                        Some("") | None => String::new(),
                        Some(note) => format!("  ({note})"),
                    },
                );
            }
            Ok(())
        }
        OperatorAction::RevokeInvite { id } => {
            let body = remote.json(
                "POST",
                &format!("/v1/admin/invites/{id}/revoke"),
                Some(&serde_json::json!({})),
            )?;
            println!(
                "{}",
                if body["revoked"].as_bool().unwrap_or(false) {
                    "revoked — the link no longer works"
                } else {
                    "not a pending invite (claimed, already revoked, or unknown)"
                }
            );
            Ok(())
        }
        OperatorAction::Keys => {
            let body = remote.json("GET", "/v1/admin/keys", None)?;
            for key in body["keys"].as_array().into_iter().flatten() {
                println!(
                    "{}  {:<12} {:<8} {:<16} minted {}  last used {}{}",
                    key["id"].as_str().unwrap_or("?"),
                    key["handle"].as_str().unwrap_or("?"),
                    key["scope"].as_str().unwrap_or("?"),
                    key["label"].as_str().unwrap_or(""),
                    key["created_at"].as_str().unwrap_or("?"),
                    key["last_used_at"].as_str().unwrap_or("never"),
                    match key["revoked_at"].as_str() {
                        Some(at) => format!("  REVOKED {at}"),
                        None => String::new(),
                    },
                );
            }
            Ok(())
        }
        OperatorAction::RevokeKey { id } => {
            let body = remote.json(
                "POST",
                &format!("/v1/admin/keys/{id}/revoke"),
                Some(&serde_json::json!({})),
            )?;
            println!(
                "{}",
                if body["revoked"].as_bool().unwrap_or(false) {
                    "revoked"
                } else {
                    "not a live key"
                }
            );
            Ok(())
        }
        OperatorAction::Withhold {
            handle,
            id,
            version,
            reason,
            undo,
        } => {
            remote.json(
                "POST",
                "/v1/admin/withhold",
                Some(&serde_json::json!({
                    "handle": handle, "id": id, "version": version,
                    "reason": reason, "undo": undo,
                })),
            )?;
            if undo {
                println!("{handle}/{id} v{version} is served again");
            } else {
                println!("{handle}/{id} v{version} is withheld — served to nobody, still on disk");
            }
            Ok(())
        }
    }
}

/// What to call this machine in the box's key ledger.
///
/// `/etc/hostname` is a Linux file, and reading only it meant every macOS
/// pairing arrived with an empty label — which is not cosmetic: the ledger is
/// how a person recognises which machine to revoke, and "the blank one" is not
/// a recognition. The first external tenant paired from a Mac and produced
/// exactly that row. `hostname` is in POSIX and answers on both; the file
/// stays first because it needs no subprocess.
fn machine_label() -> String {
    if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if let Ok(name) = String::from_utf8(out.stdout) {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    // A label is free text for a human's benefit, so an unnameable machine
    // gets something recognisable rather than a refusal.
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unnamed machine".to_string())
}

fn connect_command(
    code: String,
    gate: Option<String>,
    handle: Option<String>,
    label: Option<String>,
    replace: bool,
) -> Result<()> {
    let gate = match gate.or_else(|| std::env::var("FACTORY_GATE").ok().filter(|g| !g.is_empty())) {
        Some(gate) => gate,
        None => bail!("no gate named — pass --gate https://gate.example.org"),
    };

    // The assertion. Interactive, the person types the handle they expect —
    // that *is* the confirmation, and there is no y/N to wave through.
    // Non-interactive, --handle carries it, so an agent running this on
    // somebody's instruction states whose machine this is becoming. Either
    // way the server checks it, and a mismatch spends nothing.
    let handle = match handle {
        Some(handle) => handle,
        None => {
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() {
                bail!(
                    "no terminal to ask on — pass --handle <yours>. The handle \
                     is checked by the server, and a code that pairs somewhere \
                     you did not assert is refused."
                );
            }
            eprint!("Type the handle this machine should publish for: ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let typed = line.trim().to_string();
            if typed.is_empty() {
                bail!("nothing typed, nothing paired");
            }
            typed
        }
    };

    let label = label.unwrap_or_else(machine_label);

    let summary = remote::connect(&remote::Connect {
        gate: &gate,
        code: &code,
        handle: &handle,
        label: &label,
        replace,
    })?;
    println!("{summary}");
    Ok(())
}

/// Bring one record's files home: fetch what is missing, verify every digest
/// against the drained metadata, and write with the store's own discipline.
/// A path that already exists is trusted — it was digest-verified when it was
/// written — so the repair path costs nothing when there is nothing to
/// repair.
fn fetch_blobs(
    remote: &remote::Remote,
    store: &mecha_factory_publish::requests::RequestStore,
    record: &mecha_factory_publish::requests::Record,
) -> Result<()> {
    for att in &record.attachments {
        if store.attachment_path(&att.path).exists() {
            continue;
        }
        let bytes = remote.fetch_attachment(&att.id, att.size)?;
        use sha2::Digest;
        let digest = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
        anyhow::ensure!(
            digest == att.sha256,
            "field `{}`: the bytes hash to {digest}, not the {} the box declared",
            att.field,
            att.sha256
        );
        store.write_attachment(&att.path, &bytes)?;
    }
    Ok(())
}

fn drain_command(out: Option<PathBuf>, dry_run: bool, json: bool, wait: u64) -> Result<()> {
    use mecha_factory_publish::requests::{record_from, RequestStore};

    let Some(remote) = remote::Remote::configured_for(remote::Scope::Drain)? else {
        bail!(
            "no factory is configured. Write ~/.mecha/factory/config.toml with \
             `gate = \"https://…\"` and put a drain key in ~/.mecha/factory/drain.key"
        );
    };

    // Always from zero. Acknowledgement is what removes a record, so a cursor
    // would only add a second idea of what home holds, and one that can drift
    // ahead of what was actually written.
    let body = remote.drain(0, wait)?;
    let rows = body["records"].as_array().cloned().unwrap_or_default();

    if dry_run {
        let summary = serde_json::json!({
            "drained": rows.len(),
            "written": 0,
            "acknowledged": 0,
            "dry_run": true,
        });
        if json {
            println!("{summary}");
        } else if rows.is_empty() {
            println!("nothing waiting");
        } else {
            println!("{} record(s) waiting; nothing written", rows.len());
            for row in &rows {
                println!(
                    "  seq {:<5} {:<20} {}",
                    row["seq"].as_i64().unwrap_or(0),
                    row["type"].as_str().unwrap_or("?"),
                    row["created_at"].as_str().unwrap_or("")
                );
            }
        }
        return Ok(());
    }

    let store = match out {
        Some(path) => RequestStore::open(path)?,
        None => RequestStore::open_default()?,
    };
    let now = chrono::Utc::now().to_rfc3339();

    let mut written = Vec::new();
    let mut already = 0usize;
    let mut invalid = Vec::new();
    // Every row that reached disk — including one that was already there,
    // which is a lost acknowledgement rather than a new record and must be
    // acknowledged again or it returns forever.
    let mut safe = Vec::new();

    for row in &rows {
        let record = record_from(row, &now);
        // Blobs before the record, and both before the ack. The ack destroys
        // the box's copies of the files along with the row, so the invariant
        // that carries this loop is: **a record on disk implies its blobs on
        // disk.** A fetch that fails keeps the whole record off the ack list
        // and it all comes back next drain. This also repairs the
        // already-held path — a record written by an older binary, or a blob
        // someone deleted by hand, is refetched before the seq is
        // acknowledged again.
        if let Err(e) = fetch_blobs(&remote, &store, &record) {
            eprintln!(
                "failed to fetch the files of seq {}: {e:#}; the record stays on the box",
                record.seq
            );
            continue;
        }
        match store.write(&record) {
            Ok(true) => {
                if !record.valid {
                    invalid.push(record.clone());
                }
                safe.push(record.seq);
                written.push(record);
            }
            Ok(false) => {
                already += 1;
                safe.push(record.seq);
            }
            // Not acknowledged, so it stays on the box and comes back. A disk
            // that is full must not be the way a request disappears.
            Err(e) => eprintln!("failed to store seq {}: {e:#}", record.seq),
        }
    }

    let acknowledged = if safe.is_empty() {
        0
    } else {
        remote.ack(&safe)?
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "drained": rows.len(),
                "written": written.len(),
                "already_held": already,
                "invalid": invalid.len(),
                "acknowledged": acknowledged,
                "store": store.root().display().to_string(),
            })
        );
        return Ok(());
    }

    if rows.is_empty() {
        println!("nothing waiting");
        return Ok(());
    }
    println!(
        "{} new, {} already held, {} acknowledged → {}",
        written.len(),
        already,
        acknowledged,
        store.root().display()
    );
    for record in &written {
        println!(
            "  seq {:<5} {:<20} {}",
            record.seq,
            record.type_id,
            if record.valid { "" } else { "INVALID" }
        );
    }
    for record in &invalid {
        // Loud, because an invalid record is a bug on one of the two machines
        // and the box has now deleted its copy.
        eprintln!(
            "\nseq {} did not validate here: {}",
            record.seq,
            record.invalid_reason.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

/// Compute an instrument's slots from stdin's busy intervals and replace its
/// cache on the box. Everything here fails closed: partial knowledge of the
/// calendar must never become "free" on a public page.
fn slots_push_command(id: &str, policy_path: &PathBuf, dry_run: bool) -> Result<()> {
    use mecha_factory_publish::availability::{self, Interval};

    let policy_text = std::fs::read_to_string(policy_path)
        .with_context(|| format!("reading {}", policy_path.display()))?;
    let policy = availability::Policy::from_toml(&policy_text)
        .with_context(|| format!("{}", policy_path.display()))?;

    let stdin = std::io::read_to_string(std::io::stdin()).context("reading stdin")?;
    anyhow::ensure!(
        !stdin.trim().is_empty(),
        "nothing on stdin — pipe `mecha-mail freebusy --json` into this"
    );
    #[derive(serde::Deserialize)]
    struct FreebusyDoc {
        generated_at: String,
        time_min: String,
        time_max: String,
        busy: Vec<Interval>,
    }
    let doc: FreebusyDoc =
        serde_json::from_str(&stdin).context("stdin is not `mecha-mail freebusy --json` output")?;

    let stamp = |raw: &str| -> Result<chrono::DateTime<chrono::Utc>> {
        Ok(chrono::DateTime::parse_from_rfc3339(raw)
            .with_context(|| format!("`{raw}` in the freebusy document"))?
            .with_timezone(&chrono::Utc))
    };
    let generated_at = stamp(&doc.generated_at)?;
    let time_min = stamp(&doc.time_min)?;
    let time_max = stamp(&doc.time_max)?;

    // Everything downstream is anchored to the freebusy document's own
    // stamp, never this process's clock. Two clocks a second apart made
    // `--days 60` structurally unable to satisfy a 60-day horizon (found by
    // running the documented pipeline, not by reading it) — and the anchor
    // buys the property the timer unit promises anyway: the same input
    // produces the same slots. Staleness of the anchor is bounded first.
    anyhow::ensure!(
        chrono::Utc::now() - generated_at <= chrono::Duration::hours(1),
        "this freebusy answer is from {generated_at}, over an hour old — refusing to \
         push stale availability; re-run the pipeline"
    );

    // The busy data has to cover everything the policy would offer. Data
    // that stops short of the horizon says nothing about the far weeks, and
    // "nothing known" rendered as "nothing busy" is exactly the lie this
    // refuses to tell.
    let horizon_end = generated_at + chrono::Duration::days(i64::from(policy.horizon_days));
    anyhow::ensure!(
        time_max >= horizon_end,
        "the freebusy window ends {time_max}, short of the {}-day horizon ({horizon_end}) — \
         run `mecha-mail freebusy --days {}` or more",
        policy.horizon_days,
        policy.horizon_days
    );
    anyhow::ensure!(
        time_min <= generated_at,
        "the freebusy window starts at {time_min}, after its own generated_at — \
         the busy data must cover the present"
    );

    // Open meeting polls hold their candidates (SCHEDULING-DESIGN.md §5.5):
    // the page must not sell a slot a poll is still asking about. Bookings
    // from the local ledger arrive with a later build step.
    let holds = mecha_factory_publish::lifecycle::open_holds()?;
    let slots = availability::availability(&policy, &doc.busy, &holds, &[], generated_at);

    if dry_run {
        println!(
            "{} slot(s) from {} busy interval(s), horizon {} days",
            slots.len(),
            doc.busy.len(),
            policy.horizon_days
        );
        for slot in &slots {
            let local = slot.start.with_timezone(&policy.timezone);
            println!(
                "  {}  {:>3} min",
                local.format("%a %Y-%m-%d %H:%M %Z"),
                slot.duration_minutes
            );
        }
        println!("\n(dry run: nothing pushed)");
        return Ok(());
    }

    let Some(remote) = remote::Remote::configured_for(remote::Scope::Slots)? else {
        bail!(
            "no factory gate configured — write `gate = \"https://…\"` to \
             ~/.mecha/factory/config.toml and install slots.key beside it"
        );
    };
    let payload = serde_json::json!({
        "generated_at": doc.generated_at,
        "horizon_days": policy.horizon_days,
        "slots": slots,
    });
    let reply = remote.slots_push(id, &payload)?;
    // What this tick saw, kept: a meeting poll created without its own
    // freebusy seeds from here — including one released hours after it was
    // staged, which is the case a path to a file could never serve.
    mecha_factory_publish::lifecycle::remember_freebusy(
        id,
        &policy_path.display().to_string(),
        &policy_text,
        &stdin,
    )?;
    println!(
        "pushed {} slot(s) for `{id}` (generated {}){}",
        reply["stored"].as_u64().unwrap_or(slots.len() as u64),
        doc.generated_at,
        if holds.is_empty() {
            String::new()
        } else {
            format!(", {} slot(s) held by open polls", holds.len())
        }
    );
    Ok(())
}

/// One tick of every open meeting poll. Each record is advanced on its own,
/// so one unreachable poll never stalls the rest; what could not be read is
/// printed as a finding, never skipped as if absent.
fn sweep_command() -> Result<()> {
    use mecha_factory_publish::lifecycle::{self, Observed};
    use mecha_factory_publish::polls::{self, Status};

    let (records, problems) = lifecycle::records()?;
    for problem in &problems {
        eprintln!("unreadable: {problem}");
    }
    let now = chrono::Utc::now();
    let mut touched = 0usize;
    for mut record in records {
        // Retired once the box is settled — unless the record still holds
        // slots off the booking page, in which case the stall bound must
        // keep running until the event exists or the hold is released.
        if record.lifecycle.box_closed_at.is_some() && !record.lifecycle.holds_slots() {
            continue;
        }
        let status = match polls::status(&record.instrument, &record.poll_id) {
            Ok(status) => status,
            Err(e) => {
                eprintln!("{}: the box did not answer — {e:#}", record.poll_id);
                continue;
            }
        };
        let Status::Times {
            state,
            resolution,
            responded,
            total,
            ranked,
            silent,
            answers,
            ..
        } = status
        else {
            eprintln!("{}: not a meeting poll on the box; skipped", record.poll_id);
            continue;
        };
        let seen = Observed {
            state,
            resolution,
            responded,
            total,
            silent,
            ranked: ranked
                .iter()
                .map(|r| mecha_manifest::RankedCandidate {
                    start: r.start,
                    duration_minutes: r.duration_minutes,
                    yes: r.yes,
                    if_needed: r.if_needed,
                    no: r.no,
                    feasible: r.feasible,
                    unanimous: r.unanimous,
                })
                .collect(),
            answers,
        };
        let before = record.lifecycle.clone();
        let advanced = lifecycle::advance(&mut record.lifecycle, &seen, now);
        if let Some(sentence) = &advanced.close_box_with {
            match polls::close(&record.instrument, &record.poll_id, Some(sentence)) {
                Ok(true) => record.lifecycle.box_closed_at = Some(now),
                // `false` is the box saying it was already closed — by hand,
                // since the tally — and that it keeps the resolution written
                // then. Not a success, and not ours to report as one.
                Ok(false) => {
                    let line = lifecycle::box_refused_close(&mut record.lifecycle, now);
                    println!("{}: {line}", record.poll_id);
                }
                Err(e) => eprintln!("{}: could not close on the box — {e:#}", record.poll_id),
            }
        }
        if record.lifecycle != before {
            // A write that fails is this record's failure, reported and
            // retried next tick — never the reason a later poll goes
            // un-advanced.
            match lifecycle::save(&mut record, &before) {
                Ok(()) => touched += 1,
                Err(e) => {
                    eprintln!("{}: could not write the record — {e:#}", record.poll_id);
                    continue;
                }
            }
        }
        for line in &advanced.lines {
            println!("{}: {line}", record.poll_id);
        }
    }
    println!("swept: {touched} record(s) changed");
    if problems.is_empty() {
        Ok(())
    } else {
        bail!("{} record(s) could not be read", problems.len())
    }
}

/// The person's front end onto `polls`. Everything below this line is
/// formatting: the verbs themselves live in the library, where the MCP server
/// can reach them too. See that module's docs for why that is a rule and not a
/// preference.
fn polls_command(action: PollsAction) -> Result<()> {
    use mecha_factory_publish::polls::{self, Status};

    match action {
        PollsAction::Sweep => sweep_command()?,

        PollsAction::Create {
            instrument,
            poll_id,
            spec,
            policy,
            title,
            duration,
            participants,
            roster,
            deadline,
            earliest,
            latest,
            message,
            account,
            max_candidates,
        } => {
            let roster_text = match &roster {
                Some(path) => Some(
                    std::fs::read_to_string(path)
                        .with_context(|| format!("reading {}", path.display()))?,
                ),
                None => None,
            };
            let named = polls::participants(&participants, roster_text.as_deref())?;

            let created = match spec {
                Some(spec_path) => {
                    let text = std::fs::read_to_string(&spec_path)
                        .with_context(|| format!("reading {}", spec_path.display()))?;
                    polls::create_general(&instrument, &poll_id, &text, &named)
                        .with_context(|| format!("{}", spec_path.display()))?
                }
                None => {
                    let (Some(title), Some(duration)) = (title, duration) else {
                        bail!("clap enforces --title/--duration unless --spec is given");
                    };
                    // With --policy, stdin is the pipeline contract `slots
                    // push` uses; without it, the pipeline's own last input.
                    let (policy_text, freebusy) = match &policy {
                        Some(path) => {
                            let text = std::fs::read_to_string(path)
                                .with_context(|| format!("reading {}", path.display()))?;
                            let stdin = std::io::read_to_string(std::io::stdin())
                                .context("reading stdin")?;
                            let freebusy = polls::Freebusy::parse(&stdin)
                                .context("stdin is not `mecha-mail freebusy --json` output")?;
                            (Some(text), Some(freebusy))
                        }
                        None => {
                            // Busy time piped in without the policy it goes
                            // with is half an input: refused, as the library
                            // refuses it, rather than silently replaced by
                            // the pipeline's cache.
                            use std::io::IsTerminal;
                            if !std::io::stdin().is_terminal() {
                                let stdin = std::io::read_to_string(std::io::stdin())
                                    .context("reading stdin")?;
                                anyhow::ensure!(
                                    stdin.trim().is_empty(),
                                    "freebusy on stdin without --policy — pass both, or \
                                     neither to use the pipeline's last input"
                                );
                            }
                            (None, None)
                        }
                    };
                    polls::create_meeting(
                        Some(&instrument),
                        &polls::MeetingRequest {
                            title: &title,
                            duration,
                            poll_id: Some(&poll_id),
                            deadline: deadline.as_deref(),
                            earliest: earliest.as_deref(),
                            latest: latest.as_deref(),
                            max_candidates,
                        },
                        policy_text.as_deref(),
                        freebusy,
                        &named,
                        &polls::Invite {
                            message: message.as_deref(),
                            account: account.as_deref(),
                            ..polls::Invite::default()
                        },
                    )?
                }
            };
            print_created(&created);
        }

        PollsAction::Status {
            instrument,
            poll_id,
            json,
        } => match polls::status(&instrument, &poll_id)? {
            status @ Status::Times { .. } if json => println!("{}", times_json(&status)),
            status @ Status::General { .. } if json => println!("{}", general_json(&status)),
            status @ Status::Times { .. } => print_times(&status),
            status @ Status::General { .. } => print_general(&status),
        },

        PollsAction::Close {
            instrument,
            poll_id,
            resolution,
        } => {
            if polls::close(&instrument, &poll_id, resolution.as_deref())? {
                match resolution {
                    Some(_) => println!(
                        "poll `{poll_id}` closed; answers are frozen and the \
                         outcome is on the page"
                    ),
                    None => println!("poll `{poll_id}` closed; answers are frozen"),
                }
            } else {
                println!(
                    "poll `{poll_id}` was already closed — a resolution does \
                     not overwrite one written at close"
                );
            }
        }

        PollsAction::Export {
            instrument,
            poll_id,
            out,
        } => {
            let (csv, ballots) = polls::export(&instrument, &poll_id)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &csv)
                        .with_context(|| format!("writing {}", path.display()))?;
                    println!("{ballots} ballot(s) → {}", path.display());
                }
                None => print!("{csv}"),
            }
        }

        PollsAction::Addin { out } => {
            let Some(remote) = remote::Remote::configured_for(remote::Scope::Slots)? else {
                bail!(
                    "no factory gate configured — write `gate = \"https://…\"` to \
                     ~/.mecha/factory/config.toml and install slots.key beside it"
                );
            };
            let gate = remote.gate().to_string();
            anyhow::ensure!(
                gate.starts_with("https://"),
                "Office loads add-ins over HTTPS only — the gate is `{gate}`"
            );
            let path = out.unwrap_or_else(|| PathBuf::from("mecha-polls.xml"));
            std::fs::write(&path, mecha_factory_publish::slides::addin_manifest(&gate))
                .with_context(|| format!("writing {}", path.display()))?;
            println!("manifest → {}\n", path.display());
            println!(
                "{}",
                mecha_factory_publish::slides::sideload_instructions(&path.display().to_string())
            );
        }
    }
    Ok(())
}

fn print_created(created: &mecha_factory_publish::polls::Created) {
    use mecha_factory_publish::polls::Created;
    match created {
        Created::Link {
            poll_id,
            questions,
            max_ballots,
            url,
            screen_url,
            ..
        } => {
            println!(
                "poll `{poll_id}`: {questions} question(s), open link, capped at {} ballot(s)",
                max_ballots.unwrap_or(0)
            );
            println!("  {url}");
            if let Some(screen) = screen_url {
                println!("projector: {screen}");
            }
            println!(
                "\nOne vote per person is a cookie and an honor system — the page \
                 says so. Post the link where the audience already is."
            );
        }
        Created::Roster {
            poll_id,
            questions,
            people,
            screen_url,
            record,
            links_csv,
            ..
        } => {
            println!(
                "poll `{poll_id}`: {questions} question(s), {} participant(s)",
                people.len()
            );
            if people.len() <= 12 {
                print_invites(people);
            } else {
                println!("  (a class section — the links live in the CSV)");
            }
            println!("\nrecord: {}", record.display());
            println!(
                "links:  {}  ← one row per person, for the mail-merge",
                links_csv.display()
            );
            if let Some(screen) = screen_url {
                println!("projector: {screen}  ← aggregates only; prose stays off the wall");
            }
            println!("Send each person their own link — it is their identity on the poll.");
        }
        Created::Times {
            poll_id,
            candidates,
            first,
            last,
            deadline_local,
            people,
            record,
            ..
        } => {
            println!(
                "poll `{poll_id}`: {candidates} candidate time(s) from {first} to {last}, \
                 {} participant(s); answers close {deadline_local}",
                people.len()
            );
            println!("record: {}", record.display());
            println!(
                "The invitations are queued for `mecha-mail polls`, which sends each person \
                 their own link on its timer; `polls sweep` carries the poll from there. \
                 Neither has run yet — this minted the poll on the box and wrote the record."
            );
        }
    }
}

fn print_invites(people: &[mecha_factory_publish::polls::Invited]) {
    for person in people {
        let url = if person.url.is_empty() {
            "(no url?)"
        } else {
            &person.url
        };
        println!("  {} <{}>\n    {url}", person.name, person.email);
    }
}

/// The typed verdict an agent workflow reads. Unchanged in shape from before
/// polls moved into the library — `polls status --json` is consumed elsewhere.
fn times_json(status: &mecha_factory_publish::polls::Status) -> serde_json::Value {
    use mecha_factory_publish::polls::Status;
    let Status::Times {
        poll_id,
        state,
        resolution,
        responded,
        total,
        ranked,
        clean_winner,
        silent,
        ..
    } = status
    else {
        unreachable!("times_json is only called on a times poll")
    };
    // Three worlds, kept apart: a lifecycle, none (a general poll, or a
    // record from before one existed), and a record that is on disk but
    // could not be read — which must not render as the benign second.
    let lifecycle = match mecha_factory_publish::lifecycle::record(poll_id) {
        Ok(Some(r)) => mecha_factory_publish::lifecycle::describe(&r.lifecycle),
        Ok(None) => serde_json::Value::Null,
        Err(e) => serde_json::json!({ "unreadable": format!("{e:#}") }),
    };
    serde_json::json!({
        "poll": poll_id,
        "state": state,
        "resolution": resolution,
        "responded": responded,
        "total": total,
        "silent": silent,
        "lifecycle": lifecycle,
        "ranked": ranked.iter().map(|r| serde_json::json!({
            "start": r.start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "duration_minutes": r.duration_minutes,
            "yes": r.yes, "if_needed": r.if_needed, "no": r.no,
            "feasible": r.feasible, "unanimous": r.unanimous,
        })).collect::<Vec<_>>(),
        "clean_winner": clean_winner.map(|(start, duration)| serde_json::json!({
            "start": start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "duration_minutes": duration,
        })),
    })
}

/// Per-question tallies, with text answers in their own clearly-marked prose
/// section — never mixed into the typed one. This is the *person's* view and it
/// carries the words; what a run with tools may see is
/// `Status::for_privileged_run`, which does not.
fn general_json(status: &mecha_factory_publish::polls::Status) -> serde_json::Value {
    use mecha_factory_publish::polls::Status;
    let Status::General {
        poll_id,
        state,
        resolution,
        responded,
        total,
        tallies,
        prose,
        ..
    } = status
    else {
        unreachable!("general_json is only called on a general poll")
    };
    serde_json::json!({
        "poll": poll_id,
        "state": state,
        "resolution": resolution,
        "responded": responded,
        "total": total,
        "tallies": tallies,
        "prose": prose,
    })
}

fn print_times(status: &mecha_factory_publish::polls::Status) {
    use mecha_factory_publish::polls::Status;
    let Status::Times {
        poll_id,
        state,
        responded,
        total,
        ranked,
        clean_winner,
        silent,
        ..
    } = status
    else {
        unreachable!("print_times is only called on a times poll")
    };
    println!("poll `{poll_id}` ({state}): {responded} of {total} answered");
    if !silent.is_empty() {
        println!("  silent: {}", silent.join(", "));
    }
    match mecha_factory_publish::lifecycle::record(poll_id) {
        Ok(Some(record)) => println!("  lifecycle: {}", record.lifecycle.summary()),
        Ok(None) => {}
        Err(e) => println!("  lifecycle: record unreadable — {e:#}"),
    }
    for r in ranked.iter().take(5) {
        println!(
            "  {}  {:>3} min   yes {}  if-needed {}  no {}{}",
            r.start.format("%a %Y-%m-%d %H:%M UTC"),
            r.duration_minutes,
            r.yes,
            r.if_needed,
            r.no,
            if r.unanimous { "   ← unanimous" } else { "" },
        );
    }
    match clean_winner {
        Some((start, _)) => println!(
            "\nclean winner: {} — book it and close the poll",
            start.format("%a %Y-%m-%d %H:%M UTC")
        ),
        None => println!(
            "\nno clean winner{} — judgment call: pick from the ranking above",
            if responded < total {
                " yet (answers outstanding)"
            } else {
                ""
            }
        ),
    }
}

fn print_general(status: &mecha_factory_publish::polls::Status) {
    use mecha_factory_publish::polls::Status;
    use mecha_manifest::{
        tally_choice, tally_likert, tally_ranking, tally_vas, Answer, QuestionKind,
    };
    let Status::General {
        poll_id,
        state,
        resolution,
        responded,
        total,
        spec,
        ballots,
        ..
    } = status
    else {
        unreachable!("print_general is only called on a general poll")
    };
    println!("poll `{poll_id}` ({state}): {responded} of {total} answered");
    if let Some(resolution) = resolution {
        println!("outcome: {resolution}");
    }
    let closed = state == "closed";
    for question in &spec.questions {
        let prompt = question.prompt.as_deref().unwrap_or(&spec.title);
        println!("\n{prompt}");
        let answers: Vec<Answer> = ballots
            .iter()
            .filter_map(|b| b.get(&question.id).cloned())
            .collect();
        match &question.kind {
            QuestionKind::Choice { options, .. } => {
                for (id, count) in tally_choice(options, &answers).counts {
                    let label = options
                        .iter()
                        .find(|o| o.id == id)
                        .map(|o| o.label.as_str())
                        .unwrap_or(&id);
                    println!("  {count:>3}  {label}");
                }
            }
            QuestionKind::Ranking { options } => {
                let ranked = tally_ranking(options, &answers);
                for (id, count) in &ranked.first_preferences {
                    let label = options
                        .iter()
                        .find(|o| &o.id == id)
                        .map(|o| o.label.as_str())
                        .unwrap_or(id);
                    println!("  {count:>3}  {label}   (first preferences)");
                }
                if closed {
                    match &ranked.winner {
                        Some(winner) => println!("  winner by instant runoff: {winner}"),
                        None => println!("  no runoff winner"),
                    }
                } else {
                    println!("  (runoff runs when the poll closes)");
                }
            }
            QuestionKind::Likert { points, .. } => {
                let t = tally_likert(*points, &answers);
                println!("  distribution 1..={points}: {:?}", t.counts);
                if let (Some(median), Some(mean)) = (t.median, t.mean) {
                    println!("  n {}  median {median}  mean {mean:.2}", t.n);
                }
            }
            QuestionKind::Vas { .. } => {
                let t = tally_vas(&answers);
                if let (Some(median), Some(mean)) = (t.median, t.mean) {
                    println!("  n {}  median {median}  mean {mean:.1}  (0–100)", t.n);
                } else {
                    println!("  no answers yet");
                }
            }
            QuestionKind::Text { .. } => {
                // A person reading a stranger's prose in a terminal is the safe
                // context — the front door's `show` rule.
                for answer in &answers {
                    if let Answer::Text(text) = answer {
                        println!("  · {}", text.replace('\n', "\n    "));
                    }
                }
            }
            QuestionKind::Times { .. } => println!("  (times ride `polls status` unspecced)"),
        }
    }
}

/// Whether an agent may reach a command, and — when it may not — why.
///
/// This is the durable half of closing a gap that stayed open for six weeks:
/// the MCP surface drifted to seven tools behind twenty commands, and **nothing
/// anywhere failed while it did**. Polls, notebooks and request types were
/// unreachable by any agent; mecha said "I don't have a tool that can create
/// polls" and was right, and the only way anyone found out was by asking it to
/// make one.
///
/// So the decision is made structural, in the shape mecha already uses when
/// `[outbox] tools` names a tool that does not exist: adding a command fails the
/// build until somebody writes down, here, whether an agent should reach it.
/// A reason is required for every exclusion, because "it isn't exposed" and "we
/// decided it should not be" are different states that look identical in a
/// diff.
#[cfg(test)]
#[derive(Debug)]
enum Reach {
    /// Exposed as these MCP tools.
    Tools(&'static [&'static str]),
    /// Not an agent's to run, for this reason.
    NotATool(&'static str),
    /// A group whose actions each carry their own row.
    Descend,
}

#[cfg(test)]
mod surface {
    use super::Reach::{self, Descend, NotATool, Tools};

    /// Every command `factory-publish` has, and what an agent may do with it.
    pub const REACH: &[(&str, Reach)] = &[
        // The bundle pipeline: an agent's core loop here.
        ("render", Tools(&["bundle_render"])),
        ("publish", Tools(&["bundle_publish"])),
        ("alias", Tools(&["bundle_alias"])),
        ("unpublish", Tools(&["bundle_unpublish"])),
        ("fetch", Tools(&["bundle_fetch"])),
        ("list", Tools(&["bundle_list"])),
        ("status", Tools(&["bundle_status"])),
        ("notebook", Tools(&["notebook_render"])),
        ("type", Descend),
        ("type push", Tools(&["type_push"])),
        ("type list", Tools(&["type_list"])),
        ("type check", Tools(&["type_check"])),
        // The personal public surface. An agent reaches all of it: writing
        // somebody's own pages from their own files is the errand worth
        // delegating, and the release scope is what bounds it. One
        // `surface_push` covers every record, because the difference between
        // a profile and a switchboard is an argument rather than a capability.
        ("profile", Descend),
        ("profile push", Tools(&["surface_push"])),
        ("profile pull", Tools(&["surface_pull"])),
        ("hangar", Descend),
        ("hangar push", Tools(&["surface_push"])),
        ("hangar pull", Tools(&["surface_pull"])),
        ("switchboard", Descend),
        ("switchboard push", Tools(&["surface_push"])),
        ("switchboard pull", Tools(&["surface_pull"])),
        ("switchboard list", Tools(&["surface_list"])),
        ("polls", Descend),
        (
            "polls create",
            Tools(&["poll_create", "poll_meeting_create"]),
        ),
        ("polls status", Tools(&["poll_status"])),
        ("polls close", Tools(&["poll_close"])),
        (
            "polls sweep",
            NotATool(
                "the timer's verb: it advances every open meeting poll by one tick, with no \
                 judgment in it. A model reads where a poll stands through poll_status; the \
                 decisions are the sweep's and the owner's.",
            ),
        ),
        (
            "polls export",
            NotATool(
                "a bulk CSV dump of every ballot, for a spreadsheet or an analysis script — \
                 a person's errand, and one whose output nobody reads in a chat window. \
                 poll_status already returns the answers an agent needs, prose included.",
            ),
        ),
        (
            "polls addin",
            NotATool(
                "it writes an Office add-in manifest to be sideloaded once per machine — \
                 setting up the operator's laptop, not doing their work.",
            ),
        ),
        ("slots", Descend),
        (
            "slots push",
            NotATool(
                "a scheduled command with no model anywhere: a systemd timer runs it every \
                 two minutes against live freebusy. An agent calling it would race the \
                 timer to publish availability, and everything it refuses it refuses \
                 loudly because the failure mode is offering strangers time the user does \
                 not have.",
            ),
        ),
        (
            "check",
            NotATool(
                "the same gate `publish` runs, and bundle_publish already runs it — a \
                 separate tool would only let a model check and then publish something \
                 else.",
            ),
        ),
        (
            "push",
            NotATool(
                "re-uploads bytes already published, for recovering from a failed mirror. \
                 bundle_publish reports that failure with the exact command to run; \
                 retrying it is the operator's call.",
            ),
        ),
        (
            "serve",
            NotATool(
                "a loopback preview server that blocks until interrupted. A tool that \
                 never returns is a hung run.",
            ),
        ),
        (
            "remote",
            NotATool(
                "prints which gate and keys this machine has installed — operator diagnostics.",
            ),
        ),
        (
            "mcp",
            NotATool("this server itself; a tool that starts it would be recursion."),
        ),
        (
            "drain",
            NotATool(
                "a CLI and deliberately not a tool — the documented exclusion. It pulls \
                 strangers' submissions, and `mecha frontdoor` is the quarantine that \
                 decides what a privileged run may see of them.",
            ),
        ),
        (
            "connect",
            NotATool(
                "pairing decides what an agent on this machine may do, so it is not \
                 something an agent should do to itself as a side effect of conversation.",
            ),
        ),
        (
            "disconnect",
            NotATool("the other half of pairing, for the same reason."),
        ),
        (
            "operator",
            NotATool(
                "accounts, invites, keys and withholds — the operator's own surface, and \
                 the one place where a compromised run could hand out access rather than \
                 merely publish something.",
            ),
        ),
    ];
}

#[cfg(test)]
mod tests {
    use super::surface::REACH;
    use super::Reach;
    use clap::CommandFactory;
    use std::collections::BTreeSet;

    /// The remediation command a publish-key-only machine prints has to be a
    /// command this binary accepts.
    ///
    /// It was not: it spelled the version `--version {v}` where `alias` takes
    /// it positionally, so the one instruction that gets a staged bundle in
    /// front of a reader failed with `unexpected argument '--version'`. Prose
    /// cannot check itself, so it is parsed here through the real `Cli` — the
    /// only assertion that would have caught it.
    #[test]
    fn the_printed_release_command_parses() {
        use clap::Parser;

        let printed = mecha_factory_publish::remote::release_command("brief", 3);
        let argv: Vec<&str> = printed.split_whitespace().collect();
        assert_eq!(argv[0], "factory-publish", "{printed}");

        let parsed = super::Cli::try_parse_from(&argv).unwrap_or_else(|e| {
            panic!(
                "the note tells the user to run `{printed}`, which this \
                                        binary refuses:\n{e}"
            )
        });
        match parsed.command {
            super::Command::Alias {
                id,
                version,
                visibility,
            } => {
                assert_eq!(id, "brief");
                assert_eq!(version, 3);
                assert_eq!(visibility.as_deref(), Some("public"));
            }
            _ => panic!("`{printed}` parses, but as some other subcommand"),
        }
    }

    /// Every path clap knows about, one level into any group.
    fn cli_paths() -> BTreeSet<String> {
        let cli = super::Cli::command();
        let mut paths = BTreeSet::new();
        for top in cli.get_subcommands() {
            let name = top.get_name().to_string();
            if name == "help" {
                continue;
            }
            let descend = REACH
                .iter()
                .any(|(path, reach)| *path == name && matches!(reach, Reach::Descend));
            if descend {
                for child in top.get_subcommands() {
                    if child.get_name() != "help" {
                        paths.insert(format!("{name} {}", child.get_name()));
                    }
                }
            }
            paths.insert(name);
        }
        paths
    }

    /// The test that would have caught the drift: a command with no row here is
    /// a decision nobody made.
    #[test]
    fn every_command_is_either_exposed_or_excluded_with_a_reason() {
        let declared: BTreeSet<String> = REACH.iter().map(|(path, _)| path.to_string()).collect();
        let actual = cli_paths();

        let undecided: Vec<&String> = actual.difference(&declared).collect();
        assert!(
            undecided.is_empty(),
            "these commands are reachable by a person and not decided about for an agent: \
             {undecided:?}\nAdd a row to `surface::REACH` — Tools(...) if an agent should \
             reach it, NotATool(\"why\") if not."
        );

        let stale: Vec<&String> = declared.difference(&actual).collect();
        assert!(
            stale.is_empty(),
            "these rows name commands that no longer exist: {stale:?}"
        );
    }

    /// A row may not name a tool the server does not serve, and the server may
    /// not serve a tool no row accounts for.
    #[test]
    fn the_rows_and_the_served_tools_agree() {
        let served: BTreeSet<&str> = mecha_factory_publish::mcp::tool_names()
            .into_iter()
            .collect();
        let mut claimed = BTreeSet::new();
        for (path, reach) in REACH {
            if let Reach::Tools(names) = reach {
                for name in *names {
                    assert!(
                        served.contains(name),
                        "`{path}` claims to be exposed as `{name}`, which this server does \
                         not serve"
                    );
                    claimed.insert(*name);
                }
            }
        }
        let orphans: Vec<&&str> = served.difference(&claimed).collect();
        assert!(
            orphans.is_empty(),
            "these tools are served but no command row accounts for them: {orphans:?}"
        );
    }

    /// An exclusion with an empty reason is the drift again, wearing a row.
    #[test]
    fn every_exclusion_says_why() {
        for (path, reach) in REACH {
            if let Reach::NotATool(reason) = reach {
                assert!(
                    reason.len() > 30,
                    "`{path}` is excluded without a reason anyone can act on"
                );
            }
        }
    }
}

// ---- the personal public surface ----------------------------------------

/// The remote, presenting the release credential.
///
/// `Scope::Release` rather than `Publish`, matching the box: a board is a
/// page with buttons at a URL, which is "what the world can see" rather than
/// "versions nobody can read".
fn surface_remote() -> Result<remote::Remote> {
    remote::Remote::configured_for(remote::Scope::Release)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no release credential is installed. `factory-publish connect` pairs this \
             machine, or write ~/.mecha/factory/release.key"
        )
    })
}

fn surface_push(what: Record, file: Option<PathBuf>) -> Result<()> {
    let path = match file {
        Some(path) => path,
        None => what.default_path()?,
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    what.check(&text)?;
    let answer = surface_remote()?.record_push(&what.route(), &text)?;

    // The overwritten list is the part worth printing loudly. A push that
    // silently reverted something somebody fixed in the cockpit is the
    // failure the merge exists to prevent, and a near miss nobody hears
    // about is how the next one becomes a real one.
    let overwritten: Vec<&str> = answer["overwritten"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if overwritten.is_empty() {
        println!("pushed {} from {}", what.what(), path.display());
        // A page with three of eight fields set looks broken rather than
        // unfinished, and "stored" alone invites the wrong diagnosis.
        if let Ok(profile) = mecha_manifest::Profile::from_toml(&text) {
            let unset = profile.unset();
            if !unset.is_empty() {
                println!("  not set: {}", unset.join(", "));
            }
        }
    } else {
        println!("pushed {} from {}", what.what(), path.display());
        println!();
        println!(
            "  This file overwrote {} edited in the cockpit: {}",
            if overwritten.len() == 1 {
                "a field"
            } else {
                "fields"
            },
            overwritten.join(", ")
        );
        println!("  Those edits are gone. `pull` first next time to keep them.");
    }
    Ok(())
}

fn surface_pull(what: Record, file: Option<PathBuf>) -> Result<()> {
    let path = match file {
        Some(path) => path,
        None => what.default_path()?,
    };
    let answer = surface_remote()?.record_get(&what.route())?;
    let source = answer["source"].as_str().unwrap_or_default();
    if source.trim().is_empty() {
        println!("the box holds no {} yet — nothing to pull", what.what());
        return Ok(());
    }
    // Parent directories are created, because `pull` on a board made in the
    // cockpit is the case this verb exists for and that machine has no
    // `switchboards/` yet.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, source).with_context(|| format!("writing {}", path.display()))?;
    println!("pulled {} into {}", what.what(), path.display());
    Ok(())
}

fn record_command(what: Record, action: RecordAction) -> Result<()> {
    match action {
        RecordAction::Push { file } => surface_push(what, file),
        RecordAction::Pull { file } => surface_pull(what, file),
    }
}

fn switchboard_command(action: SwitchboardAction) -> Result<()> {
    match action {
        SwitchboardAction::Push { slug, file } => surface_push(Record::Board(slug), file),
        SwitchboardAction::Pull { slug, file } => surface_pull(Record::Board(slug), file),
        SwitchboardAction::List => {
            let answer = surface_remote()?.board_list()?;
            let boards = answer["boards"].as_array().cloned().unwrap_or_default();
            if boards.is_empty() {
                println!("no boards on the box yet");
                return Ok(());
            }
            for board in &boards {
                let slug = board["slug"].as_str().unwrap_or_default();
                let name = if slug.is_empty() {
                    "(the hangar)".to_string()
                } else {
                    slug.to_string()
                };
                // Drift is the actionable column: it means the cockpit holds
                // something this machine's file does not.
                let drift = if board["drifted"].as_bool().unwrap_or(false) {
                    "  edited in the cockpit — `pull` to keep it"
                } else {
                    ""
                };
                println!(
                    "{name:<24}{}{drift}",
                    board["updated_at"].as_str().unwrap_or_default()
                );
            }
            Ok(())
        }
    }
}
