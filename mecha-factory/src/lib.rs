//! The public surface: one binary, three origins, a box we assume is lost.
//!
//! ```text
//!            the world                         home
//!               │                               │
//!        HTTPS  ▼                               │ POST /v1/bundles   (mk_pub_…)
//!   ┌───────────────────────────┐               │ GET  /v1/queue     (mk_drn_…)
//!   │ gate       artifacts  compute│◀────────────┘
//!   │  /v1/*      /b/<id>/   /b/<id>/            the server never initiates
//!   └───────────────────────────┘
//!            │
//!      SQLite (WAL) — the index
//!      files on disk — the content
//! ```
//!
//! A library and a thin binary, for the same reason the rest of this repository
//! is: the tests drive the real thing. An HTTP surface whose behaviour is only
//! ever produced by a running deployment is one nobody checks until it is
//! serving strangers.
//!
//! **Three properties this crate exists to hold**, each of which has a test
//! naming it:
//!
//! 1. **The class of a bundle decides which origin serves it.** `wasm-unsafe-eval`
//!    exists on the compute origin and nowhere else, so a report can never be
//!    weakened to accommodate a notebook.
//! 2. **The server holds no key that reaches home.** It verifies two scoped
//!    keys and stores their Argon2id hashes; nothing here can open a connection
//!    outward, and there is no field in the configuration where a secret could
//!    be put.
//! 3. **Published versions are immutable, and only an acknowledged queue row is
//!    ever deleted.** The alias is the single moving part.

pub mod bundles;
pub mod certificates;
pub mod config;
pub mod db;
pub mod http;
pub mod intake;
pub mod keys;
pub mod mail;
pub mod ratelimit;
pub mod serve;
pub mod tls;
pub mod upload;
