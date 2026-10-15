#!/usr/bin/env bash
# Drive the same tools over stdio and over HTTP and compare the answers.
#
# Not just the schema: the actual tool bodies, including the query pipeline,
# the note projection and the error envelope.
#
# Invoked by `just probe-tool-bodies`. Everything is resolved relative to this
# script, so the probe works from any worktree and any checkout.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$HERE/target/debug/bases-mcp"
VAULT="$HERE/test/vault"
PORT=8794

cd "$HERE" || exit 1

if [[ ! -x "$BIN" ]]; then
  echo "no debug binary at $BIN — run \`cargo build\` first" >&2
  exit 1
fi

export BASES_MCP_VAULT="$VAULT"

TOOL_CALLS=(
  '{"name":"resolve_base","arguments":{"base":"Tickets.base","context":"Root Ticket.md","format":"markdown"}}'
  '{"name":"resolve_base","arguments":{"base":"Tickets.base","context":"Root Ticket.md","format":"json"}}'
  '{"name":"resolve_base","arguments":{"base":"Tickets.base","context":"Root Ticket.md","format":"json","includeAllFormulas":true}}'
  '{"name":"get_note","arguments":{"path":"Root Project.md"}}'
  '{"name":"get_note","arguments":{"path":"Root Project.md","raw":true}}'
  '{"name":"backlinks","arguments":{"path":"Root Project.md"}}'
  '{"name":"resolve_base","arguments":{"base":"Nope.base"}}'
)

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"; kill "${HTTP_PID:-}" 2>/dev/null' EXIT

# The stdio run: one handshake, then one request per call.
: > "$WORK/bodies.stdio"
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"p","version":"0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  id=1
  for call in "${TOOL_CALLS[@]}"; do
    id=$((id + 1))
    printf '{"jsonrpc":"2.0","id":%d,"method":"tools/call","params":%s}\n' "$id" "$call"
  done
} | "$BIN" --transport stdio > "$WORK/bodies.stdio" 2>/dev/null

               
MCP_HTTP_PORT=$PORT "$BIN" --transport http > /dev/null 2> "$WORK/http.err" &
HTTP_PID=$!
for _ in $(seq 1 50); do
  ss -ltn 2>/dev/null | grep -q ":$PORT " && break
  sleep 0.1
done

curl -sS -D "$WORK/bodies.headers" -o /dev/null -X POST "http://127.0.0.1:$PORT/mcp" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"p","version":"0"}}}' > /dev/null
SID=$(tr -d '\r' < "$WORK/bodies.headers" | awk -F': ' 'tolower($1)=="mcp-session-id"{print $2}')

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

# One request per call, so a failure names the call that failed.
: > "$WORK/bodies.http"
id=1
for call in "${TOOL_CALLS[@]}"; do
  id=$((id + 1))
  curl -sS -X POST "http://127.0.0.1:$PORT/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Mcp-Session-Id: $SID" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":$id,\"method\":\"tools/call\",\"params\":$call}" \
    | tr -d '\r' | grep '^data: {' | sed 's/^data: //' >> "$WORK/bodies.http"
done

python3 - "$WORK" <<'PY'
import json, sys

work = sys.argv[1]

def by_id(name):
    out = {}
    for line in open(f"{work}/{name}"):
        line = line.strip()
        if line:
            message = json.loads(line)
            out[message["id"]] = message
    return out

std = by_id("bodies.stdio")
http = by_id("bodies.http")

ok = True
for id in sorted(k for k in std if k >= 2):
    s, h = std.get(id), http.get(id)
    if h is None:
        print(f"id {id}: MISSING over HTTP"); ok = False; continue
    if s != h:
        print(f"id {id}: DIFFERS")
        print("  stdio:", json.dumps(s)[:400])
        print("  http :", json.dumps(h)[:400])
        ok = False
    else:
        content = s["result"].get("structuredContent")
        summary = content.get("health", "?") if isinstance(content, dict) else "?"
        print(f"id {id}: identical (health={summary})")

print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
PY
