#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "source tree and scripted mixed-tool turn"
llm_test_setup

mkdir -p /tmp/ws
echo "fixture" > /tmp/ws/original.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the source tree")
cp -R /tmp/ws /tmp/feat
echo "merged" > /tmp/feat/feature.txt
feature_tree=$(publish_tree /tmp/feat /cas/feat "publishing the feature tree")

MIXED_CALLS='[
 {"id":"tu_mw","input":{"file-path":"main/mix.txt","content":"hello"},"name":"write","type":"tool_use"},
 {"id":"tu_create","input":{"cmd":"cp -a main side; mkdir -p memories; echo remembered > memories/test.txt","paths":["main"]},"name":"bash","type":"tool_use"},
 {"id":"tu_mb","input":{"cmd":"tr a-z A-Z < side/mix.txt > main/mix3.txt; echo second > side/second.txt; mv side review; cp -a review side","paths":["main","side"]},"name":"bash","type":"tool_use"},
 {"id":"tu_me","input":{"source_tree":"main","file-path":"mix.txt","old-string":"hello","new-string":"world"},"name":"edit","type":"tool_use"},
 {"id":"tu_mg","input":{"pattern":"world"},"name":"grep","type":"tool_use"},
 {"id":"tu_mm","input":{"source_tree":"main","theirs":"feature"},"name":"merge","type":"tool_use"},
 {"id":"tu_side","input":{"source_tree":"side","file-path":"side.txt","content":"isolated"},"name":"write","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$MIXED_CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"mixed done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

new_llm_conversation tools-mixed "$STUB_PORT" "$ws"
feature=$(mint_commit /cas/feature "$feature_tree" feature "$base")
fetch_code "$feature" "fetching the feature commit"
git push -q caos "$feature:refs/caos/req/$feature" || fail "pushing feature closure"
llm=$(caos curry --base:hash="$llm" --merge-refs="feature $feature") \
  || fail "currying merge refs"
dispatch_turn "write, run bash, edit, grep, and merge"
wait_turn || fail "the mixed turn never reached a terminal head"

source_tree=$(source_tree_commit "$head")
fetch_code "$source_tree" "fetching the mixed-tool source_tree"
[ "$(git show "$source_tree:mix.txt")" = "world" ] \
  || fail "post-bash edit did not land"
[ "$(git show "$source_tree:mix3.txt")" = "HELLO" ] \
  || fail "bash did not see inline write"
[ "$(git show "$source_tree:feature.txt")" = "merged" ] \
  || fail "feature file did not land"
git merge-base --is-ancestor "$feature" "$source_tree" \
  || fail "feature is not an ancestor of the resulting source_tree"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/mixed-tools.jsonl
jq -s -e '
  (map(.id) | sort) == (["tu_mw","tu_mb","tu_me","tu_mg","tu_mm","tu_create","tu_side"] | sort) and
  (map(select(.id == "tu_mb"))[0] |
    .status == "complete" and .task != null and
    .input_commit != null and .source_tree_name == null and (.files_outcome.applied | sort) == ["main","review","side"]) and
  (map(select(.id == "tu_mg"))[0] |
    .status == "complete" and .result.proposal == null) and
  (map(select(.id == "tu_mm"))[0] |
    .status == "complete" and
    (.source_tree_resolution.kind == "merged" or .source_tree_resolution.kind == "direct"))
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

sequence=$(grep -o '"tool_use_id":"tu_m[wbegm]"' /tmp/stub/request-2.json \
  | grep -o 'tu_m[wbegm]' | paste -sd,)
[ "$sequence" = "tu_mw,tu_mb,tu_me,tu_mg,tu_mm" ] \
  || fail "results missing or misordered: $sequence"
grep -qF 'main/mix.txt:1:world' /tmp/stub/request-2.json \
  || fail "grep did not report the post-edit content to the model"
[ ! -f /tmp/stub/request-3.json ] || fail "unexpected extra model round"

side_source_tree=$(source_tree_commit "$head" side)
fetch_code "$side_source_tree" "fetching the new named source_tree"
[ "$(git show "$side_source_tree:side.txt")" = isolated ] || fail "explicit side edit missing"
if git cat-file -e "$source_tree:side.txt" 2>/dev/null; then fail "side edit leaked into main"; fi
[ "$(git show "$side_source_tree:mix.txt")" = hello ] || fail "input snapshot changed with main"
grep -qF 'side' /tmp/stub/request-2.json || fail "new source_tree missing from model context"

[ "$(git show "$head:memories/test.txt")" = remembered ] || fail "conversation memory was not staged"
[ "$(git show "$side_source_tree:second.txt")" = second ] || fail "second source tree edit lost"
review=$(source_tree_commit "$head" review)
fetch_code "$review" "fetching copied source tree"
[ "$(git show "$review:second.txt")" = second ] || fail "moved source tree lost its contents"
git merge-base --is-ancestor "$base" "$review" || fail "copy/move lost code ancestry"

pass chat-tools-mixed
