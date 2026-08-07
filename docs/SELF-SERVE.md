# Self-serve: a second person, without an SSH session

Today the factory has exactly one user, and creating a second is a root SSH
command followed by a service restart. That is not a gap in the implementation
— it is what the design assumed, and the log says so out loud on every start:

```
ordering certificates; a user created after this needs a restart to get one
```

This document is the plan for removing that assumption. It is written before
the code so the decisions can be argued about while they are still cheap.

## What we are not building

Naming these first, because each is a thing somebody will reasonably propose:

- **No SSH access for tenants.** A mecha agent does not need a shell on the
  box, and giving it one would mean a Unix account per user and a machine whose
  premise — one static binary and a SQLite file — has stopped being true. The
  connection already exists: `remote.rs` speaks HTTPS to four verbs with a
  scoped key, and **home always initiates**, which is what makes "the box holds
  no credential that reaches home" checkable by inspection.
- **No passwords.** The magic-link machinery in `intake.rs` already does
  single-use, short-expiry verification with per-recipient and per-user send
  budgets, and SES is live. Signup is that, pointed at a new row type. No
  password store, no reset flow, no credential stuffing, nothing to breach.
- **No asymmetric keys, yet.** Tempting, and the right end state — a box that
  holds only public keys leaks nothing when it is lost. But the tokens are
  already high-entropy random stored argon2-hashed, so a stolen database is
  already not crackable, and under the pairing flow below the secret never
  touches a browser either. The remaining win is real but narrow. Worth doing
  when `keys.rs` is being changed for another reason; not worth a rewrite of
  its own.
- **No open signup on day one.** See "Handles are forever".
- **No separate repository for the box.** Tempting, since a client has no use
  for the server's source. But `mecha-manifest` is the contract *both sides
  run* — the box derives the schema and validates submissions, home validates
  the drained record against the same code — and one workspace is what makes
  drift impossible. Splitting means a published crate, pinned versions, and
  skew that shows up as records validating differently on each side. **Split
  the distribution instead**: publish `mecha-manifest` and
  `mecha-factory-publish` to crates.io, so a client runs
  `cargo install mecha-factory-publish` and never sees the box. Same benefit,
  no skew, and the deploy story stays "clone one repo".

## The client is not part of the security model

`mecha-factory-publish` serves an MCP surface over stdio, and **any** MCP
client can drive it — mecha, Claude Code, something not written yet. That is a
feature, and it is also the reason for the scope split below.

Two hops, with different mechanisms, and the distinction is worth holding:

```
agent ──stdio MCP, local subprocess, no auth──▶ factory-publish ──HTTPS + scoped key──▶ box
```

The agent never sees a credential. So swapping the client changes nothing about
authentication — and exactly one thing about safety, which had to be fixed:

> "An agent drafts, a human releases" was a property of mecha's
> `[outbox] tools`. A different client, or a typo in that list, had no review
> at all and nothing said so.

A guarantee that depends on which program connected is the
silently-degrading-sandbox shape. So it moved onto the credential, where no
client can be missing it: `Scope::Publish` writes immutable versions nobody can
read, and `Scope::Release` moves an alias or serves a form — the two acts that
change what the world can see. *(Built 2026-08-07.)*

That reframes the web interface too. **A signed-in session that can make an
artifact public is a release credential**, reached through a different door
than `release.key`. Which is the right shape: releasing is one narrow
capability with two front doors, rather than something implied by holding any
write key.

## Two interfaces, and they must not be one

- **Tenant**: settings, the artifacts they own, making one public, connected
  machines, revocation. Authenticated as a user; carries release authority.
- **Operator**: users and status, suspend and withhold, every key, queue
  depths. Authenticated as the operator; today it is root over SSH, which is
  tenable for one user and not for several.

Separate surfaces on purpose. They need different authentication, and a mistake
in the tenant one must not reach the operator one.

## Three problems, and only two are ours

1. **Transport and authentication** — done, and in production. Nothing to
   design.
2. **Provisioning** — how a key reaches a new user's machine. Small.
3. **Account management** — signup, handles, and seeing what is connected.
   Moderate.

The thing that actually gates self-serve is none of these. It is the
certificate, and it is infrastructure rather than auth. It has its own section.

## Provisioning: pairing that never shows anyone a secret

The flow starts in the browser, because that is where a person signs up:

1. A signed-in page mints a **pairing code** — short, high-entropy, single-use,
   expiring in minutes — and displays one command.
2. The user runs `factory-publish connect <code>` on the machine that will hold
   the key.
3. The CLI redeems the code over TLS and receives a publish key and a drain
   key, writing both at mode 0600.
4. The code is consumed. The keys were never rendered in a page, never in a
   clipboard, never in a screenshot, never in shell history.

### The attack this shape has, and it is not the obvious one

