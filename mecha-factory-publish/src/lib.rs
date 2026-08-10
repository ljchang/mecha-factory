//! The home side of the public surface.
//!
//! Renders bundles, versions them immutably, and moves the one alias a share
//! URL resolves through. Later it will also hold the publish key and POST to
//! the box; today it writes into `~/.mecha/bundles/`, which
//! `tailscale serve <dir>` is enough to make reachable — no VPS, no domains, no
//! origin decisions, and nothing yet that has to be patched forever.
//!
//! Deliberately a library plus a thin binary, because the MCP server that mecha
//! actually talks to is the same code with a different front end. The tool
//! surface it will expose is already decided: `bundle_render` (cheap, local,
//! unrouted), `bundle_publish` / `bundle_alias` / `bundle_unpublish` (each
//! costing one human review, each staged through mecha's outbox),
//! `bundle_fetch` / `bundle_list` / `bundle_status`. Which is why every verb
//! here is a function on a store rather than a branch in `main`.
//!
//! Nothing in this crate depends on `mecha-core`, and nothing may. The shared
//! contract is `mecha-manifest`, which is data.

/// The availability engine lives in `mecha-manifest` — it is part of the
/// contract a `booking` manifest carries, and the box parses slot JSON with
/// the same types. Re-exported here because this crate is where it *runs*
/// (`slots push`), and the CLI reads better naming it locally.
pub use mecha_manifest::availability;
pub mod mcp;
pub mod notebook;
pub mod poll_export;
pub mod polls;
pub mod pyodide;
pub mod records;
pub mod remote;
pub mod render;
pub mod requests;
pub mod serve;
pub mod slides;
pub mod store;
pub mod vendor;

pub use notebook::{notebook, NotebookOptions};
pub use render::{report, Rendered};
pub use requests::{Record, RequestStore};
pub use serve::Preview;
pub use store::{Alias, BundleStore, Published};
pub use vendor::{gate, scan, Finding};

/// The lock a test takes before touching `MECHA_HOME`.
///
/// Environment variables are process-global and the test harness is threaded,
/// so two tests that set and remove the same variable interleave: one test's
/// `remove_var` lands mid-way through the other's read, and a type file that
/// was there a microsecond ago is not found. It only ever fails under a loaded
/// parallel run — exactly the kind of flake that reads as a bug in whatever
/// else changed that day. Poisoning is ignored on purpose: a panicked test
/// already failed, and the variable it left behind is a problem the next test
/// has whether or not the lock reports it.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
