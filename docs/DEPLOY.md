# Putting the factory on a box

The first thing in this project that creates a machine to patch forever. That
sentence is the whole reason this document exists: the failure mode of
forgetting is not "the site is down", it is "the site is someone else's".

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

```sh
# On the box, as root.
adduser --system --group --no-create-home factory
install -m 0755 factory /usr/local/bin/factory
install -d -m 0755 /etc/mecha-factory
install -m 0644 factory.toml /etc/mecha-factory/factory.toml
install -m 0644 mecha-factory.service /etc/systemd/system/

systemctl daemon-reload
systemctl enable --now mecha-factory
journalctl -u mecha-factory -f
```

The unit runs `factory check` before it starts, so a configuration typo fails
without stopping the server that is already running.

**Start with `staging = true`.** Let's Encrypt's production rate limits are
per-week; the staging directory's are enormous and its certificates are trusted
by nobody. Confirm in the log that an order completed, then set `staging =
false` and restart. The certificate cache lives in the data directory, so a
restart does not re-issue.

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
