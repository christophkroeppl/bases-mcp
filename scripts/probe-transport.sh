#!/usr/bin/env bash
# Compare the MCP tool surface over stdio and over HTTP, byte for byte.
#
# Same vault, same binary, two transports. If the schemas differ, the
# requirement "identical over HTTP and stdio" is broken.
#
# Invoked by `just probe-transport`. Everything is resolved relative to this
# script, so the probe works from any worktree and any checkout without an
# author-specific path in it.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$HERE/target/debug/bases-mcp"
VAULT="$HERE/test/vault"
PORT=8793

cd "$HERE" || exit 1

if [[ ! -x "$BIN" ]]; then
  echo "no debug binary at $BIN — run \`cargo build\` first" >&2
  exit 1
fi

export BASES_MCP_VAULT="$VAULT"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"; kill "${HTTP_PID:-}" 2>/dev/null' EXIT

# The stdio run: one process, four messages, four answers.
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"p","version":"0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_bases","arguments":{}}}'
} | "$BIN" --transport stdio > "$WORK/stdio.out" 2> "$WORK/stdio.err"

# The HTTP run: initialize, then the same two calls on the session it issued.
MCP_HTTP_PORT=$PORT "$BIN" --transport http > "$WORK/http.out" 2> "$WORK/http.err" &
HTTP_PID=$!

for _ in $(seq 1 50); do
  ss -ltn 2>/dev/null | grep -q ":$PORT " && break
  sleep 0.1
done

curl -sS -D "$WORK/http-init.headers" -o "$WORK/http-init.body" -X POST "http://127.0.0.1:$PORT/mcp" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"p","version":"0"}}}' > /dev/null

SID=$(tr -d '\r' < "$WORK/http-init.headers" | awk -F': ' 'tolower($1)=="mcp-session-id"{print $2}')

if [ -z "$SID" ]; then
  echo "FAIL: no Mcp-Session-Id from initialize" >&2
  sed -n '1,20p' "$WORK/http.err" >&2
  exit 1
fi
curl -sS -o /dev/null -X POST "http://127.0.0.1:$PORT/mcp" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H "Mcp-Session-Id: $SID" \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'

curl -sS -X POST "http://127.0.0.1:$PORT/mcp" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H "Mcp-Session-Id: $SID" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' > "$WORK/http-tools.out"

curl -sS -X POST "http://127.0.0.1:$PORT/mcp" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H "Mcp-Session-Id: $SID" \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_bases","arguments":{}}}' > "$WORK/http-call.out"

python3 - "$WORK" <<'PY'
import json, sys

work = sys.argv[1]

def read(name):
    with open(f"{work}/{name}") as handle:
        return handle.read()

def stdio_lines():
    return [json.loads(line) for line in read("stdio.out").splitlines() if line.strip()]

def sse_data(path):
    """Pull the JSON-RPC payload out of a text/event-stream body."""
    for line in read(path).splitlines():
        line = line.rstrip("\r")
        if line.startswith("data: ") and line.strip() != "data:":
            payload = line[len("data: "):]
            if payload.startswith("{"):
                return json.loads(payload)
    raise SystemExit(f"no JSON-RPC payload in {path}")

std = {m["id"]: m for m in stdio_lines() if "id" in m}
http_init = sse_data("http-init.body")
http_tools = sse_data("http-tools.out")
http_call = sse_data("http-call.out")

ok = True

# 1. tools/list: identical tool names, order, and input schemas.
st, ht = std[2]["result"]["tools"], http_tools["result"]["tools"]
if [t["name"] for t in st] != [t["name"] for t in ht]:
    print("FAIL: tool names/order differ"); ok = False
else:
    for a, b in zip(st, ht):
        if a["inputSchema"] != b["inputSchema"]:
            print(f"FAIL: inputSchema differs for {a['name']}"); ok = False
        if a.get("title") != b.get("title") or a.get("description") != b.get("description"):
            print(f"FAIL: title/description differ for {a['name']}"); ok = False
    if ok:
        print(f"tools/list: {len(st)} tools, names/order/schemas identical")

# 2. tools/call list_bases: identical result.
sc, hc = std[3]["result"], http_call["result"]
if sc != hc:
    print("FAIL: tools/call list_bases differs"); ok = False
else:
    print("tools/call list_bases: identical result")

# 3. initialize: same server identity and capabilities.
si, hi = std[1]["result"], http_init["result"]
for field in ("serverInfo", "capabilities", "protocolVersion"):
    if si.get(field) != hi.get(field):
        print(f"FAIL: {field} differs"); ok = False
if ok:
    print(f"initialize: protocolVersion/serverInfo/capabilities identical ({si['serverInfo']['name']} {si['serverInfo']['version']})")

print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
PY
