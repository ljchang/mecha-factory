# Putting the factory on a box

The first thing in this project that creates a machine to patch forever. That
sentence is the whole reason this document exists: the failure mode of
forgetting is not "the site is down", it is "the site is someone else's".

> **Done once, on 2026-08-06.** A DigitalOcean droplet in NYC —
> Ubuntu 24.04, 1 vCPU, 1 GB RAM, x86_64 — serving
> `gate` / `art` / `compute` under `mecha-factory.ai`, with a Let's Encrypt
> certificate the binary obtained for itself over TLS-ALPN-01. What follows is
> the procedure as it was actually run, including the two things that went
> wrong.

Everything below assumes the box is **assumed lost**. Nothing on it reaches
home, and the two keys it holds are Argon2id hashes of tokens minted elsewhere.

## What runs where

```
  home (your machine)                    the box
  ─────────────────────                  ─────────────────────────────
  factory-publish                POST ─▶ /v1/bundles     (mk_pub_…)
  mecha trigger: drain            GET ─▶ /v1/queue       (mk_drn_…)
                                         serves gate / artifacts / compute
```

The box never initiates a connection to home. There is no field in its
configuration where a credential could be put, which is a property you verify
by reading `/etc/mecha-factory/factory.toml` rather than by trusting a claim.

## Users

A tenant is a person. Create one, and their first key with them:

```sh
factory --config /etc/mecha-factory/factory.toml user create alice \
    --email alice@example.org --with-key
```

The token prints once. A drain key is
`factory key create --handle alice --scope drain`.

**A handle is never issued twice**, including after a rename or a closed
account: a freed handle would let whoever claimed it next serve content at URLs
somebody already put in a paper. Reserved names (`www`, `abuse`,
`_acme-challenge`, …) are refused, and so is anything that is not a legal DNS
label.

Two operations exist for content that should stop being served, and only one of
them destroys anything:

```sh
factory withhold alice brief 1 --reason "reported"   # instant, reversible, keeps the bytes
factory user suspend alice                           # their whole namespace stops serving
```

Neither deletes. That is deliberate — see §15.3 of the design document — and it
is what lets a report that turns out to be wrong cost nothing.

## The three names

Three registrable names are required and they must be distinct, because the
content class of a bundle decides which origin serves it and the compute origin
is the only one granting `wasm-unsafe-eval`. Sharing a name would put a
notebook and a report under one policy, which is the whole reason there are
three.

Point all three at the droplet's address with **A records** (and AAAA if it has
v6), **plus a wildcard** for the two artifact names:

```
gate.example.org           A    203.0.113.10
artifacts.example.org      A    203.0.113.10
*.artifacts.example.org    A    203.0.113.10
compute.example.org        A    203.0.113.10
*.compute.example.org      A    203.0.113.10
```

The wildcards are how `alice.artifacts.example.org` resolves. The
*certificate* is a separate matter: TLS-ALPN-01 cannot issue a wildcard, so the
server orders one name per active user at startup and **a user created while it
is running needs a restart before their hostname has a certificate**. That is
fine for tens of users. Beyond that it wants a real wildcard certificate, which
needs DNS-01 and therefore a zone-scoped API token on the box — recorded in
§14.2 with the mitigation, and deliberately not done yet.

**One consequence of that is a security property rather than a limitation.** A
handle nobody owns has no certificate, so a request for one fails at the TLS
handshake — a stranger cannot reach the application at all. The 404 the server
would have returned is the *second* line of defence here, behind one the
certificate gives for free. A wildcard certificate would remove that first
line, which is worth knowing before treating DNS-01 as a pure upgrade.

### On collapsing to one registrable domain

The deployment runs all three origins under `mecha-factory.ai`, which is not
what §14.2 prefers: the gate is the only origin no user code runs on, and it
would ideally sit on a registrable domain of its own so that a cookie set by a
user's artifact can never be sent to it. Nothing here uses cookies today, so
the separation currently buys nothing.

What it costs to defer: **moving the gate later changes every form URL**, since
those live on the gate. So the day a capability becomes a cookie — which is the
day the argument stops being theoretical — the move has to happen before any
form link is in circulation, not after.

### If the DNS is at Cloudflare

Use **DNS only** — the grey cloud, not the orange one. Proxying changes two
things that this design deliberately decided against:

- **Cloudflare terminates TLS**, so it reads the plaintext of every request and
  response, including drained submissions. §13.2 of the design document chose
  no CDN specifically to avoid that; the honest cost is no DDoS absorption, and
  for a personal booking page that is an annoyance rather than a crisis.
- **TLS-ALPN-01 stops working**, because the proxy answers the handshake the
  challenge lives in. Issuance would have to move to DNS-01, which means an API
  token for the whole zone sitting on the box we assume is lost.

Turning the proxy on later changes nothing about the origin — that is the point
of keeping it a plain program — but it is a decision to make deliberately, not
one to arrive at by clicking a toggle.

## Installing

**Swap first, if the box has 1 GB of RAM.** A release build will not link
without it, and the failure is an OOM kill part-way through rather than
anything that names the cause:

```sh
fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
echo "/swapfile none swap sw 0 0" >> /etc/fstab
```

Then the firewall and the patching, before anything is listening:

