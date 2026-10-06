#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "source tree and scripted remove turn"
llm_test_setup

mkdir -p /tmp/ws/dir/sub /tmp/ws/solo
echo "alpha" > /tmp/ws/a.txt
echo "ex" > /tmp/ws/dir/x.txt
echo "why" > /tmp/ws/dir/sub/y.txt
echo "only" > /tmp/ws/solo/only.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the source tree")

# One batch, run in order. Inside `main`: remove a whole directory, the only
# file of a directory, and the refusals (missing path, `.caos`). Across the
# source tree boundary: `copy` makes two more source trees (setup, not the
# subject); `remove` takes a file out of one and the whole of the other.
CALLS='[
 {"id":"tu_r1","input":{"file-path":"main/dir"},"name":"remove","type":"tool_use"},
 {"id":"tu_r2","input":{"file-path":"main/nothing.txt"},"name":"remove","type":"tool_use"},
 {"id":"tu_r3","input":{"file-path":"main/solo/only.txt"},"name":"remove","type":"tool_use"},
 {"id":"tu_r4","input":{"file-path":".caos/x"},"name":"remove","type":"tool_use"},
 {"id":"tu_r5","input":{"file-path":".caos"},"name":"remove","type":"tool_use"},
 {"id":"tu_s0","input":{"from":"main","to":"review"},"name":"copy","type":"tool_use"},
 {"id":"tu_s1","input":{"from":"main","to":"scratch"},"name":"copy","type":"tool_use"},
 {"id":"tu_s2","input":{"file-path":"review/a.txt"},"name":"remove","type":"tool_use"},
 {"id":"tu_s3","input":{"file-path":"scratch"},"name":"remove","type":"tool_use"},
 {"id":"tu_ls","input":{"path":"."},"name":"ls","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"remove done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

new_llm_conversation tools-remove "$STUB_PORT" "$ws"
dispatch_turn "exercise remove"
wait_turn || fail "the turn never reached a terminal head"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/tools.jsonl
jq -s -e '
  length == 10 and all(.[]; .status == "complete" and .task == null)
' /tmp/tools.jsonl >/dev/null || { cat /tmp/tools.jsonl >&2; fail "tool records are wrong"; }
[ ! -f /tmp/stub/request-3.json ] || fail "remove cost extra model rounds"

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
expect_ok r1 'removed main/dir'
expect_error r2 ''
expect_ok r3 'removed main/solo/only.txt'
expect_error r4 '.caos is protocol metadata'
expect_error r5 '.caos is protocol metadata'
expect_ok s0 'copied main to review'
expect_ok s1 'copied main to scratch'
expect_ok s2 'removed review/a.txt'
expect_ok s3 'removed scratch'
# `scratch` was removed whole, so it is not at the root any more.
observe ls
grep -qF 'review' /tmp/obs-ls.json || fail "the other source tree is not listed"
if grep -qF 'scratch' /tmp/obs-ls.json; then fail "the removed source tree is still listed"; fi

stage "main after the batch"
source_tree=$(source_tree_commit "$head")
fetch_code "$source_tree" "fetching the resulting source_tree"
[ "$(git show "$source_tree:a.txt")" = alpha ] || fail "remove took an unrelated file"
for gone in dir dir/x.txt dir/sub/y.txt nothing.txt; do
  if git cat-file -e "$source_tree:$gone" 2>/dev/null; then fail "$gone should not exist in main"; fi
done
# Removing the only file of a directory must leave no files under it (git has
# no empty directories, so whether `solo` survives is not the point).
[ -z "$(git ls-tree -r --name-only "$source_tree" -- solo)" ] \
  || fail "files remain under solo"

stage "the source tree boundary"
review=$(source_tree_commit "$head" review)
fetch_code "$review" "fetching the edited source tree"
git merge-base --is-ancestor "$base" "$review" || fail "remove lost the source tree's history"
if git cat-file -e "$review:a.txt" 2>/dev/null; then fail "remove left review/a.txt"; fi
# The copy was taken after main lost `dir`, so it has none either; what remains
# of `main` at that point must still be there.
[ -z "$(git ls-tree -r --name-only "$review" -- solo)" ] || fail "files remain under review/solo"
[ "$(git show "$source_tree:a.txt")" = alpha ] || fail "remove in review reached main"

pass remove-tool
