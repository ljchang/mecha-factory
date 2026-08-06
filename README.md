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

Planned, in order: the bundle store and a markdown `report` template; the
vendoring gate; the `notebook` template; `mecha-factory-publish` (the home-side
MCP server that holds the publish key); and `mecha-factory` itself (the server
on the public box).

## Try it

```sh
cargo test
cargo run --example render -- mecha-manifest/types/speaking.toml /tmp/form
xdg-open /tmp/form/index.html
```

That writes `index.html`, `form.css`, `form.js` and `schema.json` from one
manifest. Nothing in any of them makes an external request — which is not a
nicety: the publish gate **fails** on a surviving external reference.

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

## Licence

MIT.
