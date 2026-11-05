#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

# Declare one call the way a harness that drives the model itself does, and
# leave it pending. `$head` follows the declaration.
declare_call() {
  local call=$1 input=$2 output key value
  output=$($TOOL declare --repo /tmp/repo --head "$head" \
    --actor tester --id "$call" --tool run_tool --input "$input" \
    --ref "$session_ref") || fail "declaring $call"
  while read -r key value; do
    if [ "$key" = head ]; then head=$value; fi
  done <<<"$output"
  assert_oid "$head" "the declared session head"
}

# Run the step for one declared call, in a fresh arg tree, and wait for the
# call's record to reach the status it must end with: `complete` for a call
# that ran (a tool's own refusal is a complete call with an error result), or
# `failed` for one the harness refused before it ran.
run_call() {
  local call=$1 want=$2 arg_tree attempt status
  arg_tree=$(caos prepare-request --base:hash="$llm" --head:commit="$human" \
    --run="$request" --tools-only="$call") \
    || fail "preparing the tools-only run for $call"
  caos sub-run "$arg_tree" >/dev/null || fail "dispatching the tools-only step for $call"
  for ((attempt = 0; attempt < 600; attempt++)); do
    head=$($TOOL fetch --repo /tmp/repo --ref "$session_ref") \
      || fail "fetching the session after $call"
    head=${head#head }
    assert_oid "$head" "the session head after $call"
    status=$($TOOL tools --repo /tmp/repo --head "$head" \
      | jq -r --arg id "$call" 'select(.id == $id) | .status') || status=""
    case "$status" in
      "$want")
        $TOOL tool-observation --repo /tmp/repo --head "$head" --id "$call" \
          > "/tmp/$call.observation" || fail "$call recorded no observation"
        return 0
        ;;
      complete | failed | cancelled)
        $TOOL tool-observation --repo /tmp/repo --head "$head" --id "$call" >&2 || true
        fail "call $call ended $status, not $want"
        ;;
    esac
    sleep 0.2
  done
  fail "call $call never completed"
}

says() {
  grep -qF -- "$2" "/tmp/$1.observation" \
    || fail "$1 does not say \"$2\": $(cat "/tmp/$1.observation")"
}

main_commit() {
  workspace=$(source_tree_commit "$head" main)
  fetch_code "$workspace" "fetching the source tree after the call"
}

stage "a workspace with the file tools at paths inside it"
llm_test_setup

rm -rf /tmp/ws
mkdir -p /tmp/ws
echo "hello tools" > /tmp/ws/greeting.txt
printf '#!/bin/sh\necho hi\n' > /tmp/ws/run.sh
chmod +x /tmp/ws/run.sh
for tool in read ls write edit move remove; do
  mkdir -p "/tmp/ws/tools/$tool"
  printf 'curry --base:hash=%s\n' "$(caos hash "/cas/args/$tool")" \
    > "/tmp/ws/tools/$tool/.caos-expr"
done
ws=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")

rm -rf /tmp/stub
mkdir -p /tmp/stub
start_stub /tmp/stub

new_llm_session root-tools "$STUB_PORT" "$ws" "You are a coding agent."
admit_turn "change some files"
base_commit=$base

stage "write: a new file in the source tree is a child commit of it"
declare_call w1 '{"path":"main/tools/write","arguments":{"root":"main/docs/new.txt","content":"fresh\n"}}'
run_call w1 complete
says w1 "wrote 6 bytes"
main_commit
[ "$(git show "$workspace:docs/new.txt")" = fresh ] || fail "the written file is not in the source tree"
[ "$(git rev-parse "$workspace^")" = "$base_commit" ] \
  || fail "the write is not a child commit of the source tree it changed"
[ "$(git log -1 --format=%s "$workspace")" = "main/tools/write main/docs/new.txt" ] \
  || fail "the commit message is $(git log -1 --format=%s "$workspace")"
[ "$(git show "$workspace:greeting.txt")" = "hello tools" ] || fail "an untouched file was lost"

stage "edit: an executable file keeps its mode"
declare_call e1 '{"path":"main/tools/edit","arguments":{"root":"main/run.sh","old-string":"hi","new-string":"bye"}}'
run_call e1 complete
says e1 "edited (1 replacement)"
main_commit
git show "$workspace:run.sh" | grep -qF "echo bye" || fail "the edit did not apply"
read -r mode _ <<<"$(git ls-tree "$workspace" run.sh)"
[ "$mode" = 100755 ] || fail "run.sh lost its executable bit: $mode"

stage "read and ls"
declare_call r1 '{"path":"main/tools/read","arguments":{"root":"main/greeting.txt"}}'
run_call r1 complete
says r1 "hello tools"
declare_call l1 '{"path":"main/tools/ls","arguments":{"root":"main"}}'
run_call l1 complete
says l1 "docs/"
says l1 "greeting.txt"
declare_call l2 "{\"path\":\"main/tools/ls\",\"arguments\":{\"root\":\"$ws/tools\"}}"
run_call l2 complete
says l2 "write/"

stage "move within the source tree, then remove a directory"
declare_call m1 '{"path":"main/tools/move","arguments":{"root":"main","from":"docs/new.txt","to":"notes/new.txt"}}'
run_call m1 complete
main_commit
[ "$(git show "$workspace:notes/new.txt")" = fresh ] || fail "the moved file is not at its new path"
if git cat-file -e "$workspace:docs" 2>/dev/null; then fail "docs/ survived the move of its only file"; fi
declare_call x1 '{"path":"main/tools/remove","arguments":{"root":"main/notes"}}'
run_call x1 complete
main_commit
if git cat-file -e "$workspace:notes" 2>/dev/null; then fail "notes/ was not removed"; fi

stage "a file at the top of the session, outside any source tree"
declare_call s1 '{"path":"main/tools/write","arguments":{"root":"plan.md","content":"the plan\n"}}'
run_call s1 complete
[ "$(record "$head" plan.md)" = "the plan" ] || fail "plan.md is not at the top of the session"

stage "refusals"
declare_call f1 '{"path":"main/tools/read","arguments":{"root":"main/greeting.txt/inside"}}'
run_call f1 failed
says f1 "which is a file"
declare_call f2 '{"path":"main/tools/write","arguments":{"root":".caos/x","content":"no"}}'
run_call f2 failed
says f2 "protocol metadata"
declare_call f3 "{\"path\":\"main/tools/write\",\"arguments\":{\"root\":\"$ws/x\",\"content\":\"no\"}}"
run_call f3 failed
says f3 "outside the session"
declare_call f4 '{"path":"main/tools/write","arguments":{"content":"no root"}}'
run_call f4 failed
says f4 "root"
declare_call f5 '{"path":"main/tools/read","arguments":{"root":"main/nothing.txt"}}'
run_call f5 complete
says f5 "nothing is at the root path"

assert_spine "$head"
[ ! -f /tmp/stub/request-1.json ] || fail "a tools-only run called the model"

pass root-tools
