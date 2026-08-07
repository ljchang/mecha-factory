# Driving the factory from Claude Code, or any other MCP client

`mecha-factory-publish` serves its MCP surface over stdio, and nothing in it
assumes mecha is on the other end. This document is the path for a client that
is not mecha — Claude Code today, something not written yet tomorrow — and,
more importantly, what such a client does *not* get and why that is safe.

The design reasoning lives in [`SELF-SERVE.md`](SELF-SERVE.md) under "The
client is not part of the security model" — this file is the user path, not a
restatement of the argument. The one sentence to carry: **safety lives on the
credential scopes and in this binary, so swapping the client changes nothing
about what an agent can do to the world.**

## Install

```sh
cargo install mecha-factory-publish
```

(Until the crates.io split ships: `cargo install --git
https://github.com/ljchang/mecha-factory mecha-factory-publish`.)

The binary is both the CLI and the MCP server. There is no separate daemon.

## Pair the machine

Pairing starts in the browser, where you signed up: your account page mints a
single-use pairing code and shows one command. Run it on the machine that will
hold the keys:

```sh
factory-publish connect <code> --gate https://gate.mecha-factory.ai
```

Interactively, `connect` asks you to **type the handle** you expect this
machine to publish for — typing it is the whole confirmation, and the server
refuses a mismatch without spending the code. When stdin is not a terminal
(an agent running the command for you), `--handle <expected>` is required for
the same reason: the assertion travels with the command, and a code that
pairs to someone else's handle fails with no judgment call anywhere.

Pairing is a CLI verb and deliberately not an MCP tool: it decides what an
agent on this machine may do, which is not something an agent should do to
itself as a side effect of conversation.

What lands, at mode 0600 under `~/.mecha/factory/`:

- `publish.key` — write immutable versions nobody can read.
- `drain.key` — collect what the box has verified for you.

**Never `release.key`.** Making an artifact public is a release, and release
authority stays in the browser — your account page at
`gate.mecha-factory.ai/account`, behind a signed-in session. A paired agent
machine's worst case is "immutable versions nobody can read", and that is the
property to protect: if `connect` finds a release key already installed it
warns, because an agent beside one can make artifacts public with no review.
Under mecha that release path is outbox-routed; under Claude Code the only
gate is the tool-approval prompt, which is exactly one keypress on a good day.
Keep release keys off machines agents use.

`factory-publish disconnect` undoes a pairing; the account page revokes a
machine's keys from the other end.

## Wire it into Claude Code

```sh
claude mcp add factory -- factory-publish mcp --root ~/factory-work
```

or, checked into a project, `.mcp.json`:

```json
{
  "mcpServers": {
    "factory": {
      "command": "factory-publish",
      "args": ["mcp", "--root", "."]
    }
  }
}
```

`--root` is a path jail: every path a tool call supplies must resolve inside
it, and the server says so on startup. It defaults to the working directory,
which is what the `.mcp.json` form above relies on.

## What the tools are

Seven, all `bundle_*`:

| Tool | Reaches the box? | What it does |
|---|---|---|
| `bundle_render` | no | Markdown → a publishable bundle directory, locally. |
| `bundle_publish` | yes | An already-rendered directory → a new immutable version. Identical bytes return the existing version. |
| `bundle_alias` | yes | Point a share URL at a version — a publication, not bookkeeping. On a correctly paired machine (no release key) the answer is "not released here", not an error. |
| `bundle_unpublish` | yes | Withdraw. |
| `bundle_fetch` | no | Read a stored version back. |
| `bundle_list` / `bundle_status` | no | What exists, and where it points. |

The working loop the render tool itself suggests: render, read the output
back, fix it, publish once.

## What this client does not get

- **Release authority.** Covered above; it is the load-bearing one.
- **The queue's prose.** `drain` is deliberately a CLI verb, absent from the
  MCP surface — verified against `tools/list`, not just asserted. A stranger's
  free text goes to mecha's frontdoor quarantine, where an extractor with no
  tools and no history types it before any privileged run sees it. Claude
  Code has no such layer, so an MCP drain tool would hand that prose straight
  to a context holding your tools. If a second client ever needs queue access
  over MCP, it gets the typed non-prose fields and nothing else.
- **A safety net it didn't bring.** mecha stages `bundle_publish` and
  `bundle_alias` through its outbox for human review. Claude Code's
  equivalent is its own tool-approval prompt — real, but per-call and
  habituating. The reason this is acceptable is the scope split: the
  blast radius of every tool on this surface, on a correctly paired
  machine, is bounded by what `publish.key` can do.

## Verified

2026-08-07, driven from a Claude Code session over raw stdio JSON-RPC:
initialize handshake (protocol `2025-06-18`), `tools/list` (the seven above,
no drain), and `bundle_render` end to end through `tools/call`, with the
`--root` jail announced and enforced. The live-box legs (`bundle_publish`
against the gate) and the scope refusals were verified separately — the
publish key refused on `/alias`, the release key accepted — as recorded in
`SELF-SERVE.md`.
