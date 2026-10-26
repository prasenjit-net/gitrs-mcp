#!/usr/bin/env bash
# End-to-end MCP check over stdio: initialize, list tools, call each Phase 0 tool.
# Usage: scripts/smoke.sh <path-to-gitrs-mcp-binary> <repo>
set -euo pipefail

bin=${1:?binary path required}
repo=${2:-.}

requests=(
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
  '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"repo_info","arguments":{}}}'
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"log","arguments":{"limit":3}}}'
  '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_refs","arguments":{"kind":"all"}}}'
  '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"show_commit","arguments":{}}}'
)

# Keep stdin open briefly so the server can answer before seeing EOF.
{ printf '%s\n' "${requests[@]}"; sleep 1; } \
  | "$bin" --repo "$repo" \
  | python3 -c '
import json, sys
failed = False
for line in sys.stdin:
    msg = json.loads(line)
    res = msg.get("result") or {}
    if "error" in msg or res.get("isError"):
        failed = True
    print(json.dumps(msg, indent=2))
sys.exit(1 if failed else 0)
'
