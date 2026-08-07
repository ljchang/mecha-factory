# Putting the factory on a box

The first thing in this project that creates a machine to patch forever. That
sentence is the whole reason this document exists: the failure mode of
forgetting is not "the site is down", it is "the site is someone else's".

> **Done once, on 2026-08-06.** A DigitalOcean droplet in NYC —
> Ubuntu 24.04, 1 vCPU, 1 GB RAM, x86_64 — serving
> `gate` / `art` / `compute` under `mecha-factory.ai`, with a Let's Encrypt
> certificate the binary obtained for itself. What follows is the procedure as
> it was actually run, including the two things that went wrong.
>
> **Issuance moved to HTTP-01 on 2026-08-07**, and with it one certificate per
> user ordered while the server runs. The consequence for a deployment is one
> sentence: **port 80 is now part of issuance and `[listen] http` is required
> beside `[tls]`.** A configuration without it is refused at startup rather
> than serving what it has cached and quietly never renewing.

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

The wildcards are how `alice.artifacts.example.org` resolves, and they are what
lets a brand-new handle answer an ACME challenge with no DNS work at all. The
*certificate* is a separate matter: neither challenge `rustls-acme` speaks can
issue a wildcard, so the server orders **one certificate per active user** —
two names, artifacts and compute — and reconciles the set against the ledger
every thirty seconds. A user created while it is running gets a certificate
without a restart, which is the whole of `SELF-SERVE.md` step 2.

The ceiling is Let's Encrypt's, and it is on signups rather than on the fleet:
50 *new* certificates per registered domain per week, refilling at one per 202
minutes. **Renewals are exempt**, so a large deployment costs nothing to keep
running — only to grow. A real wildcard certificate would remove even that, and
it needs DNS-01, and therefore a zone-scoped API token on the box; recorded in
§14.2 with its mitigation and deliberately not done.

**One consequence is a security property rather than a limitation, and it
survived the change.** A handle nobody owns has no certificate and no resolver,
so a request for one fails at the TLS handshake — a stranger cannot reach the
application at all. The 404 the server would have returned is the *second* line
of defence here, behind one the certificate gives for free. A wildcard
certificate would remove that first line, which is worth knowing before
treating DNS-01 as a pure upgrade.

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

### Moving the zone to Cloudflare

Worth doing, and it is not what unlocks wildcard certificates — that is
foreclosed by `rustls-acme`, which speaks only HTTP-01 and TLS-ALPN-01 (see
`SELF-SERVE.md`). The payoff is that **Squarespace has no API for custom
records**, so every row is typed by hand: the five SES rows were, and a
tenant's custom domain would be.

**The risk is not the factory, it is the website.** The apex and `www` point at
Squarespace's own hosting, and a nameserver change that drops them takes the
site down. Capture the zone before touching anything:

```sh
for n in mecha-factory.ai www gate art compute mail; do
  for t in A AAAA CNAME MX TXT; do dig +short $t ${n%.*}.mecha-factory.ai; done
done
```

What has to survive, as of 2026-08-07:

| Type | Name | Value |
|---|---|---|
| A | `@` | `198.49.23.144`, `198.49.23.145`, `198.185.159.144`, `198.185.159.145` (Squarespace) |
| CNAME | `www` | `ext-sq.squarespace.com` |
| A | `gate`, `art`, `compute` | `64.227.29.109` |
| A | `*.art`, `*.compute` | `64.227.29.109` |
| CNAME | ×3 `<token>._domainkey` | `<token>.dkim.amazonses.com` |
| MX | `mail` | `10 feedback-smtp.us-east-1.amazonses.com` |
| TXT | `mail` | `v=spf1 include:amazonses.com -all` |
| TXT | `@` | `v=spf1 -all` |
| TXT | `_dmarc` | `v=DMARC1; p=reject; sp=reject; adkim=s; aspf=s` |

The order that avoids an outage:

1. Add the zone at Cloudflare. Its scan imports most records — **check every
   row against the table above**, because a scan infers from public DNS and
   cannot see anything that was not resolving.
