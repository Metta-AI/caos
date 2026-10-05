#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "source tree and scripted copy/move/remove turn"
llm_test_setup

mkdir -p /tmp/ws/dir/sub /tmp/ws/solo
echo "alpha" > /tmp/ws/a.txt
echo "ex" > /tmp/ws/dir/x.txt
echo "why" > /tmp/ws/dir/sub/y.txt
echo "only" > /tmp/ws/solo/only.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the source tree")

# One batch, run in order. Within `main`: copy creating parents; the refusals
# (existing destination, missing source, same path, into itself, `.caos`); a
# directory copy, a move, and a recursive remove; removing the only file of a
# directory. Then across the source tree boundary: copy the whole source tree
# (`main` -> `review`), copy a file INTO it, remove a file from it, and remove
# the copied source tree. The final `ls` shows what is left at the root.
CALLS='[
 {"id":"tu_c1","input":{"from":"main/a.txt","to":"main/copies/deep/a.txt"},"name":"copy","type":"tool_use"},
 {"id":"tu_c2","input":{"from":"main/a.txt","to":"main/dir/x.txt"},"name":"copy","type":"tool_use"},
 {"id":"tu_c3","input":{"from":"main/nothing.txt","to":"main/z.txt"},"name":"copy","type":"tool_use"},
 {"id":"tu_c4","input":{"from":"main/dir","to":"main/dir2"},"name":"copy","type":"tool_use"},
 {"id":"tu_m1","input":{"from":"main/dir2","to":"main/dir2/inner"},"name":"move","type":"tool_use"},
 {"id":"tu_m2","input":{"from":"main/a.txt","to":"main/a.txt"},"name":"move","type":"tool_use"},
 {"id":"tu_m3","input":{"from":"main/dir2","to":"main/moved/dir"},"name":"move","type":"tool_use"},
 {"id":"tu_r1","input":{"file-path":"main/moved"},"name":"remove","type":"tool_use"},
 {"id":"tu_r2","input":{"file-path":"main/nothing.txt"},"name":"remove","type":"tool_use"},
 {"id":"tu_r3","input":{"file-path":"main/solo/only.txt"},"name":"remove","type":"tool_use"},
 {"id":"tu_p1","input":{"from":"main/a.txt","to":".caos/a.txt"},"name":"copy","type":"tool_use"},
 {"id":"tu_p2","input":{"from":"main/a.txt","to":".caos/a.txt"},"name":"move","type":"tool_use"},
 {"id":"tu_p3","input":{"file-path":".caos/a.txt"},"name":"remove","type":"tool_use"},
 {"id":"tu_s1","input":{"from":"main","to":"review"},"name":"copy","type":"tool_use"},
 {"id":"tu_s2","input":{"from":"main/dir/x.txt","to":"review/extra/x.txt"},"name":"copy","type":"tool_use"},
 {"id":"tu_s3","input":{"file-path":"review/dir/sub"},"name":"remove","type":"tool_use"},
 {"id":"tu_s4","input":{"from":"main","to":"scratch"},"name":"copy","type":"tool_use"},
 {"id":"tu_s5","input":{"file-path":"scratch"},"name":"remove","type":"tool_use"},
 {"id":"tu_ls","input":{"path":"."},"name":"ls","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"direct tools done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

new_llm_conversation tools-direct "$STUB_PORT" "$ws"
dispatch_turn "exercise copy, move and remove"
wait_turn || fail "the turn never reached a terminal head"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/tools.jsonl
jq -s -e '
  length == 19 and all(.[]; .status == "complete" and .task == null)
' /tmp/tools.jsonl >/dev/null || { cat /tmp/tools.jsonl >&2; fail "tool records are wrong"; }
[ ! -f /tmp/stub/request-3.json ] || fail "direct tools cost extra model rounds"

# observation ID -> required error text ("" = only that it is an error)
observe() {
  $TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
    --round 0 --id "$1" > "/tmp/obs-$1.json"
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
expect_error m1 'into itself'
expect_error m2 'are the same path'
expect_ok m3 'moved main/dir2 to main/moved/dir'
expect_ok r1 'removed main/moved'
expect_error r2 ''
expect_ok r3 'removed main/solo/only.txt'
expect_error p1 '.caos is protocol metadata'
expect_error p2 '.caos is protocol metadata'
expect_error p3 '.caos is protocol metadata'
expect_ok s1 'copied main to review'
expect_ok s2 'copied main/dir/x.txt to review/extra/x.txt'
expect_ok s3 'removed review/dir/sub'
expect_ok s4 'copied main to scratch'
expect_ok s5 'removed scratch'
# `scratch` was removed whole, so it is not at the root any more.
if grep -qF 'scratch' /tmp/obs-tu_ls.json 2>/dev/null; then fail "removed source tree still listed"; fi
observe tu_ls
grep -qF 'review' /tmp/obs-tu_ls.json || fail "copied source tree is not listed"
if grep -qF 'scratch' /tmp/obs-tu_ls.json; then fail "removed source tree still listed"; fi

stage "main after the batch"
source_tree=$(source_tree_commit "$head")
fetch_code "$source_tree" "fetching the resulting source_tree"
[ "$(git show "$source_tree:a.txt")" = alpha ] || fail "the source of a copy was lost"
[ "$(git show "$source_tree:copies/deep/a.txt")" = alpha ] \
  || fail "copy did not create the missing parents"
[ "$(git show "$source_tree:dir/x.txt")" = ex ] || fail "refused copy changed the destination"
[ "$(git show "$source_tree:dir/sub/y.txt")" = why ] || fail "dir copy changed its source"
for gone in dir2 moved nothing.txt z.txt solo/only.txt extra; do
  if git cat-file -e "$source_tree:$gone" 2>/dev/null; then fail "$gone should not exist in main"; fi
done
# Removing the only file of a directory must leave no files under it (git has
# no empty directories, so whether `solo` survives is not the point).
[ -z "$(git ls-tree -r --name-only "$source_tree" -- solo)" ] \
  || fail "files remain under solo"

stage "the source tree boundary"
review=$(source_tree_commit "$head" review)
fetch_code "$review" "fetching the copied source tree"
git merge-base --is-ancestor "$base" "$review" || fail "copy lost the source tree's history"
[ "$(git show "$review:a.txt")" = alpha ] || fail "copied source tree lost a file"
[ "$(git show "$review:extra/x.txt")" = ex ] || fail "copy into a source tree did not land"
[ "$(git show "$review:copies/deep/a.txt")" = alpha ] || fail "copy ran before main's earlier edits"
if git cat-file -e "$review:dir/sub" 2>/dev/null; then fail "remove left review/dir/sub"; fi
[ "$(git show "$review:dir/x.txt")" = ex ] || fail "remove took a sibling with it"
if git cat-file -e "$source_tree:extra" 2>/dev/null; then fail "copy into review leaked into main"; fi
[ "$(git show "$source_tree:dir/sub/y.txt")" = why ] || fail "remove in review reached main"

pass chat-tools-direct
