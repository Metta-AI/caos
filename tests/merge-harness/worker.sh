#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace, feature, and scripted model"
llm_test_setup

rm -rf /tmp/ws /tmp/feat
mkdir -p /tmp/ws /tmp/feat
echo v1 > /tmp/ws/f.txt
echo v1 > /tmp/feat/f.txt
echo hello > /tmp/feat/g.txt
base_tree=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")
feature_tree=$(publish_tree /tmp/feat /cas/feat "publishing the feature tree")

R1='[{"id":"toolu_01","input":{"theirs":"feature"},"name":"merge","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"merged the feature branch","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

new_llm_conversation merge "$STUB_PORT" "$base_tree"
feature=$(mint_commit /cas/feature "$feature_tree" feature "$base")
fetch_code "$feature" "fetching the feature commit"
git push -q caos "$feature:refs/caos/req/$feature" || fail "pushing feature closure"
llm=$(caos curry --base:hash="$llm" --merge-refs="feature $feature") \
  || fail "currying merge refs"

stage "dispatch merge turn"
dispatch_turn "merge in the feature branch"
wait_turn || fail "the merge turn never reached a terminal head"
workspace=$(workspace_commit "$head")
fetch_code "$workspace" "fetching the merged workspace"
git merge-base --is-ancestor "$feature" "$workspace" \
  || fail "feature is not an ancestor of the resulting main pointer"
[ "$(git show "$workspace:g.txt")" = hello ] || fail "merged file g.txt missing"
[ "$(git show "$workspace:f.txt")" = v1 ] || fail "f.txt changed"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/merge.tools
jq -e 'select(.name == "merge") |
  (.workspace_resolution.kind == "merged" or .workspace_resolution.kind == "direct")' \
  /tmp/merge.tools >/dev/null || fail "merge resolution was not recorded"
grep -qF 'merged the feature branch' <<<"$(transcript_text "$head")" \
  || fail "assistant transcript missing"
assert_spine "$head"

pass merge-harness
