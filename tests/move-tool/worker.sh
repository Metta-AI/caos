#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "source tree and scripted move turn"
llm_test_setup

mkdir -p /tmp/ws/dir/sub
echo "alpha" > /tmp/ws/a.txt
echo "ex" > /tmp/ws/dir/x.txt
echo "why" > /tmp/ws/dir/sub/y.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the source tree")

# One batch, run in order. Inside `main`: a rename, a directory move that must
# create its parents, and the refusals (existing destination, onto itself, into
# itself, missing source, `.caos` at either end). Across the source tree
# boundary: `copy` makes a second source tree (setup, not the subject), `move`
# renames the whole source tree, then moves a file from `main` into it.
CALLS='[
 {"id":"tu_m1","input":{"from":"main/a.txt","to":"main/b.txt"},"name":"move","type":"tool_use"},
 {"id":"tu_m2","input":{"from":"main/dir","to":"main/moved/dir"},"name":"move","type":"tool_use"},
 {"id":"tu_m3","input":{"from":"main/b.txt","to":"main/moved/dir/x.txt"},"name":"move","type":"tool_use"},
 {"id":"tu_m4","input":{"from":"main/moved","to":"main/moved/inner"},"name":"move","type":"tool_use"},
 {"id":"tu_m5","input":{"from":"main/b.txt","to":"main/b.txt"},"name":"move","type":"tool_use"},
 {"id":"tu_m6","input":{"from":"main/nothing.txt","to":"main/z.txt"},"name":"move","type":"tool_use"},
 {"id":"tu_m7","input":{"from":"main/b.txt","to":".caos/b.txt"},"name":"move","type":"tool_use"},
 {"id":"tu_m8","input":{"from":".caos/a.txt","to":"main/c.txt"},"name":"move","type":"tool_use"},
 {"id":"tu_s0","input":{"from":"main","to":"review"},"name":"copy","type":"tool_use"},
 {"id":"tu_s1","input":{"from":"review","to":"review2"},"name":"move","type":"tool_use"},
 {"id":"tu_s2","input":{"from":"main/moved/dir/sub/y.txt","to":"review2/y.txt"},"name":"move","type":"tool_use"},
 {"id":"tu_ls","input":{"path":"."},"name":"ls","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"move done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

new_llm_conversation tools-move "$STUB_PORT" "$ws"
dispatch_turn "exercise move"
wait_turn || fail "the turn never reached a terminal head"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/tools.jsonl
jq -s -e '
  length == 12 and all(.[]; .status == "complete" and .task == null)
' /tmp/tools.jsonl >/dev/null || { cat /tmp/tools.jsonl >&2; fail "tool records are wrong"; }
[ ! -f /tmp/stub/request-3.json ] || fail "move cost extra model rounds"

# Ids are the call ids without their tu_ prefix. expect_error takes the text the
# error must contain ("" = only that it is an error); expect_ok the result text.
observe() {
  $TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
    --round 0 --id "tu_$1" > "/tmp/obs-$1.json"
}
expect_error() {
  observe "$1"
  grep -qF '"is_error":true' "/tmp/obs-$1.json" || fail "$1 is not marked is_error"
  [ -z "$2" ] || grep -qF "$2" "/tmp/obs-$1.json" || fail "$1 lacks its error: $2"
}
expect_ok() {
  observe "$1"
  if grep -qF '"is_error":true' "/tmp/obs-$1.json"; then
    cat "/tmp/obs-$1.json" >&2
    fail "$1 failed"
  fi
  grep -qF "$2" "/tmp/obs-$1.json" || fail "$1 lacks its result: $2"
}

stage "results the model sees"
expect_ok m1 'moved main/a.txt to main/b.txt'
expect_ok m2 'moved main/dir to main/moved/dir'
expect_error m3 'already exists'
expect_error m4 'into itself'
expect_error m5 'same path'
expect_error m6 ''
expect_error m7 '.caos is protocol metadata'
expect_error m8 '.caos is protocol metadata'
expect_ok s0 'copied main to review'
expect_ok s1 'moved review to review2'
expect_ok s2 'moved main/moved/dir/sub/y.txt to review2/y.txt'
observe ls
grep -qF 'review2' /tmp/obs-ls.json || fail "the renamed source tree is not listed"
# `review2/` contains `review`, so look for the old name as a whole entry.
if grep -qE '(^|[^0-9A-Za-z_-])review(/|\\n|")' /tmp/obs-ls.json; then
  fail "the moved source tree is still listed under its old name"
fi

stage "main after the batch"
source_tree=$(source_tree_commit "$head")
fetch_code "$source_tree" "fetching the resulting source_tree"
[ "$(git show "$source_tree:b.txt")" = alpha ] || fail "rename lost the content"
[ "$(git show "$source_tree:moved/dir/x.txt")" = ex ] \
  || fail "directory move did not land (or the refused move overwrote it)"
for gone in a.txt dir z.txt c.txt moved/inner moved/dir/sub/y.txt; do
  if git cat-file -e "$source_tree:$gone" 2>/dev/null; then fail "$gone should not exist in main"; fi
done

stage "the source tree boundary"
review2=$(source_tree_commit "$head" review2)
fetch_code "$review2" "fetching the moved source tree"
git merge-base --is-ancestor "$base" "$review2" || fail "move lost the source tree's history"
[ "$(git show "$review2:b.txt")" = alpha ] || fail "moved source tree lost a file"
[ "$(git show "$review2:y.txt")" = why ] || fail "move into a source tree did not land"
# The copy was taken before the move out of `main`, so it keeps its own y.txt.
[ "$(git show "$review2:moved/dir/sub/y.txt")" = why ] \
  || fail "moving a file out of main changed the earlier copy"

pass move-tool
