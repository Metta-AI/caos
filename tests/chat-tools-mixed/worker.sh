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

# THE SHELL IS A TOOL AT THE CONVERSATION ROOT, and that position is the whole
# point of this test: a path outside every source tree runs over the conversation
# tree, which is the one scope from which `cp -a main side`, `mv side review` and
# a write into `memories/` are all reachable at once. A tool inside `main` would
# see only `main`. The model stages it here with `write`, which is all a tool is
# -- a directory whose `.caos-expr` names an image (std/bash-tool's, by hash).
bash_tool_img=$(caos hash /cas/args/bash-tool)
merge_img=$(caos hash /cas/args/merge)
# The stub reads its scripted responses when the turn asks for them, so it can
# start empty: the merge call below names the feature commit by hash, and that
# commit is made on top of the conversation's own base, which needs the stub's port.
mkdir -p /tmp/stub
start_stub /tmp/stub

new_llm_conversation tools-mixed "$STUB_PORT" "$ws"
feature=$(mint_commit /cas/feature "$feature_tree" feature "$base")
fetch_code "$feature" "fetching the feature commit"
git push -q caos "$feature:refs/caos/req/$feature" || fail "pushing feature closure"

# THE MERGE IS A TOOL TOO, at the conversation root like the shell, and it picks
# the source tree it advances by ARGUMENT (`source-tree`), not by where it sits:
# `@scope` in its help. `theirs` is a commit hash, the one thing the tool cannot
# find for itself.
MIXED_CALLS='[
 {"id":"tu_tool","input":{"file-path":"tools/sh/.caos-expr","content":"curry --base:hash='"$bash_tool_img"'\n"},"name":"write","type":"tool_use"},
 {"id":"tu_mtool","input":{"file-path":"tools/merge/.caos-expr","content":"curry --base:hash='"$merge_img"'\n"},"name":"write","type":"tool_use"},
 {"id":"tu_mw","input":{"file-path":"main/mix.txt","content":"hello"},"name":"write","type":"tool_use"},
 {"id":"tu_create","input":{"arguments":{"cmd":"cp -a main side; mkdir -p memories; echo remembered > memories/test.txt","paths":["main"]},"path":"tools/sh"},"name":"run_tool","type":"tool_use"},
 {"id":"tu_mb","input":{"arguments":{"cmd":"tr a-z A-Z < side/mix.txt > main/mix3.txt; echo second > side/second.txt; mv side review; cp -a review side","paths":["main","side"]},"path":"tools/sh"},"name":"run_tool","type":"tool_use"},
 {"id":"tu_me","input":{"source_tree":"main","file-path":"mix.txt","old-string":"hello","new-string":"world"},"name":"edit","type":"tool_use"},
 {"id":"tu_mg","input":{"pattern":"world"},"name":"grep","type":"tool_use"},
 {"id":"tu_mm","input":{"arguments":{"source-tree":"main","theirs":"'"$feature"'"},"path":"tools/merge"},"name":"run_tool","type":"tool_use"},
 {"id":"tu_mp","input":{"arguments":{"source-tree":"main","theirs":"side"},"path":"tools/merge"},"name":"run_tool","type":"tool_use"},
 {"id":"tu_side","input":{"source_tree":"side","file-path":"side.txt","content":"isolated"},"name":"write","type":"tool_use"}]'
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$MIXED_CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"mixed done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
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
# `theirs` named by PATH (`side`, a source tree the shell made) rather than by hash:
# the tool takes the commit the code reference stores. Its `second.txt` arrives.
[ "$(git show "$source_tree:second.txt")" = "second" ] \
  || fail "merging the source tree at 'side' by path did not bring its file in"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/mixed-tools.jsonl
jq -s -e '
  (map(.id) | sort) == (["tu_tool","tu_mtool","tu_mw","tu_mb","tu_me","tu_mg","tu_mm","tu_mp","tu_create","tu_side"] | sort) and
  (map(select(.id == "tu_mb"))[0] |
    .status == "complete" and .task != null and
    .input_commit != null and .source_tree_name == null and (.files_outcome.applied | sort) == ["main","review","side"]) and
  (map(select(.id == "tu_mg"))[0] |
    .status == "complete" and .result.proposal == null) and
  (map(select(.id == "tu_mm"))[0] |
    .status == "complete" and
    (.source_tree_resolution.kind == "merged" or .source_tree_resolution.kind == "direct")) and
  (map(select(.id == "tu_mp"))[0] |
    .status == "complete" and .source_tree_name == "main" and
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
