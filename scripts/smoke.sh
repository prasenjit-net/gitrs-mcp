#!/usr/bin/env bash
# End-to-end MCP check over stdio: initialize, list tools, call every read tool once, list and read
# resources, complete an argument and get prompts, then start a second session with --allow-write and check the
# write tools appear only there, and a third with --watch that subscribes to a resource.
# Usage: scripts/smoke.sh <path-to-gitrs-mcp-binary> <repo> [file]
# `file` is a tracked file for read_file, blame and file_history (default README.md).
# Calls whose result depends on what the repo contains (a tag to show, ...) ask for something that
# can't exist and must come back as a tool error, so the check works on any repository. Write tools
# are only called in ways that can't change it.
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
  "$(call 16 describe '{}')"
  "$(call 17 show_tag '{"name":"gitrs-smoke-no-such-tag"}')"
  "$(call 18 reflog '{"limit":3}')"
  "$(call 19 list_stashes '{"limit":3}')"
  "$(call 20 notes '{}')"
  "$(call 21 grep '{"pattern":"\\w","limit":3}')"
  "$(call 22 list_worktrees '{}')"
  "$(call 23 list_submodules '{}')"
  "$(call 24 config_get '{"section":"core","limit":3}')"
  "$(call 25 count_objects '{}')"
  "$(call 26 check_ignore '{"paths":["'"$file"'","target/x"]}')"
  "$(call 27 check_attr '{"paths":["'"$file"'"]}')"
  "$(call 28 conflicts '{}')"
  "$(call 29 merge_preview '{"theirs":"HEAD"}')"
  '{"jsonrpc":"2.0","id":30,"method":"resources/list"}'
  '{"jsonrpc":"2.0","id":31,"method":"resources/templates/list"}'
  '{"jsonrpc":"2.0","id":32,"method":"resources/read","params":{"uri":"gitrs://commit/HEAD"}}'
  '{"jsonrpc":"2.0","id":33,"method":"resources/read","params":{"uri":"gitrs://file/HEAD/'"$file"'"}}'
  '{"jsonrpc":"2.0","id":34,"method":"resources/read","params":{"uri":"gitrs://commit/gitrs-smoke-no-such-rev"}}'
  '{"jsonrpc":"2.0","id":35,"method":"completion/complete","params":{"ref":{"type":"ref/resource","uri":"gitrs://commit/{rev}"},"argument":{"name":"rev","value":"H"}}}'
  '{"jsonrpc":"2.0","id":36,"method":"prompts/list"}'
  '{"jsonrpc":"2.0","id":37,"method":"prompts/get","params":{"name":"explain_history","arguments":{"path":"'"$file"'","limit":"3"}}}'
  # HEAD..HEAD has nothing to review, whatever the repository holds.
  '{"jsonrpc":"2.0","id":38,"method":"prompts/get","params":{"name":"review_changes","arguments":{"from":"HEAD","to":"HEAD"}}}'
  # Subscriptions need --watch; the third session below has it.
  '{"jsonrpc":"2.0","id":39,"method":"resources/subscribe","params":{"uri":"gitrs://commit/HEAD"}}'
  "$(call 40 list_repos '{}')"
  "$(call 41 log '{"limit":1,"repo":"gitrs-smoke-no-such-repo"}')"
)
last_id=41
expect_error='17 34 38 39 41' # space-separated request ids that must fail: a tool error, or a protocol error

write_tools='create_branch delete_branch rename_branch create_tag stage unstage commit reset restore switch clean stash_push stash_pop stash_drop merge cherry_pick revert rebase continue_operation abort_operation worktree_add worktree_remove'
write_requests=(
  "${requests[@]:0:3}"
  # A dry run (the default) on a branch that can't exist: refused, and nothing would change anyway.
  "$(call 3 delete_branch '{"name":"gitrs-smoke-no-such-branch"}')"
  "$(call 4 restore '{"paths":["gitrs-smoke-no-such-path"]}')"
  # A dry run (the default) limited to a path that can't exist: lists nothing.
  "$(call 5 clean '{"paths":["gitrs-smoke-no-such-path"]}')"
  "$(call 6 stash_drop '{"index":99999}')"
  # Nothing is in progress in a repository being smoke-tested, so these are refused.
  "$(call 7 continue_operation '{}')"
  "$(call 8 abort_operation '{}')"
  "$(call 9 worktree_remove '{"worktree":"gitrs-smoke-no-such-worktree"}')"
)

# Run one server session: server flags, last request id, ids that must error, whether write tools must
# be listed, then the requests.
session() {
  local flags=$1 last=$2 errors=$3 writes=$4
  shift 4
  # Keep stdin open briefly so the server can answer before seeing EOF.
  # shellcheck disable=SC2086 # `flags` is a deliberately split list of options
  { printf '%s\n' "$@"; sleep 1; } \
    | "$bin" --repo "$repo" $flags \
    | python3 -c '
import json, sys
last_id, want_error = int(sys.argv[1]), {int(i) for i in sys.argv[2].split()}
write_tools, writes_expected = set(sys.argv[3].split()), sys.argv[4] == "yes"
failed = False
expected = set(range(1, last_id + 1))
for line in sys.stdin:
    msg = json.loads(line)
    msg_id = msg.get("id")
    expected.discard(msg_id)
    res = msg.get("result") or {}
    if ("error" in msg or bool(res.get("isError"))) != (msg_id in want_error):
        print(f"unexpected outcome for request id {msg_id}", file=sys.stderr)
        failed = True
    if "tools" in res:
        listed = write_tools & {tool["name"] for tool in res["tools"]}
        if listed != (write_tools if writes_expected else set()):
            print(f"write tools listed: {sorted(listed)} (expected: {writes_expected})", file=sys.stderr)
            failed = True
    print(json.dumps(msg, indent=2))
if expected:
    print(f"no response for request ids {sorted(expected)}", file=sys.stderr)
    failed = True
sys.exit(1 if failed else 0)
' "$last" "$errors" "$write_tools" "$writes"
}

session "" "$last_id" "$expect_error" no "${requests[@]}"
session --allow-write 9 "3 4 6 7 8 9" yes "${write_requests[@]}"
session --watch 3 "3" no \
  "${requests[@]:0:2}" \
  '{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{"uri":"gitrs://commit/HEAD"}}' \
  '{"jsonrpc":"2.0","id":3,"method":"resources/subscribe","params":{"uri":"gitrs://nope"}}' 
