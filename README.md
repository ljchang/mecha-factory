# mecha-factory

The public surface for [mecha](https://github.com/ljchang/mecha): a place for
what an agent makes to live, and a typed way for the outside world to get in.

A factory is where machines are built and shipped from — and it is deliberately
*not* the machine. Orders come in, product goes out.

> **Early.** Only `mecha-manifest` exists so far. The design is in mecha's
> `docs/PUBLIC-SURFACE-DESIGN.md`; §12 is the build order and this repository is
> at step 1.

## Two purposes, matching the two directions of one boundary

- **Publish what mecha makes** — reports, dashboards, a morning briefing,
  notebooks — as durable, versioned, permissioned URLs that can be read on a
  phone, sent to a collaborator, or read back by a later agent run.
- **Build interfaces back into mecha.** A form is the default rendering, not the
  point. One request type emits the HTML form, the JSON Schema, the MCP tool and
  the agent-to-agent skill, so **a human with a browser, an agent with a
  browser, an agent with MCP, and an agent doing discovery all arrive at the
  same typed object.** Adding a modality is a renderer, not a parallel system.

## What is here

| Crate | What it is |
|---|---|
| `mecha-manifest` | The versioned data contract. Request types and bundles, their JSON Schema, their HTML form, and the one validator both ends run. Pure, no I/O, no network. |
| `mecha-factory-publish` | The home side. Renders bundles, versions them immutably, moves the one alias a share URL resolves through. Later it also holds the publish key and serves the MCP surface. |

Planned, in order: the `notebook` template on `marimo export html-wasm`, and
with it the fetching half of vendoring (from a pinned allowlist — marimo's six
CDN references are known and version-pinned); the MCP surface; and
`mecha-factory` itself — the server on the public box, which is the first thing
here that creates a machine to patch forever.

## Try it

A form, from one manifest:

```sh
cargo run --example render -- mecha-manifest/types/speaking.toml /tmp/form
xdg-open /tmp/form/index.html
```

That writes `index.html`, `form.css`, `form.js` and `schema.json`. Nothing in
any of them makes an external request — which is not a nicety: the publish gate
**fails** on a surviving external reference.

A published report, from a markdown file:

```sh
cargo run --bin factory-publish -- render notes.md --out /tmp/brief
cargo run --bin factory-publish -- publish morning-brief /tmp/brief --source notes.md
cargo run --bin factory-publish -- status morning-brief
```

`render` is cheap and local; `publish` is the one that costs a human review, so
they are separate verbs. Both run the external-reference gate, and `check` runs
it alone:

```sh
cargo run --bin factory-publish -- check /tmp/brief
```

**A publish fails on a surviving external reference — it does not warn.**
There are two modes, split by kind of object rather than kind of URL: files we
emit are scanned strictly, and a **vendored third-party tree** is reviewed as a
unit — declared with a digest, pinned at the version reviewed, and not walked
line by line, with the CSP as the runtime enforcement. Measured against a real
`marimo export html-wasm`: 710 files and 224 distinct URLs, of which 234
occurrences are XML namespace identifiers no browser ever fetches. A check that
reports 541 things nobody reads is not a check. Fail-closed both ways — an
undeclared subtree is scanned strictly, and a pinned tree that changed is a
finding, not a pass.
 A page
that loads something off-origin breaks under the CSP, tells a third party who
read it and when, and stops being permanent the moment somebody else's bucket
changes. A *link* is not a resource: `<a href="https://…">` is fine and is never
a finding; `<img src="https://…">` is the page reaching out on load, and is.
Every finding names the file, the line, the URL and what made it a resource —
and for a rendered bundle it also names the line in the **source**, because
pointing someone at generated HTML is pointing them at a file they should not
edit. Publishing identical bytes returns the existing version
rather than minting a new one, which makes "did anything actually change?" a
comparison rather than a guess. `alias` moves the share URL; `unpublish` points
it at nothing and destroys no version.

Until there is a server, `~/.mecha/bundles` is the whole of it — point
`tailscale serve` at that directory and the share URLs work over the tailnet. No
VPS, no domains, no origin decisions, and nothing yet to patch forever.

**`visibility` is recorded and not yet enforced**, and every command says so:
the tailnet is the boundary at this stage. A flag that read as enforcement and
was not would be the silently-degrading-sandbox shape.

## The invariants

These are the ones worth not re-litigating. The reasoning is in the design doc;
the short version:

- **Nothing here may depend on `mecha-core`.** The shared contract is data —
  TOML plus a generated JSON Schema — not a struct two crates happen to agree
  on. That is what lets validation happen independently at the edge and at home,
  and it is why the public box holds none of mecha's code.
- **Assume the public box is lost.** It holds a request queue, published bytes
  and a TLS certificate. No provider key, no model, nothing that reaches home.
  Everything drained from it arrives marked as third-party text.
- **The server can only return objects that validate against a schema mecha
  itself uploaded**, and mecha re-validates on arrival. A hostile origin cannot
  invent a field, change a request's type, or exceed a cap. What it *can* do is
  put hostile prose in a field already known to be free text — which is what the
  quarantine layer, in mecha, is for.
- **The server filters shape; only mecha can filter meaning.** A prompt
  injection is well-formed UTF-8 inside a valid field of correct length. No
  amount of structural validation distinguishes it from an ordinary sentence, so
  that judgement is made where the privileged context is. A model on the public
  box would be a provider key on the box we assumed lost.
- **Declarative conditions only.** The browser and the server must evaluate
  exactly the same rules, so a condition is `field`/`operator`/`value` and never
  a closure. A client-side check is a convenience and never a control.
- **Free text is derived, never declared.** A `select` carries one of our own
  values; a `text` field carries whatever someone typed. There is no key that
  marks a text field trusted, because that is precisely the switch that would
  quietly turn the quarantine off.
- **A publish fails on a surviving external reference.** Not warns. It is the
  one enforcement the artifact security model rests on, and a warning is how it
  silently stops holding.

## Notebooks

```sh
factory-publish notebook nb.py --out /tmp/nb --title "Weekly figures" \
  --vendor-runtime v314.0.0
factory-publish serve /tmp/nb --class compute --port 8347 &
scripts/csp-probe.py http://127.0.0.1:8347/ --expect-text "…" \
  --allow-violation "script-src=zod:a guarded Function() probe that falls back"
```

`marimo export html-wasm`, never islands: the islands runtime resolves packages
through an AST scan plus a `micropip` list baked into its JS bundle and reads no
PEP 723, so a pure-Python package Pyodide does not ship fails to import with no
way to fix it from the host page.

**A `compute` bundle is not publishable until its runtime is vendored.**
marimo's export loads Pyodide, the standard library and every wheel from three
hosts at runtime, and `connect-src 'self'` correctly refuses all of it. The
vendorer fetches from a hardcoded allowlist — never from a URL that came out of
notebook content — verifies every wheel against the sha256 in Pyodide's own lock
file, caches per version under `~/.mecha/pyodide/`, and copies into each bundle
so a published version stays self-contained.

Verified in a browser, not asserted: a notebook boots and computes under the
full compute policy with **zero off-origin loads**. See `scripts/csp-probe.py`.

## Confinement — a gap, stated rather than implied

The `notebook` template is the one renderer that **executes code we did not
write**: `marimo export html-wasm` runs the notebook to capture its state. That
is the whole reason rendering and publishing are separate verbs — a process
running arbitrary Python must not also hold the publish key or reach the
network.

**That confinement is not implemented yet.** Today the export runs as you, with
your environment, exactly as `shell` does under mecha's default
`[sandbox] kind = "none"`. mecha confines the MCP server it launches, but the
render subprocess lives *inside* this crate, so mecha's sandbox cannot see it —
enforcing it is this crate's job.

It is written down rather than left implicit because the design is equally clear
on both halves: a renderer that executes notebook code must be confined, **and**
an unenforced claim of confinement is decoration. Do not wire this to anything
unattended until the subprocess is confined and preflighted — the rule that a
configured sandbox which does not work must stop the run transfers here intact.

## Licence

MIT.
