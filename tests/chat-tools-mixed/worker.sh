#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace and scripted mixed-tool turn"
llm_test_setup

mkdir -p /tmp/ws
echo "fixture" > /tmp/ws/original.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")

MIXED_CALLS='[
 {"id":"tu_mw","input":{"file-path":"mix.txt","content":"hello"},"name":"write","type":"tool_use"},
 {"id":"tu_mb","input":{"cmd":"tr a-z A-Z < mix.txt > mix3.txt","paths":["mix.txt"]},"name":"bash","type":"tool_use"},
 {"id":"tu_me","input":{"file-path":"mix.txt","old-string":"hello","new-string":"world"},"name":"edit","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$MIXED_CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"mixed done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

new_llm_conversation tools-mixed "$STUB_PORT" "$ws"
dispatch_turn "mix inline and bash"
wait_turn || fail "the mixed turn never reached a terminal head"

workspace=$(workspace_commit "$head")
fetch_code "$workspace" "fetching the mixed-tool workspace"
[ "$(git show "$workspace:mix.txt")" = "world" ] \
  || fail "post-bash edit did not land"
[ "$(git show "$workspace:mix3.txt")" = "HELLO" ] \
  || fail "bash did not see inline write"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/mixed-tools.jsonl
write_output=$(jq -r 'select(.id == "tu_mw") | .workspace_resolution.output' \
  /tmp/mixed-tools.jsonl)
assert_oid "$write_output" "inline write workspace"
jq -s -e --arg input "$write_output" '
  (map(.id) | sort) == (["tu_mw","tu_mb","tu_me"] | sort) and
  (map(select(.id == "tu_mb"))[0] |
    .status == "complete" and .task != null and
    .input_workspace == $input and .workspace_resolution.kind == "direct")
' /tmp/mixed-tools.jsonl >/dev/null || { cat /tmp/mixed-tools.jsonl >&2; fail "mixed tool records are wrong"; }
$TOOL parents --repo /tmp/repo --head "$head" > /tmp/mixed.parents
start_found=0
while read -r parent_oid parent_kind; do
  if [ "$parent_kind" = tool.start ] && \
      $TOOL tools --repo /tmp/repo --head "$parent_oid" --request "$request" \
        | jq -e 'select(.id == "tu_mb" and .status == "started")' >/dev/null; then
    start_found=1
  fi
done < /tmp/mixed.parents
[ "$start_found" -eq 1 ] || fail "tu_mb tool.start is absent from the spine"

sequence=$(grep -o '"tool_use_id":"tu_m[wbe]"' /tmp/stub/request-2.json \
  | grep -o 'tu_m[wbe]' | paste -sd,)
[ "$sequence" = "tu_mw,tu_mb,tu_me" ] \
  || fail "results missing or misordered: $sequence"
[ ! -f /tmp/stub/request-3.json ] || fail "unexpected extra model round"

pass chat-tools-mixed