2. **Set every record to DNS-only (grey cloud).** Proxying terminates TLS,
   which means Cloudflare reads the plaintext of drained submissions. §13.2
   chose no CDN for exactly that reason, and note that it is now the *only*
   reason: HTTP-01 is an ordinary GET that a proxy forwards, so issuance would
   survive proxying where TLS-ALPN-01 did not. The objection got narrower and
   it did not get weaker — plaintext was always the serious half.
3. Lower TTLs and let the old ones expire before switching nameservers, so a
   rollback is minutes rather than hours.
4. Change the nameservers at the registrar.
5. Verify from outside: the website loads, `gate` still serves, and
   `aws sesv2 get-email-identity --email-identity mecha-factory.ai` still
   reports `SUCCESS` for DKIM and MAIL FROM. **A DKIM CNAME lost in the move
   does not bounce mail — it silently fails DMARC under `p=reject`**, which is
   the same as not sending.

Any API token for the box later should be scoped to this one zone with
`DNS:Edit`, and if DNS-01 is ever adopted, `_acme-challenge` should be
delegated by CNAME to a separate zone so the box's token cannot reach these
records at all.

### If the DNS is proxied at Cloudflare

Use **DNS only** — the grey cloud, not the orange one. Proxying changes two
things that this design deliberately decided against:

- **Cloudflare terminates TLS**, so it reads the plaintext of every request and
  response, including drained submissions. §13.2 of the design document chose
  no CDN specifically to avoid that; the honest cost is no DDoS absorption, and
  for a personal booking page that is an annoyance rather than a crisis.
- **Issuance survives, which it would not have before.** HTTP-01 is a plain GET
  on port 80 and a proxy forwards it; TLS-ALPN-01 lived inside the handshake
  the proxy answers, and would have forced DNS-01 and a zone-scoped API token
  onto the box we assume is lost. Worth stating because it *removes* an
  objection, and an argument that quietly keeps a retired reason is one nobody
  can check.

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

## Mail

The box sends exactly one kind of message: the verification link a stranger
gets after filling in a form. It goes through **Amazon SES**, and without it
the intake path renders and validates and then quietly goes nowhere.

```toml
# /etc/mecha-factory/factory.toml
[mail]
from        = "no-reply@mecha-factory.ai"
region      = "us-east-1"
credentials = "/etc/mecha-factory/ses.toml"
```

```toml
# /etc/mecha-factory/ses.toml — mode 0600, owned by the service user
access_key_id     = "AKIA…"
secret_access_key = "…"
```

The IAM user behind that key wants exactly one permission, `ses:SendEmail`.
It is the first credential the box holds that can act as your domain, so it
should be able to do nothing else — the claim worth preserving is not "the box
holds no secrets" but **"the box holds no credential that reaches home"**.

`[mail]` present and unreadable **stops the box**, with the reason. It does not
fall back to writing links to the journal: that would look like a working
deployment while every stranger's link went nowhere. `[mail]` absent is a
different thing and is honest about itself — links go to the journal, an
operator can complete a verification by hand, and `factory check` says so:

```
mail      log — links are written to the journal and not sent
mail      Amazon SES — us-east-1 from no-reply@mecha-factory.ai
mail      MISCONFIGURED — the box will refuse to start: …
```

### Setting SES up

Two facts about this domain make the generic SES walkthrough wrong, and both
were true before any of this was built:

```
mecha-factory.ai        TXT   "v=spf1 -all"
_dmarc.mecha-factory.ai TXT   "v=DMARC1; p=reject; sp=reject; adkim=s; aspf=s"
```

`v=spf1 -all` says **this domain sends no mail, reject anything claiming to**.
And the DMARC policy is the strictest one that exists: `p=reject` (not
quarantine — *reject*), with `adkim=s` and `aspf=s` demanding **strict**
alignment, where the signing domain must equal the From domain exactly rather
than merely share an organisational parent.

So the usual failure here is not "the link went to spam". It is that receiving
servers **refuse the message outright** and the stranger gets nothing, while
SES reports a successful send. That is worth knowing before debugging the box.

