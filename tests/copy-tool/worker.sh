#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "source tree and scripted copy turn"
llm_test_setup

mkdir -p /tmp/ws/dir/sub
echo "alpha" > /tmp/ws/a.txt
echo "ex" > /tmp/ws/dir/x.txt
echo "why" > /tmp/ws/dir/sub/y.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the source tree")

# One batch, run in order. Inside `main`: a copy that must create its parents, a
# directory copy, and the refusals (existing destination, missing source, `.caos`).
# Across the source tree boundary: copy the whole source tree (`main` -> `review`),
# then copy a file INTO it.
CALLS='[
 {"id":"tu_c1","input":{"from":"main/a.txt","to":"main/copies/deep/a.txt"},"name":"copy","type":"tool_use"},
 {"id":"tu_c2","input":{"from":"main/a.txt","to":"main/dir/x.txt"},"name":"copy","type":"tool_use"},
 {"id":"tu_c3","input":{"from":"main/nothing.txt","to":"main/z.txt"},"name":"copy","type":"tool_use"},
 {"id":"tu_c4","input":{"from":"main/dir","to":"main/dir2"},"name":"copy","type":"tool_use"},
 {"id":"tu_c5","input":{"from":"main/a.txt","to":".caos/a.txt"},"name":"copy","type":"tool_use"},
 {"id":"tu_s1","input":{"from":"main","to":"review"},"name":"copy","type":"tool_use"},
 {"id":"tu_s2","input":{"from":"main/dir/x.txt","to":"review/extra/x.txt"},"name":"copy","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"copy done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

new_llm_conversation tools-copy "$STUB_PORT" "$ws"
dispatch_turn "exercise copy"
wait_turn || fail "the turn never reached a terminal head"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/tools.jsonl
jq -s -e '
  length == 7 and all(.[]; .status == "complete" and .task == null)
' /tmp/tools.jsonl >/dev/null || { cat /tmp/tools.jsonl >&2; fail "tool records are wrong"; }
[ ! -f /tmp/stub/request-3.json ] || fail "copy cost extra model rounds"

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
expect_ok c1 'copied main/a.txt to main/copies/deep/a.txt'
expect_error c2 'already exists'
expect_error c3 ''
expect_ok c4 'copied main/dir to main/dir2'
expect_error c5 '.caos is protocol metadata'
expect_ok s1 'copied main to review'
expect_ok s2 'copied main/dir/x.txt to review/extra/x.txt'

stage "main after the batch"
source_tree=$(source_tree_commit "$head")
fetch_code "$source_tree" "fetching the resulting source_tree"
[ "$(git show "$source_tree:a.txt")" = alpha ] || fail "the source of a copy was lost"
[ "$(git show "$source_tree:copies/deep/a.txt")" = alpha ] \
  || fail "copy did not create the missing parents"
[ "$(git show "$source_tree:dir/x.txt")" = ex ] || fail "refused copy changed the destination"
[ "$(git show "$source_tree:dir/sub/y.txt")" = why ] || fail "dir copy changed its source"
[ "$(git show "$source_tree:dir2/x.txt")" = ex ] || fail "dir copy lost a file"
[ "$(git show "$source_tree:dir2/sub/y.txt")" = why ] || fail "dir copy lost a nested file"
for gone in z.txt nothing.txt extra; do
  if git cat-file -e "$source_tree:$gone" 2>/dev/null; then fail "$gone should not exist in main"; fi
done

stage "the source tree boundary"
review=$(source_tree_commit "$head" review)
fetch_code "$review" "fetching the copied source tree"
git merge-base --is-ancestor "$base" "$review" || fail "copy lost the source tree's history"
[ "$(git show "$review:a.txt")" = alpha ] || fail "copied source tree lost a file"
[ "$(git show "$review:dir2/sub/y.txt")" = why ] || fail "copy ran before main's earlier copies"
[ "$(git show "$review:extra/x.txt")" = ex ] || fail "copy into a source tree did not land"
if git cat-file -e "$source_tree:extra" 2>/dev/null; then fail "copy into review leaked into main"; fi

pass copy-tool
