#!/usr/bin/env bash
# End-to-end MCP check over stdio: initialize, list tools, call every tool once.
# Usage: scripts/smoke.sh <path-to-gitrs-mcp-binary> <repo> [file]
# `file` is a tracked file for read_file, blame and file_history (default README.md).
set -euo pipefail

bin=${1:?binary path required}
repo=${2:-.}
file=${3:-README.md}

call() { # id, tool name, JSON arguments
  printf '{"jsonrpc":"2.0","id":%s,"method":"tools/call","params":{"name":"%s","arguments":%s}}' "$1" "$2" "$3"
}

requests=(
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
  '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  "$(call 3 repo_info '{}')"
  "$(call 4 log '{"limit":3}')"
  "$(call 5 list_refs '{"kind":"all"}')"
  "$(call 6 show_commit '{}')"
  "$(call 7 status '{"limit":5}')"
  "$(call 8 diff '{"limit":5}')"
  "$(call 9 read_file '{"path":"'"$file"'","max_bytes":200}')"
  "$(call 10 list_tree '{"limit":5}')"
  "$(call 11 rev_parse '{"spec":"HEAD"}')"
  "$(call 12 file_history '{"path":"'"$file"'","limit":3}')"
  "$(call 13 search_commits '{"limit":3}')"
  "$(call 14 compare_refs '{"base":"HEAD","head":"HEAD"}')"
  "$(call 15 blame '{"path":"'"$file"'","line_range":[1,1]}')"
)

# Keep stdin open briefly so the server can answer before seeing EOF.
{ printf '%s\n' "${requests[@]}"; sleep 1; } \
  | "$bin" --repo "$repo" \
  | python3 -c '
import json, sys
failed = False
expected = set(range(1, 16))
for line in sys.stdin:
    msg = json.loads(line)
    expected.discard(msg.get("id"))
    res = msg.get("result") or {}
    if "error" in msg or res.get("isError"):
        failed = True
    print(json.dumps(msg, indent=2))
if expected:
    print(f"no response for request ids {sorted(expected)}", file=sys.stderr)
    failed = True
sys.exit(1 if failed else 0)
'
