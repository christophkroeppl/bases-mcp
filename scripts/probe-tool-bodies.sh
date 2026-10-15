#!/usr/bin/env bash
# Drive the same tools over stdio and over HTTP and compare the answers.
#
# Not just the schema: the actual tool bodies, including the query pipeline,
# the note projection and the error envelope.
set -uo pipefail

WT=/media/data/projects/ACTIVE/bases-mcp/feat-mcp-http-transport
BIN="$WT/target/debug/bases-mcp"
VAULT="$WT/test/vault"
PORT=8794

cd "$WT" || exit 1
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

                                                                               
                                                                           
                                                                              
                                             
: > /tmp/bodies.stdio
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"p","version":"0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  id=1
  for call in "${TOOL_CALLS[@]}"; do
    id=$((id + 1))
    printf '{"jsonrpc":"2.0","id":%d,"method":"tools/call","params":%s}\n' "$id" "$call"
  done
} | "$BIN" --transport stdio > /tmp/bodies.stdio 2>/dev/null

                                                                               
MCP_HTTP_PORT=$PORT "$BIN" --transport http > /dev/null 2>/dev/null &
HTTP_PID=$!
trap 'kill $HTTP_PID 2>/dev/null' EXIT
for _ in $(seq 1 50); do
  ss -ltn 2>/dev/null | grep -q ":$PORT " && break
  sleep 0.1
done

curl -sS -D /tmp/bodies.headers -o /dev/null -X POST "http://127.0.0.1:$PORT/mcp" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"p","version":"0"}}}' > /dev/null
SID=$(tr -d '\r' < /tmp/bodies.headers | awk -F': ' 'tolower($1)=="mcp-session-id"{print $2}')

curl -sS -o /dev/null -X POST "http://127.0.0.1:$PORT/mcp" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H "Mcp-Session-Id: $SID" \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'

# One request per call, so a failure names the call that failed.
: > /tmp/bodies.http
id=1
for call in "${TOOL_CALLS[@]}"; do
  id=$((id + 1))
  curl -sS -X POST "http://127.0.0.1:$PORT/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Mcp-Session-Id: $SID" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":$id,\"method\":\"tools/call\",\"params\":$call}" \
    | tr -d '\r' | grep '^data: {' | sed 's/^data: //' >> /tmp/bodies.http
done

python3 - <<'PY'
import json, sys

def by_id(path):
    out = {}
    for line in open(path):
        line = line.strip()
        if line:
            m = json.loads(line)
            out[m["id"]] = m
    return out

std = by_id("/tmp/bodies.stdio")
http = by_id("/tmp/bodies.http")

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
