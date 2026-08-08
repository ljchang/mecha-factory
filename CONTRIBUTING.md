# Contributing to mecha-factory

How changes move from an idea to the production box. The same workflow holds
for the [`mecha`](https://github.com/ljchang/mecha) repository (minus the
deploy — mecha runs at home, not on a box); its CONTRIBUTING.md points here.

If you are an AI coding agent, read `CLAUDE.md` (where present) and the
`docs/` design documents first — they record *why* each subsystem is shaped
the way it is, which is what you need to change one safely.

The rules below each trace to a real incident, not to taste. The one-line
version: **main is always releasable, production only ever runs a tagged
release, and nothing hand-built ships.**

## Build and test

```sh
cargo build --release
cargo test --workspace
cargo clippy --all-targets      # CI runs with -D warnings; so should you
```

## One working tree per effort

Parallel work — including two AI sessions — never shares a working tree. Use
a git worktree per arc:

```sh
git worktree add ../mecha-factory-<arc> -b <branch>
```

This rule is the cheapest one here: on 2026-08-08 two sessions worked in one
tree, one stashed the other's changes to verify its own commit, and for an
afternoon the only copy of code already deployed to production sat in
`stash@{0}`. A worktree costs a directory; recovering nearly-lost work costs
an afternoon.

## Branches, commits, PRs

- **Branch per arc**, short-lived, named for what it does (`email-door`,
  `scheduler-frontend`). Nothing develops on `main`.
- **Every commit builds and passes tests alone.** History here is bisectable
  and stays that way. Commit messages are one narrative line stating what
  changed in the system's terms — read `git log --oneline` and match it.
- **Every change lands through a PR**, even a maintainer's. CI (tests on
  stable and MSRV, clippy with `-D warnings`) must be green. For substantive
  arcs, run a real review pass before merging — `/code-review` on the PR, or
  an ultrareview for anything touching a security boundary. A solo
  maintainer merges their own PR once checks pass; the PR still exists so
  the change has a reviewable, revertable unit.
- **Rebase-merge** (or fast-forward) — linear history, no merge commits.
  Squash only when the branch's intermediate commits aren't worth keeping.

## Releases

A release is a **tag on main**, nothing else:

1. Bump `version` in the workspace `Cargo.toml` (one PR; every crate
   inherits it).
2. Tag the merge commit `vX.Y.Z` and push the tag.
3. The `release` workflow does the rest, and refuses a tag that doesn't
   match the workspace version:
   - builds the static musl `factory` binary, **verifies it is static**, and
     attaches it with a checksum to the GitHub release;
   - publishes the crates to crates.io in dependency order
     (`mecha-manifest` → `mecha-factory` → `mecha-factory-publish`).

One tag, both artifacts: the binary on the box and the crates on crates.io
can never name different code.

## Deploying the box

Deploys stay on SSH — the root of trust is root on the box, never a
credential in CI (a deploy key in a public repo's Actions secrets would mean
anyone who compromises the workflow owns the box). What is automated is
everything *after* the trigger, by `scripts/deploy.sh`, installed on the box
as `factory-deploy`:

```sh
ssh <box> factory-deploy v0.2.0     # download, verify, prove, swap, health-check
ssh <box> factory-deploy --rollback # reinstall the previous binary
```

The script downloads the tagged release asset, verifies the checksum, runs
the binary and `factory check` against the live config **before** touching
the service, keeps the outgoing binary as `factory.prev`, and rolls back by
itself if the health check fails. Never `scp` a locally built binary: the
2026-08-08 outage was an aarch64 build reaching an x86_64 box, and
`status=203/EXEC` names the architecture in no way at all.

## crates.io

- **Publish order is dependency order**, encoded in the release workflow.
- **A published version is forever** — yankable, never deletable or
  reusable. The pass that deserves care is the one before `0.1.0`.
- **Semver is the contract.** Once past `0.x`, run `cargo semver-checks`
  before a version bump; before that, breaking changes bump the minor.
- **One-time setup per crate**: the first version is published by hand by an
  owner (`cargo login && cargo publish -p <crate>` in dependency order),
  because Trusted Publishing can only be configured on a crate that already
  exists. Then, on crates.io → the crate → Settings → Trusted Publishing,
  add this repository and `release.yml`. From the next tag on, the workflow
  publishes with a 30-minute OIDC token and no long-lived secret exists
  anywhere.
