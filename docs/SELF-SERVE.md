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
2. The user runs `mecha factory connect <code>` on the machine that will hold
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
| `mecha factory disconnect` | the agent | authenticates with the key to revoke *that key*. Needs no extra privilege: a credential may always retire itself |
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

1. **The zone move**, which is independent of everything else and stops the
   manual-DNS tax immediately.
2. **B**, the per-user certificate. Nothing self-serve is possible until a new
   user's hostname resolves without an operator.
3. **Signup**: invite → magic link → handle claim. Reuses `intake.rs`.
4. **Pairing**: the code, `mecha factory connect`, and the confirmation that
   names the handle.
5. **The connected-machines page**, and `mecha factory disconnect`.

1 and 2 do not block each other, so either can start.
