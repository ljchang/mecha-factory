# Self-serve: a second person, without an SSH session

The factory has one user, and creating a second was a root SSH command followed
by a service restart. That was not a gap in the implementation — it was what the
design assumed, and the log said so out loud on every start:

```
ordering certificates; a user created after this needs a restart to get one
```

This document is the plan for removing that assumption. It was written before
the code so the decisions could be argued about while they were still cheap;
the build order at the end says which parts have since been built. **The
restart is gone** as of 2026-08-07 — that line now reads *"a user created from
here on gets one without a restart"* — so what remains between here and a
second person is signup, pairing, and the two interfaces.

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

### What "any MCP client" commits us to (reviewed 2026-08-07)

Claude Code driving this server is not a hypothetical, so the plan was
re-read against a client that has **no outbox, no trifecta interlock, and no
frontdoor quarantine** — mecha's three safety layers, none of which travel
with the MCP protocol. The architecture already holds, for reasons worth
naming so nobody re-derives them: signup and account management are
browser-and-email and involve no client at all; authentication lives in this
server's key files, so the agent never sees a credential whoever it is; and
the release gate lives on the credential scope, which is exactly the decision
above. Three consequences are load-bearing for the clientless-safety story
and should be treated as constraints, not defaults:

- **`connect` mints publish and drain. Never release.** A paired agent
  machine's worst case must stay "immutable versions nobody can read". The
  code today will use a `release.key` from the MCP path when one is present
  (`remote::mirror_alias` — under mecha that call is outbox-routed; under
  Claude Code the only gate is the client's own tool prompt), so keeping
  release keys off agent machines is *policy the pairing flow should state
  out loud* — and `connect` should warn when it finds one already installed.
- **The pairing confirmation cannot assume a human reads stdin.** An agent
  can be the one running `connect`, and `echo y |` defeats a y/N prompt
  without anybody seeing the handle named. The fix is the plan's own
  principle made structural: the confirmation is **typing the handle**, not
  `y` — interactively, the user types the handle they expect; non-TTY,
  `--handle <expected>` is required. Mallory's code pairs to `mallory`, the
  asserted handle does not match, and the connect fails with no judgment
  call anywhere. (This also gives the agent-driven path its safe shape: the
  user tells their agent "connect this machine to alice", and the assertion
  travels with the command.)
- **The queue's prose never crosses the MCP surface.** `drain` is
  deliberately CLI-only today; the MCP tools are `bundle_*` and must stay
  that way. mecha drains into `~/.mecha/requests/` where the frontdoor's
  extractor quarantines free text before any run with tools sees it — a
  Claude Code session has no such layer, so an MCP drain tool would hand a
  stranger's prose straight to a privileged context. If another client ever
  needs queue access over MCP, it gets the typed, non-prose fields
  (`Record::for_privileged_run`'s shape) and nothing else; the prose stays
  with clients that own a quarantine.

One distribution note that gets more urgent with a second client:
`cargo install mecha-factory-publish` (the crates.io split under "What we are
not building") is how a Claude Code user arrives, since they have no reason
to clone this repository. And one conscious limit, recorded rather than
discovered: the key files live at fixed paths (`~/.mecha/factory/`), so a
machine pairs to **one** handle at a time — fine for the invite-only phase,
and the thing to revisit if one person ever operates several handles from
one machine.

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

**B is the plan, via HTTP-01** — and it is what shipped, on 2026-08-07. What
follows is the reasoning that produced it, kept because the constraints it
names are the ones to re-read before changing any of it. The idea always
survived:
`state.resolver()` returns an `Arc<ResolvesServerCertAcme>`, which *is* a
`ResolvesServerCert`, and its `resolve` already dispatches the TLS-ALPN-01
challenge by SNI — so a wrapper holding `HashMap<sni, resolver>` serves both
real traffic and challenges correctly, and can be added to at runtime.

What blocks it is the acceptor. A TLS-ALPN-01 challenge arrives as a
*connection*, not a request, and something has to answer it with the challenge
certificate and then **not** hand that connection to the application.
`AcmeAcceptor` does exactly that, and:

```rust
pub(crate) fn new(resolver: Arc<ResolvesServerCertAcme>) -> Self
```

It is `pub(crate)`, and it takes the library's concrete resolver rather than a
`dyn ResolvesServerCert`. So the one type that must know about all the
certificates cannot be handed a wrapper that does. `state.acceptor()` yields an
acceptor bound to a single state, and there is no way to combine several.

**And then the way past it, which is to stop using that challenge.** The
acceptor exists only because TLS-ALPN-01 arrives as a TLS connection on 443
that must be answered and then dropped. HTTP-01 does not: the challenge is an
ordinary GET on port 80, and both pieces needed to serve it are public API —

```rust
AcmeConfig::challenge_type(UseChallenge::Http01)          // config.rs:212
ResolvesServerCertAcme::get_http_01_key_auth(&self, tok)  // resolver.rs:48
```

so the plan becomes:

- One `AcmeState` per certificate group — the three base names, then one per
  user — each configured for HTTP-01.
- A `ServerConfig` built with `with_cert_resolver`, holding our own wrapper
  over `HashMap<sni, Arc<ResolvesServerCertAcme>>`. Plain `tokio-rustls`; no
  `AcmeAcceptor` anywhere.
- One route on the port-80 listener for
  `/.well-known/acme-challenge/{token}`, asking each resolver in turn and
  answering with the first `Some`. The redirect listener already exists.
- Creating a user spawns a state, inserts a resolver, and **nothing restarts**.

The cost is one property, and it should be stated rather than discovered:
DEPLOY.md used to say *"TLS-ALPN-01 does its challenge on 443, so port 80 is
never part of issuance — it exists because a human types a bare hostname."*
That stopped being true. Port 80 is load-bearing for certificates now, so
whoever can answer on it can obtain them. The mitigation is that anyone who can
answer on port 80 of this host has already won, and 443 was never less exposed
— but the sentence had to change with the code, or it would have become a claim
the deployment no longer earns. *(Changed, along with the two Cloudflare
paragraphs that turned on the same fact: proxying no longer breaks issuance,
because a proxy forwards a plain GET. Plaintext was always the serious half of
that objection and it is untouched.)*

And one more that is a refusal rather than a note: **`[listen] http` is
required beside `[tls]`**, checked in `Config::check`. A box that comes up
serving TLS and cannot renew works for sixty days and then does not, which is
the failure shape this project keeps finding.

HTTP-01 still cannot issue wildcards. Nothing is lost there, since
TLS-ALPN-01 could not either.

Three ways past it *without* changing challenge type, kept for the record and
all worse:

- **Do the TLS layer here**: `tokio-rustls` with our own `ServerConfig`, our
  own dispatching resolver, and our own detection of the `acme-tls/1` ALPN so a
  challenge connection is answered and dropped rather than passed to axum. All
  the pieces are public; the acceptor's job is small and would be ours.
- **A different ACME crate.** `tokio-rustls-acme` is a fork with a different
  surface and may not have the same restriction; `instant-acme` gives up the
  serving integration entirely and leaves us the renewal loop.
- **Option A as an interim.** A restart on user creation is a few lines and
  costs seconds of downtime with certificates already cached. Ugly, honest, and
  it unblocks everything downstream of the certificate while the real answer is
  chosen.

None of those three are needed now. They are recorded because the reasoning
that produced them — that owning the TLS layer is most of what option C needs
anyway — is still true, and is the argument to reach for if HTTP-01 ever stops
being enough.

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

0. ~~**The scope split.**~~ *Done and deployed 2026-08-07.* First because it
   was cheapest before any key existed in the wild and steadily more expensive
   after — and because it is what stops the review gate depending on the
   client. Verified against the live box: the publish key is refused on
   `/alias` with "this key does not cover this endpoint", and the release key
   is accepted.
1. **The zone move**, which is independent of everything else and stops the
   manual-DNS tax immediately. It turned out not to gate 2 either — the
   wildcard `A` records the deployment already had are enough for a new handle
   to answer an HTTP-01 challenge.
2. ~~**B over HTTP-01**~~ *Built 2026-08-07* — `src/certificates.rs`. A state
   and a resolver per user behind an SNI-dispatching wrapper, a challenge route
   ahead of the redirect on port 80, and the certificate set reconciled against
   the ledger every thirty seconds. Four things worth carrying forward:
   - **A reconcile loop, not a notification.** `factory user create` runs in
     another process, so a channel would only have served the signup endpoint
     that does not exist yet and left the SSH path needing its restart.
   - **The wildcard DNS records were already there**, which is why 1 turned out
     not to gate this: a brand-new handle resolves and can answer a challenge
     with no zone work at all.
   - **`[listen] http` is now required beside `[tls]`**, refused at startup.
     Port 80 is where certificates come from; a box without it serves its cache
     for sixty days and then stops.
   - The property §14.3 asked us to keep survived: an unclaimed handle has no
     resolver, so it still dies at the TLS handshake and the 404 is still the
     second line of defence.
3. ~~**Signup**~~ *Built 2026-08-07* — `factory invite create --email …`
   mints the right to claim one handle (7-day expiry, token hashed at rest,
   link printed and mailed), and `GET/POST /signup/<token>` on the gate is
   the claim: pick a handle, and the account is created through the same
   `invite_claim → create_user_in` path the CLI uses, spending the invite in
   the same transaction. No second verification email — the invite arrived
   by email and the single-use token proves the click. Every kind of dead
   invite is one page with one set of bytes, and a rejected handle keeps the
   invite claimable. The schema migration this needed (v3 → v4) established
   the additive-migration rule: the box is live, so "delete the database"
   stopped being a printable instruction.
4. ~~**Pairing**~~ *Built 2026-08-07* — the signup welcome page (and
   `factory pair create`, until the tenant surface exists) mints a
   single-use, 15-minute code; `factory-publish connect --gate … --handle …
   <code>` spends it at `POST /v1/pair` and installs this machine's own
   publish and drain keys at 0600, remembering the gate beside them. The
   review's constraint became the protocol: **the asserted handle is checked
   by the server**, a mismatch spends nothing and answers exactly what a
   nonexistent code answers, so no client can skip the confirmation and a
   stolen code cannot be probed for whose it is. Interactively the person
   types the handle; non-TTY requires `--handle`. Each machine pairs
   separately and its keys revoke separately — same-`$HOME` agents share a
   connection (separate keys there would be separable in the ledger, not in
   security), and an agent wanting real isolation gets its own `MECHA_HOME`
   and pairs as its own machine. `connect` warns when it finds a release key
   on the machine, and never installs one.
5. ~~**The tenant surface**~~ *Built 2026-08-07* — `/account` on the gate:
   magic-link sign-in (no passwords, oracle-free, budgeted per account per
   day), a `__Host-`-prefixed session cookie (the prefix is what stops a
   tenant's page tossing a `Domain=` cookie onto the gate, deferring the
   §14.2 domain move), CSRF tokens derived from the session on top of
   `SameSite=Lax`. The page is the second release door: release/unrelease
   drives the same `alias_set` the release key drives. Machines-connected
   is the keys ledger with `last_used_at` stamped on every authenticated
   call — a silent compromise shows as life where none was expected — with
   per-key revoke, and pairing codes minted from the page. A session
   deliberately cannot publish: uploading stays with the machines' scoped
   keys. `factory-publish disconnect` closes the loop — each installed key
   revokes itself (`POST /v1/disconnect`, authenticated by the key being
   revoked, no extra privilege) and leaves the disk; a key that could not
   be revoked keeps its file, because deleting the local copy of a live
   credential is tidiness dressed as security.
6. ~~**The operator surface**~~ *Built 2026-08-07* — a fourth scope rather
   than a second session system: `Scope::Operate`, minted once on the box
   (`factory key create --scope operate` — the last SSH), bound to the box
   rather than to any tenant, driving `/v1/admin/*` through
   `factory-publish operator …` — users and status, invites (mailed by the
   box, which the on-box CLI could not do for a remote operator), every key
   with last-used, break-glass revoke, withhold. The two surfaces are kept
   apart by the credential: no tenant key reaches an admin endpoint and the
   operate key is refused everywhere a tenant key works. Deliberately
   CLI-only, never MCP tools — suspending users is not power an agent
   wields as a side effect of conversation, the same rule that keeps
   drain's prose off the tool surface. What deliberately stays on SSH:
   deploys, and minting a replacement operate key if every one is lost —
   root on the box remains the root of trust, it just stops being the
   daily interface.

1 and 2 do not block each other, so either can start.
