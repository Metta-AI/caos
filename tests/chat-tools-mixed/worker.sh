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
cp -R /tmp/ws /tmp/feat
echo "merged" > /tmp/feat/feature.txt
feature_tree=$(publish_tree /tmp/feat /cas/feat "publishing the feature tree")

MIXED_CALLS='[
 {"id":"tu_mw","input":{"file-path":"mix.txt","content":"hello"},"name":"write","type":"tool_use"},
 {"id":"tu_mb","input":{"cmd":"tr a-z A-Z < mix.txt > mix3.txt","paths":["mix.txt"]},"name":"bash","type":"tool_use"},
 {"id":"tu_me","input":{"file-path":"mix.txt","old-string":"hello","new-string":"world"},"name":"edit","type":"tool_use"},
 {"id":"tu_mg","input":{"pattern":"world"},"name":"grep","type":"tool_use"},
 {"id":"tu_mm","input":{"theirs":"feature"},"name":"merge","type":"tool_use"},
 {"id":"tu_ml","input":{"count":"10"},"name":"log","type":"tool_use"},
 {"id":"tu_md","input":{},"name":"diff","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$MIXED_CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"mixed done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

# A long commit body also exercises log's subject extraction under pipefail.
history_message=$(printf 'history fixture\n'; printf 'body line\n%.0s' {1..20000})
history_base=$(mint_commit /cas/history-base "$ws" "$history_message")
new_llm_conversation tools-mixed "$STUB_PORT" - "You are a coding agent." "$history_base"
feature=$(mint_commit /cas/feature "$feature_tree" feature "$base")
fetch_code "$feature" "fetching the feature commit"
git push -q caos "$feature:refs/caos/req/$feature" || fail "pushing feature closure"
llm=$(caos curry --base:hash="$llm" --merge-refs="feature $feature") \
  || fail "currying merge refs"
dispatch_turn "write, run bash, edit, grep, and merge"
wait_turn || fail "the mixed turn never reached a terminal head"

workspace=$(workspace_commit "$head")
fetch_code "$workspace" "fetching the mixed-tool workspace"
[ "$(git show "$workspace:mix.txt")" = "world" ] \
  || fail "post-bash edit did not land"
[ "$(git show "$workspace:mix3.txt")" = "HELLO" ] \
  || fail "bash did not see inline write"
[ "$(git show "$workspace:feature.txt")" = "merged" ] \
  || fail "feature file did not land"
git merge-base --is-ancestor "$feature" "$workspace" \
  || fail "feature is not an ancestor of the resulting workspace"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/mixed-tools.jsonl
write_output=$(jq -r 'select(.id == "tu_mw") | .workspace_resolution.output' \
  /tmp/mixed-tools.jsonl)
assert_oid "$write_output" "inline write workspace"
jq -s -e --arg input "$write_output" '
  (map(.id) | sort) == (["tu_mw","tu_mb","tu_me","tu_mg","tu_mm","tu_ml","tu_md"] | sort) and
  (map(select(.id == "tu_mb"))[0] |
    .status == "complete" and .task != null and
    .input_workspace == $input and .workspace_resolution.kind == "direct") and
  (map(select(.id == "tu_mg"))[0] |
    .status == "complete" and .result.proposal == null) and
  (map(select(.id == "tu_mm"))[0] |
    .status == "complete" and
    (.workspace_resolution.kind == "merged" or .workspace_resolution.kind == "direct"))
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

sequence=$(grep -o '"tool_use_id":"tu_m[wbegmld]"' /tmp/stub/request-2.json \
  | grep -o 'tu_m[wbegmld]' | paste -sd,)
[ "$sequence" = "tu_mw,tu_mb,tu_me,tu_mg,tu_mm,tu_ml,tu_md" ] \
  || fail "results missing or misordered: $sequence"
grep -qF 'mix.txt:1:world' /tmp/stub/request-2.json \
  || fail "grep did not report the post-edit content to the model"
jq -e '[.messages[].content[]? | select(.type == "tool_result" and (.tool_use_id == "tu_ml" or .tool_use_id == "tu_md"))] | length == 2 and all(.is_error != true)' \
  /tmp/stub/request-2.json >/dev/null || fail "history tools failed after merge"
grep -qF 'history fixture' /tmp/stub/request-2.json \
  || fail "history log did not reach the initial commit"
grep -qF '+merged' /tmp/stub/request-2.json \
  || fail "history diff did not report the merge change"
[ ! -f /tmp/stub/request-3.json ] || fail "unexpected extra model round"

pass chat-tools-mixed
