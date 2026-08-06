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

pub mod render;
pub mod store;
pub mod vendor;

pub use render::{report, Rendered};
pub use store::{Alias, BundleStore, Published};
pub use vendor::{gate, scan, Finding};