```sh
apt-get install -y build-essential pkg-config git curl unattended-upgrades ufw
ufw default deny incoming && ufw default allow outgoing
ufw allow 22/tcp && ufw allow 80/tcp && ufw allow 443/tcp && ufw --force enable
systemctl enable --now unattended-upgrades
```

**The binary is built on the box, from the public repository.** One vCPU takes
a while — this is the step to start and walk away from:

```sh
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
git clone --depth 1 https://github.com/ljchang/mecha-factory /root/build
cd /root/build && cargo build --release -p mecha-factory --bin factory
```

*Worth replacing:* a release workflow that publishes an x86_64 binary would
mean the box never needs a Rust toolchain at all, which is one fewer thing to
patch on a machine whose whole premise is that it is one static binary and a
SQLite file.

```sh
adduser --system --group --no-create-home factory
install -m 0755 target/release/factory /usr/local/bin/factory
install -d -m 0755 /etc/mecha-factory
install -m 0644 factory.toml /etc/mecha-factory/factory.toml
install -m 0644 scripts/mecha-factory.service /etc/systemd/system/

systemctl daemon-reload
systemctl enable --now mecha-factory
journalctl -u mecha-factory -f
```

The unit runs `factory check` before it starts, so a configuration typo fails
without stopping the server that is already running.

> **Do not run `factory check` as root before the service has ever started.**
> It creates the ledger, and it creates it owned by root — after which the
> service can read it and not write it, and the first thing you try (`user
> create`) fails with `attempt to write a readonly database`. This happened.
> The fix is `chown -R factory:factory /var/lib/mecha-factory`; the reason it
> cannot recur through the normal path is that the unit runs `check` as the
> service user.

**Start with `staging = true`.** Let's Encrypt's production rate limits are
per-week; the staging directory's are enormous and its certificates are trusted
by nobody. Confirm in the log that an order completed — the sequence to look
for is `trigger challenge` for each name, then `completed all authorizations`,
`sending csr`, `download certificate`, `DeployedNewCert`.

Then set `staging = false`, **delete the certificate cache**, and restart:

```sh
rm -rf /var/lib/mecha-factory/acme
systemctl restart mecha-factory
```

The cache holds the staging account *and* the staging certificate, and without
clearing it the server happily goes on serving one no browser trusts. Verify
from somewhere else entirely:

```sh
echo | openssl s_client -connect gate.<yours>:443 -servername gate.<yours> 2>/dev/null \
  | openssl x509 -noout -issuer -ext subjectAltName
```

One certificate covers all three names, and one more per active user.

## The keys

Every key belongs to a user, so minting one names them:

```sh
factory --config /etc/mecha-factory/factory.toml key create --handle alice --scope publish --label laptop
factory --config /etc/mecha-factory/factory.toml key create --handle alice --scope drain   --label laptop
```

Each prints its token **once**, on stdout, alone — so redirecting it to a file
is the whole installation procedure. There is no way to read one back; a "show
it again" verb would be a plaintext key at rest with extra steps.

At home:

```sh
install -d -m 0700 ~/.mecha/factory
# paste, or scp, into:
#   ~/.mecha/factory/publish.key   mode 0600
#   ~/.mecha/factory/drain.key     mode 0600
```

Rotation is mint, install, revoke — both keys work until the old one is
revoked, and `factory key revoke <id>` never deletes the row, because the row is
the record that the key existed and when it stopped.

## Patching

```sh
apt install unattended-upgrades
dpkg-reconfigure --priority=low unattended-upgrades
```

And **watch it from home rather than remembering to look**: a mecha trigger that
`GET`s `https://<gate>/v1/health` on a schedule and stages a warning when it is
not 200. Health is public precisely so that check costs nothing and keeps
working on a box where every key has just been rotated. With a key it also
reports **that user's** queue depth and account status — not the box's totals,
because how many strangers wrote to somebody else this week is not a fact this
endpoint owes anyone.

## Where it actually runs

| | |
|---|---|
| box | DigitalOcean, NYC, Ubuntu 24.04, 1 vCPU / 1 GB / 24 GB, 2 GB swap |
| gate | `https://gate.mecha-factory.ai` |
| artifacts | `https://<handle>.art.mecha-factory.ai` |
| compute | `https://<handle>.compute.mecha-factory.ai` |
| first user | `ljchang` |
| keys at home | `~/.mecha/factory/{publish,drain}.key`, mode 0600, with `config.toml` naming the gate |
| DNS | Squarespace, five A records, no proxy in front |

`mecha-factory.org` is registered and unused; the intention is to forward it.

## What is deliberately not here yet

- **The inbound form and its verification.** The box has no public write
  endpoint at all: nothing but a held key can put a row in the queue. That is
  step 7, and until it exists `factory queue add` on the box is the only writer
  — it validates against an uploaded type exactly as the form endpoint will.
- **Capability URLs for private bundles.** A private bundle is served to nobody
  and answers exactly what a bundle that never existed answers. The gate issuing
  short-lived URLs comes with the same step.
- **A real wildcard certificate**, and with it users who can sign up without a
  restart. See "The three names".
- **Backups.** The published bytes are also mirrored at home under
  `~/.mecha/bundles/`, so the box is not the only copy of anything that matters
  — but the ledger and the queue are only here. A nightly `sqlite3 .backup` of
  `factory.db` off the box is worth having before there is anything in the queue
  you would miss.