What satisfies it: **Easy DKIM signing as `mecha-factory.ai` itself**, which
gives `d=mecha-factory.ai` and aligns strictly. DMARC passes when *either* SPF
or DKIM aligns, so DKIM alone is sufficient — but it also means DKIM is the
single point of failure, which is the tradeoff in step 6.

**The DNS is at Squarespace** (`nsd1–4.squarespacedns.com`), which has no
public API for custom records. Every DNS row below is typed into their web
panel by hand; only the AWS half is scriptable.

**1. The CLI.** Already installed at `~/.local/aws-cli` (v2). Add it to your
path and give it a credential — an admin key, for setup only; the box gets its
own much smaller one in step 7.

```sh
export PATH="$HOME/.local/bin:$PATH"
aws configure          # key, secret, region, json
```

Pick the region once and use it everywhere. It goes in `[mail] region`, it is
half the SES endpoint host, and it is part of the SigV4 signing scope — a
mismatch is a 403 that says nothing about which of the three is wrong.

**2. The domain identity, with Easy DKIM.**

```sh
aws sesv2 create-email-identity \
  --email-identity mecha-factory.ai \
  --dkim-signing-attributes NextSigningKeyLength=RSA_2048_BIT
```

**3. The DKIM records.** Three tokens, three CNAMEs:

```sh
aws sesv2 get-email-identity --email-identity mecha-factory.ai \
  --query 'DkimAttributes.Tokens' --output text
```

For each token, add at Squarespace — **CNAME**, and note the host has no
trailing domain in most panels (Squarespace appends the zone itself):

| Type | Host | Value |
|---|---|---|
| CNAME | `<token1>._domainkey` | `<token1>.dkim.amazonses.com` |
| CNAME | `<token2>._domainkey` | `<token2>.dkim.amazonses.com` |
| CNAME | `<token3>._domainkey` | `<token3>.dkim.amazonses.com` |

**4. A custom MAIL FROM subdomain.** Without one, the envelope sender is
`amazonses.com`, so SPF authenticates a domain that is not yours and the
bounce path is shared. With one, bounces come back to you and SPF has
something of yours to check.

```sh
aws sesv2 put-email-identity-mail-from-attributes \
  --email-identity mecha-factory.ai \
  --mail-from-domain mail.mecha-factory.ai \
  --behavior-on-mx-failure USE_DEFAULT_VALUE
```

Two more rows, on the `mail` subdomain — **not** the root:

| Type | Host | Value |
|---|---|---|
| MX | `mail` | `10 feedback-smtp.us-east-1.amazonses.com` |
| TXT | `mail` | `v=spf1 include:amazonses.com -all` |

Substitute your region into the MX value. **Leave the root `v=spf1 -all`
alone** — it is correct, and now accurate: nothing uses the bare domain as an
envelope sender.

**5. Wait, then check.** Propagation is usually minutes; SES rechecks on its
own schedule and can take up to 72 hours to flip.

```sh
aws sesv2 get-email-identity --email-identity mecha-factory.ai \
  --query '{Sending:VerifiedForSendingStatus, DKIM:DkimAttributes.Status, MailFrom:MailFromAttributes.MailFromDomainStatus}'
```

Wanted: `Sending: true`, `DKIM: SUCCESS`, `MailFrom: SUCCESS`. Independently:

```sh
dig +short CNAME <token1>._domainkey.mecha-factory.ai
dig +short TXT mail.mecha-factory.ai
dig +short MX  mail.mecha-factory.ai
```

**6. A DMARC decision, and it is a real one.** Under `aspf=s`, a MAIL FROM of
`mail.mecha-factory.ai` does **not** strictly align with a From of
`mecha-factory.ai` — subdomains only satisfy *relaxed* alignment. So SPF will
not contribute to DMARC and every message rides on DKIM alone. One rotated key
or one stripped signature is then total delivery failure, under `p=reject`.

Relaxing SPF alignment restores the second leg and gives up very little, since
the subdomain is one you control:

```
_dmarc   TXT   v=DMARC1; p=reject; sp=reject; adkim=s; aspf=r; rua=mailto:dmarc@mecha-factory.ai
```

Adding `rua=` is worth more than it looks: aggregate reports are the only way
to find out that something is failing DMARC without a person telling you their
link never arrived. Leaving `aspf=s` is a defensible choice — just a deliberate
one, made knowing DKIM is then load-bearing alone.

**7. The box's own credential**, which is not the admin key from step 1. One
action, one identity, one From address:

```sh
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
REGION=$(aws configure get region)

cat > /tmp/ses-send.json <<JSON
{"Version":"2012-10-17","Statement":[{
  "Effect":"Allow",
  "Action":["ses:SendEmail"],
  "Resource":"arn:aws:ses:${REGION}:${ACCOUNT}:identity/mecha-factory.ai",
  "Condition":{"StringEquals":{"ses:FromAddress":"no-reply@mecha-factory.ai"}}
}]}
JSON

aws iam create-user --user-name mecha-factory-mail
aws iam put-user-policy --user-name mecha-factory-mail \
  --policy-name ses-send-only --policy-document file:///tmp/ses-send.json
aws iam create-access-key --user-name mecha-factory-mail
```

That last command prints the secret **once**. It goes to
`/etc/mecha-factory/ses.toml` on the box at mode 0600 and nowhere else. The
`Condition` is what makes a stolen key uninteresting: it can send as
`no-reply@` and it cannot send as you.

**8. Out of the sandbox.** A new account may only send to *verified*
addresses. Sends to anyone else are **accepted by the API and never
delivered**, so the box looks healthy while no stranger gets a link. This is
the failure this project refuses everywhere else, and it is AWS's to own.

```sh
aws sesv2 get-account --query '{Production:ProductionAccessEnabled, Quota:SendQuota}'

aws sesv2 put-account-details \
  --production-access-enabled \
  --mail-type TRANSACTIONAL \
  --website-url https://mecha-factory.ai \
  --contact-language EN \
  --use-case-description "Double opt-in confirmation links for a personal \
request intake form. One message per submission, sent only to the address \
just entered, in response to that action. No marketing, no lists, no bulk."
```

Review is usually within 24 hours. Until it clears, verify your own address so
there is something to test against:

```sh
aws sesv2 create-email-identity --email-identity you@example.edu
```

**9. End to end.** Point `[mail]` at the credential, then:

```sh
factory --config /etc/mecha-factory/factory.toml check   # expect the SES line
```

Submit a real form to a verified address, click the link, and confirm the row
moves. Then check the headers of what arrived: `DKIM-Signature` should carry
`d=mecha-factory.ai`, and `Authentication-Results` should show `dkim=pass` and
`dmarc=pass`. **An end-to-end test against your own verified address proves
nothing about strangers while the account is in the sandbox** — that is what
step 8 is for.

### When it fails

| What you see | Almost always |
|---|---|
| `403` / `SignatureDoesNotMatch` | `[mail] region` disagrees with the identity's region, or the box's clock has drifted — SigV4 signs a timestamp |
| `MessageRejected: Email address is not verified` | still in the sandbox (step 8), or `from` is not the verified identity |
| `AccessDenied` on send | the IAM condition — `from` in the config is not the address the policy pins |
| SES says sent, nothing arrives, no bounce | `p=reject` did its job: check `Authentication-Results` on any copy you can get, and re-check DKIM status |
| Was working, now silently rejected | someone edited the root SPF, or the DKIM CNAMEs were dropped in a DNS migration |

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
- **A real wildcard certificate.** Not needed for signup any more — one
  certificate per user, reconciled from the ledger, closed that. It would raise
  the ceiling from 50 new certificates a week to none at all, and it needs
  DNS-01. See "The three names".
- **Backups.** The published bytes are also mirrored at home under
  `~/.mecha/bundles/`, so the box is not the only copy of anything that matters
  — but the ledger and the queue are only here. A nightly `sqlite3 .backup` of
  `factory.db` off the box is worth having before there is anything in the queue
  you would miss.
