#!/usr/bin/env bash
# Deploy one tagged release to this box, or roll back to the previous binary.
#
#   deploy.sh v0.2.0        # download, verify, swap, restart, health-check
#   deploy.sh --rollback    # reinstall /usr/local/bin/factory.prev
#
# This script exists because every step it runs was once typed by hand, and
# two of them went wrong on 2026-08-08: a wrong-architecture binary crash-
# looped the service (203/EXEC says nothing about why), and a config line
# appended in the wrong place was silently ignored. The rules it encodes:
#
#   - Only a release asset is ever installed — never a binary someone built.
#     The release workflow guarantees static musl x86-64; the checksum
#     guarantees this download is that build.
#   - The binary and the config are both proven BEFORE the service is
#     touched. A deploy that is going to fail must fail while the site is
#     still up.
#   - The previous binary survives as factory.prev, and a failed health
#     check reinstalls it without being asked. The rollback path runs on
#     every bad deploy, which is what keeps it working.
#
# Lives in the repo, installed on the box:
#   install -m 0755 scripts/deploy.sh /usr/local/bin/factory-deploy
set -euo pipefail

REPO="${REPO:-ljchang/mecha-factory}"
GATE="${GATE:-gate.mecha-factory.ai}"
CONFIG="${CONFIG:-/etc/mecha-factory/factory.toml}"
BIN=/usr/local/bin/factory
ASSET=factory-x86_64-linux-musl.tar.gz

log() { printf '\n== %s\n' "$*"; }

health() {
    # Through the front door but resolved to loopback: proves TLS, the
    # config, and the router without depending on hairpin routing.
    curl -fsS --max-time 5 --resolve "$GATE:443:127.0.0.1" "https://$GATE/" -o /dev/null
}

restore() {
    log "health check failed — rolling back"
    install -m 0755 "$BIN.prev" "$BIN"
    systemctl restart mecha-factory
    sleep 2
    if health; then
        echo "rolled back: the previous binary is serving again" >&2
    else
        echo "ROLLBACK DID NOT COME UP EITHER — the journal is the next stop:" >&2
        journalctl -u mecha-factory -n 20 --no-pager >&2
    fi
    exit 1
}

if [[ "${1:-}" == "--rollback" ]]; then
    [[ -f "$BIN.prev" ]] || { echo "no $BIN.prev to roll back to" >&2; exit 1; }
    install -m 0755 "$BIN.prev" "$BIN"
    systemctl restart mecha-factory
    sleep 2
    health && echo "rolled back and serving"
    exit
fi

TAG="${1:?usage: deploy.sh <tag>   (e.g. deploy.sh v0.2.0), or --rollback}"

WORK=$(mktemp -d "/root/deploy-$TAG.XXXX")
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

log "downloading $TAG"
curl -sSfLO "https://github.com/$REPO/releases/download/$TAG/$ASSET"
curl -sSfLO "https://github.com/$REPO/releases/download/$TAG/$ASSET.sha256"
sha256sum -c "$ASSET.sha256"
tar xzf "$ASSET"

log "proving the binary and the config before touching the service"
./factory --help >/dev/null
# `check` validates what the box is actually configured to be; a config
# mistake fails here, with the site still up, instead of after the restart.
./factory --config "$CONFIG" check

log "swapping (previous binary kept as factory.prev)"
cp "$BIN" "$BIN.prev"
install -m 0755 factory "$BIN"
systemctl restart mecha-factory

log "health check"
for _ in $(seq 1 10); do
    sleep 2
    if health; then
        echo "deployed $TAG and serving"
        exit 0
    fi
done
restore