This is the OAuth device grant (RFC 8628) with the direction reversed, and
reversing it reverses the attack too.

**Device-code phishing** is not hypothetical — it is the live technique that
hit 340+ Microsoft 365 organisations in early 2026. In the ordinary direction
the attacker initiates, sends the victim a code, the victim approves in their
browser, and the attacker silently collects the victim's tokens. No password is
ever phished, which is exactly why it works.

Our direction cannot leak Alice's key that way. Instead:

> Mallory sends Alice a pairing code from **Mallory's** account. Alice runs it.
> Alice's mecha is now holding Mallory's publish key, and Alice's morning
> briefing publishes to `mallory.art.mecha-factory.ai`.

That is still exfiltration; the credential simply travels the other way. It is
the failure this design has to spend its care on.

RFC 8628's own mitigation applies, pointed the right way: **the user must be
made to verify what they are approving.** So `connect` names the account before
it commits anything:

```
This will connect this machine to handle `mallory`  (mallory@evil.example)
Published bundles will go to  https://mallory.art.mecha-factory.ai
Drained requests will be Mallory's, not yours.
Continue? [y/N]
```

Alice expecting `alice` and reading `mallory` is the entire defence, which
means the prompt has to be **loud and specific rather than a confirmation**.
`Connect to mecha-factory? [Y/n]` is a prompt everybody answers `y` to, and it
would make this whole paragraph decorative. Default `N`, and EOF is `no` — the
same rule the outbox already follows for a send drafted with the trifecta
armed.

The other two mitigations are cheap and should be taken: enough entropy in the
code that guessing is infeasible, plus rate limiting and a short life, so the
work an attacker can do against a live code is bounded.

## Revocation: authority is the box, interface is anywhere

Worth separating, because "revoke from either side" mixes two different things.

**Only the box refusing a key is revocation.** Deleting the file on the agent
stops that agent using it; a copy taken beforehand still works. `keys.rs`
already has the authoritative operation, and already keeps the row rather than
deleting it, because the record that a key existed and when it stopped is the
point.

Three interfaces, all ending in the same server-side revoke:

| Where | Who | Notes |
|---|---|---|
| a signed-in page | the user | "machines connected", with first-seen and last-used |
| `factory-publish disconnect` | the agent | authenticates with the key to revoke *that key*. Needs no extra privilege: a credential may always retire itself |
| `factory key revoke <id>` | the operator | break-glass, already built |

The middle one is the reason a compromised laptop is recoverable by the person
who owns it rather than only by whoever holds root on the box.

**A list of connected machines is a security feature, not a convenience.** It
is what turns a silent compromise into a visible one, and it costs almost
nothing because the ledger already exists.

## Handles are forever

`valid_handle` and the reserved list already exist, and a handle is a DNS label
that becomes a permanent hostname. Handles are never reissued, so **open signup
on day one means `luke`, `dartmouth` and `anthropic` are claimed by strangers
in the first week**, and the remedy is taking a name off somebody.

Start invite-only: the operator mints an invite, the box emails it, the link
carries the right to claim one handle. That is a flag and an extra row, not an
architecture, and it can be relaxed the day squatting is worth handling
properly.

## Certificates: the thing that actually gates this

TLS-ALPN-01 cannot issue a wildcard, so every user's hostname needs to be on a
certificate, and today they all share one ordered at startup. A user created
while the server is running has **no certificate at all** until it restarts —
their URLs fail at the handshake, before the application is reached.

Note what that failure currently buys, because it is easy to throw away: an
unclaimed handle is unreachable at the TLS layer, so the 404 the server would
have returned is a *second* line of defence rather than the first.

Two constraints were measured rather than assumed:

- **`rustls-acme` 0.15 cannot add a domain at runtime.** `AcmeState::new`
  consumes its config; `domains()` and `domains_push()` are builder methods
  usable only before `.state()`, and there is no setter afterwards. It orders
  one certificate covering all names.
- **`rustls-acme` has no DNS-01.** `UseChallenge` is `Http01 | TlsAlpn01`, and
  a wildcard requires DNS-01. So wildcards are foreclosed by the library,
  whoever hosts the DNS.

And one that corrected an assumption: **Let's Encrypt exempts renewals from
rate limits**, even as lifetimes fall to 45 days. Only *new* certificates count
against the 50-per-registered-domain-per-week, refilling at one per 202
minutes. Per-user certificates are therefore far more affordable than renewal
churn suggests.

| | Restart per signup | Ceiling | Cost |
|---|---|---|---|
| **A** — restart on user creation | yes | ~48 users (SAN limit on one certificate) | almost nothing |
| **B** — one ACME state per user, dispatch by SNI | **no** | ~50 new per week; thousands total | moderate, all our own code |
| **C** — DNS-01 wildcard | no | unlimited | replace `rustls-acme` **and** move DNS |

