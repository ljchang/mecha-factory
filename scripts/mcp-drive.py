#!/usr/bin/env python3
"""Drive factory-publish's MCP surface over stdio, as any MCP client would.

The smoke test for the second-client story (docs/SECOND-CLIENT.md): no MCP
library, just the JSON-RPC a client actually speaks, so what passes here
passes for a client that brings nothing of its own.

Usage: mcp-drive.py <root> <request.json>...
Each request file holds one JSON object with "method" and optional "params";
requests are sent in order after the initialize handshake, and each response
is printed as pretty JSON. E.g.:

    echo '{"method":"tools/list","params":{}}' > /tmp/list.json
    scripts/mcp-drive.py "$PWD" /tmp/list.json
"""
import json, shutil, subprocess, sys, os

root = sys.argv[1]
reqs = [json.load(open(p)) for p in sys.argv[2:]]

binary = (
    os.environ.get("FACTORY_PUBLISH_BIN")
    or shutil.which("factory-publish")
    or os.path.expanduser("~/.cargo/bin/factory-publish")
)
proc = subprocess.Popen(
    [binary, "mcp", "--root", root],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
)

def rpc(id_, method, params):
    msg = {"jsonrpc": "2.0", "id": id_, "method": method, "params": params}
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()
    while True:
        line = proc.stdout.readline()
        if not line:
            print(f"!! server closed stdout after {method}", file=sys.stderr)
            sys.exit(1)
        resp = json.loads(line)
        if resp.get("id") == id_:
            return resp

def notify(method, params):
    msg = {"jsonrpc": "2.0", "method": method, "params": params}
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()

init = rpc(0, "initialize", {
    "protocolVersion": "2024-11-05",
    "capabilities": {},
    "clientInfo": {"name": "claude-code-drive", "version": "0.0.1"},
})
print("== initialize ==")
print(json.dumps(init.get("result", init), indent=2))
notify("notifications/initialized", {})

for i, r in enumerate(reqs, start=1):
    print(f"\n== {r['method']} ==")
    resp = rpc(i, r["method"], r.get("params", {}))
    print(json.dumps(resp.get("result", resp), indent=2))

proc.stdin.close()
proc.wait(timeout=10)
