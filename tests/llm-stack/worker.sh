#!/bin/bash
# Exercise the agent tools across separate turns, not just the replay engine.
set -euo pipefail
caos get /cas/args/common
# shellcheck disable=SC1090
source /cas/args/common
llm_test_setup
mkdir -p /tmp/source /tmp/stub
printf 'base\n' > /tmp/source/file
ws=$(publish_tree /tmp/source /cas/ws "publishing stack fixture")

reply() {
  local number=$1 name=$2 input=$3
  jq -n --arg id "call-$number" --arg name "$name" --argjson input "$input"     '{content:[{type:"tool_use",id:$id,name:$name,input:$input}],stop_reason:"tool_use"}'     > "/tmp/stub/response-$number.json"
}
end_turn() {
  printf '%s\n' '{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}'     > "/tmp/stub/response-$1.json"
}
reply 1 bash '{"cmd":"mkdir -p feature; cp -a main feature/00-base; cp -a main feature/01-a","paths":["main"]}'
reply 2 write '{"file-path":"feature/01-a/file","content":"ours\n"}'
reply 3 bash '{"cmd":"cp -a feature/01-a feature/02-b; printf second > feature/02-b/extra","paths":["feature/01-a"]}'
reply 4 stack '{"action":"create","path":"feature","base":"00-base","layers":["01-a","02-b"]}'
reply 5 bash '{"cmd":"mkdir -p imports; cp -a feature/00-base imports/main; printf \"theirs\\n\" > imports/main/file","paths":["feature/00-base"]}'
reply 6 stack '{"action":"rebase","path":"feature","onto":"imports/main"}'
end_turn 7
reply 8 write '{"file-path":"feature/restack/work/file","content":"resolved\n"}'
reply 9 stack '{"action":"continue","path":"feature","resolved":true}'
end_turn 10
reply 11 write '{"file-path":"feature/01-a/review","content":"review correction\n"}'
reply 12 stack '{"action":"rebase","path":"feature","onto":"feature/00-base"}'
end_turn 13
start_stub /tmp/stub
new_llm_conversation llm-stack "$STUB_PORT" "$ws" "Execute the scripted stack workflow."
stage "create and pause a stack rebase"
dispatch_turn "Create and rebase the two-layer stack. ($SALT)"
wait_turn 600
record "$head" feature/restack/operation.json > /tmp/operation.json
jq -e '.operation.pending.result.conflicted == true and (.operation.pending.result.stages | length) == 3'   /tmp/operation.json >/dev/null || fail "missing structured conflict report"
original_a=$(jq -r '.operation.layers[0].head' /tmp/operation.json)
original_b=$(jq -r '.operation.layers[1].head' /tmp/operation.json)
[ "$(source_tree_commit "$head" feature/01-a)" = "$original_a" ] || fail "conflict moved first source"
[ "$(source_tree_commit "$head" feature/02-b)" = "$original_b" ] || fail "conflict moved second source"
draft=$(source_tree_commit "$head" feature/restack/work)
fetch_code "$draft" "reading conflict draft"
if git cat-file -e "$draft:.caos/conflicts" 2>/dev/null; then fail "draft contains source-tree conflict ledger"; fi
git show "$draft:file" > /tmp/draft-file
grep -q '<<<<<<<' /tmp/draft-file || fail "draft lacks proposed merge contents"

stage "resolve and resume in a new turn"
dispatch_turn "The draft has been reviewed; resolve it and continue. ($SALT)"
wait_turn 600
first=$(source_tree_commit "$head" feature/01-a)
second=$(source_tree_commit "$head" feature/02-b)
main=$(source_tree_commit "$head" imports/main)
fetch_code "$second" "reading completed stack"
[ "$(git rev-parse "$first^")" = "$main" ] || fail "first layer has wrong parent"
[ "$(git rev-parse "$second^")" = "$first" ] || fail "second layer has wrong parent"
[ "$(git show "$second:file")" = resolved ] || fail "resolution did not propagate"
[ "$(git show "$second:extra")" = second ] || fail "upper layer was lost"
if git merge-base --is-ancestor "$draft" "$second"; then fail "draft entered source history"; fi
if record "$head" feature/restack/operation.json >/dev/null 2>&1; then fail "completed operation still pending"; fi
if git rev-list --objects "$second" | grep -q ' .caos'; then fail "published history contains conflict bookkeeping"; fi

stage "propagate a review edit through the existing stack"
dispatch_turn "Revise the lower layer and restack. ($SALT)"
wait_turn 600
first=$(source_tree_commit "$head" feature/01-a)
second=$(source_tree_commit "$head" feature/02-b)
fetch_code "$second" "reading revised stack"
[ "$(git rev-parse "$second^")" = "$first" ] || fail "upper layer was not restacked onto the exact lower tip"
[ "$(git show "$second:review")" = "review correction" ] || fail "review edit did not propagate"
[ "$(git show "$second:extra")" = second ] || fail "upper layer disappeared"
jq -e '.tools | any(.name == "stack")' /tmp/stub/request-1.json >/dev/null || fail "stack tool is absent"
assert_spine "$head"
pass llm-stack