**B is the plan.** `state.resolver()` returns an `Arc<ResolvesServerCertAcme>`,
which is a `ResolvesServerCert`; a wrapper holding `HashMap<sni, resolver>` can
be added to at runtime. Creating a user spawns an `AcmeState` for their two
names and inserts it. No restart, no new dependency, and the rate ceiling is
irrelevant at any scale this will see soon.

**C is the better end state and is deliberately deferred.** One certificate for
`*.art` and `*.compute`, no per-user work at all. It means swapping to
something like `instant-acme` with our own renewal loop. Revisit at a few
hundred users, or the day a tenant wants a custom domain.

If C is ever taken, the §14.2 objection — a zone-scoped API token on a box we
assume is lost — has a standard answer: **delegate `_acme-challenge` by CNAME
to a separate throwaway zone**, so the token on the box controls only challenge
records and can never touch the real ones.

### Considered and rejected: routes instead of hostnames

The obvious escape, and it genuinely works: serve
`art.mecha-factory.ai/u/alice/b/brief/` instead of
`alice.art.mecha-factory.ai/b/brief/`, and **the certificate problem disappears
entirely**. One certificate, three names, ordered once. A new user needs no
certificate at all — no restart, no per-user ACME state, no SAN ceiling, no
rate limit. Options A, B and C all evaporate. It is the largest simplification
available anywhere in this document, and it is worth stating plainly before the
reasons not to.

**It cannot be done on the compute origin.** A `compute` bundle is served with
`connect-src 'self'`, `worker-src 'self' blob:` and `wasm-unsafe-eval`. On a
shared origin, `'self'` *is* the other tenants: Alice's notebook can
`fetch('/u/bob/…')` and read Bob's bundle, and a service worker scoped to `/`
intercepts every request every tenant makes. That is not a leak to be narrowed;
it is every co-located account, at once.

Note what the line actually is. It is **not** WebAssembly — plain JavaScript
does all of the above, and `wasm-unsafe-eval` is only why compute needs the
grant. The line is *executes* versus *does not execute*, which is exactly the
distinction `ContentClass` already draws, and `Role::for_class` is already the
one function that maps it to an origin. So the hybrid is expressible in a
single place: paths for artifacts, which execute nothing, and hostnames for
compute, which does.

The hybrid is a real option and is rejected for a different reason than the
first half. **A published URL has to stay resolvable forever.** Path-routing
the artifact origin commits to that shape permanently — the day it has to
become a hostname, every URL published in between breaks. `config.rs` chose
per-user hostnames from the first row for precisely this, before any of the
certificate work existed. So the hybrid is not "safe for artifacts, risky for
notebooks". It is safe for artifacts *irreversibly*.

There is also a property that would be quietly lost. Today an unclaimed handle
has no certificate, so a stranger fails at the TLS handshake and never reaches
the application; the 404 is the second line of defence. Under path routing
every path is reachable and the 404 is the only one.

So the trade is **a bounded problem against a one-way commitment**: per-user
certificates are perhaps a day's work on machinery that already runs, while the
URL shape cannot be taken back. That is why it is hostnames, and it should not
need re-deriving the next time somebody meets the certificate work and wonders
why it is not simpler.

## DNS

Moving the zone to a provider with an API does **not** unlock wildcards on its
own, because the library is the binding constraint. It is still worth doing,
for reasons that stand alone: Squarespace has no API for custom records, so
every change is typed by hand — the five SES rows were, and a tenant's custom
domain would be. Scoped tokens, automation, and keeping C open are the payoff.

**DNS-only, never proxied.** §13.2's objection is to a proxy terminating TLS
and reading the plaintext of drained submissions, and to TLS-ALPN-01 breaking
because the proxy answers the handshake the challenge lives in. Hosting the
zone triggers neither.

## Build order

0. ~~**The scope split.**~~ *Done 2026-08-07.* First because it was cheapest
   before any key existed in the wild and steadily more expensive after — and
   because it is what stops the review gate depending on the client. **Not yet
   deployed**: the live box has one `publish` key that can currently alias, so
   a release key has to be minted and placed before the binary ships, or
   aliasing breaks.
1. **The zone move**, which is independent of everything else and stops the
   manual-DNS tax immediately.
2. **B**, the per-user certificate. Nothing self-serve is possible until a new
   user's hostname resolves without an operator.
3. **Signup**: invite → magic link → handle claim. Reuses `intake.rs`.
4. **Pairing**: the code, `factory-publish connect`, and the confirmation that
   names the handle.
5. **The tenant surface**: artifacts owned, make public, connected machines,
   `factory-publish disconnect`.
6. **The operator surface**, which is the last thing still living on an SSH
   session.

1 and 2 do not block each other, so either can start.
