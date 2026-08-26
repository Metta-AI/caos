#!/bin/bash
# tests/run-and-update-ref — a WORKER test, in dev/worker-test (it needs git).
#
# std/run-and-update-ref runs a subrequest R and appends ONE status event to a
# conversation ref: `complete` with R's exact result on success, `failed` with a
# structured error on failure — tree-neutral either way, and idempotent, so
# replaying a settled Q appends nothing.
#
# WHY GIT, AND ONLY GIT. Refs on the server are the one thing caos gives a
# worker no verb for, and this worker's whole output is a ref. Everything the
# client used to do here — `curry`, `prepare-request`, `run` — a worker does
# itself, which is why nothing below drives a client.
#
# ONE STAGE, BY POLLING THE REF. The obvious shape is a stage per run, but the
# thing being observed is the ref, not the result — so the test dispatches Q
# with `sub-run` and waits for the ref to move. That also makes the result
# assertions honest: they read what the EVENT recorded, which is the artifact
# under test, rather than what a separate `run` handed back.
#
# R'S RESULT IS COMPUTED, NOT OBSERVED. succeed.sh returns a tree that is a pure
# function of its own ArgTree (`payload from R` plus that hash), and this test
# forms that ArgTree with `prepare-request` — so the exact tree Q must pass
# through can be built here and hashed, without running R separately. Comparing
# against a computed expectation is stronger than comparing two runs.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

caos get /cas/args/test-salt || fail "reading --test-salt"
SALT=$(cat /cas/args/test-salt)
TS=1700000000

caos get /cas/args/succeed || fail "reading succeed.sh"
caos get /cas/args/fail    || fail "reading fail.sh"
caos get -r /cas/args/bash    || fail "reading the bash image"
caos get -r /cas/args/updater || fail "reading the run-and-update-ref image"
BASH=$(caos hash /cas/args/bash)
UPDATER=$(caos hash /cas/args/updater)

: "${CAOS_SERVER_URL:?this test needs CAOS_SERVER_URL from the runner}"
rm -rf /tmp/repo && mkdir -p /tmp/repo && cd /tmp/repo
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$CAOS_SERVER_URL"

remote_head() { # <ref>
  local line
  line=$(git ls-remote --refs caos "$1") || return 1
  [ -n "$line" ] || return 1
  printf '%s\n' "${line%%[[:space:]]*}"
}
assert_oid() { # <value> <what>
  [ "${#1}" -eq 40 ] || fail "$2 is not an oid: $1"
  case "$1" in *[!0-9a-f]*) fail "$2 is not an oid: $1" ;; esac
}
# Wait for <ref> to leave <was>, or time out. The event is what we are watching
# for; nothing else observes Q's completion.
wait_moved() { # <ref> <was> -> the new head
  local ref=$1 was=$2 now
  for _ in $(seq 1 300); do
    now=$(remote_head "$ref" 2>/dev/null || true)
    if [ -n "$now" ] && [ "$now" != "$was" ]; then printf '%s\n' "$now"; return 0; fi
    sleep 0.1
  done
  return 1
}
# The event a settled Q appends: tree-neutral, and recording task/status/result.
only_status_event() { # <before> <after> <task> <status> <result>
  local before=$1 after=$2 task=$3 status=$4 result=$5
  git -c fetch.negotiationAlgorithm=noop fetch -q caos "$after" \
    || fail "fetching the appended event $after"
  [ "$(git rev-parse "$after^")" = "$before" ] \
    || fail "the $status event is not based on the conversation head"
  [ "$(git rev-parse "$before^{tree}")" = "$(git rev-parse "$after^{tree}")" ] \
    || fail "the $status event changed the workspace"
  [ "$(git show "$after:workspace.txt")" = "workspace survives" ] \
    || fail "the status append lost the workspace"
  local got
  got=$(git show -s --format=%B "$after" | jq -r --arg task "$task" \
      'select((has("v") | not) and (has("base") | not) and .async.task == $task)
       | [.async.status, .async.result] | @tsv')
  [ "$got" = "$status"$'\t'"$result" ] || fail "the event recorded
  $got
instead of
  $status	$result
(full message: $(git show -s --format=%B "$after"))"
}

echo "== build exact successful and failing subrequests R ==" >&2
# R's IDENTITY, formed here rather than recovered from a run: `prepare-request`
# produces the very ArgTree the run would have, which is what succeed.sh hashes
# and returns, and what fail.sh prints before dying.
success_request=$(caos prepare-request --base:hash="$BASH" \
  --worker1:@=/cas/args/succeed --case=success) || fail "preparing the successful R"
assert_oid "$success_request" "successful R"
failure_request=$(caos prepare-request --base:hash="$BASH" \
  --worker1:@=/cas/args/fail --case=failure) || fail "preparing the failing R"
assert_oid "$failure_request" "failing R"

# R's exact RESULT, likewise computed: succeed.sh writes `payload from R` and
# its own ArgTree hash, so the tree is a pure function of the identity above.
rm -rf /tmp/expected && mkdir -p /tmp/expected
printf 'payload from R\n' > /tmp/expected/payload
printf '%s\n' "$success_request" > /tmp/expected/request
caos put /tmp/expected /cas/expected >/dev/null || fail "building R's expected result"
# The event records a BARE oid, not the client `run`'s "tree <oid>" rendering.
success_result=$(caos hash /cas/expected)
echo "  ok: R=$success_request, expecting tree $success_result" >&2

echo "== initialize two conversation heads ==" >&2
mkdir -p /tmp/ws && printf 'workspace survives\n' > /tmp/ws/workspace.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the conversation workspace"
ws_tree=$(caos hash /cas/ws)
mint() { # <dst> <message> [parent] -> the commit hash
  local dst=$1 msg=$2 parent=${3:-}
  { printf 'tree %s\n' "$ws_tree"
    if [ -n "$parent" ]; then printf 'parent %s\n' "$parent"; fi
    printf 'author caos <test@caos> %s +0000\n' "$TS"
    printf 'committer caos <test@caos> %s +0000\n' "$TS"
    printf '\n%s\n' "$msg"
  } > /tmp/commit
  caos put-commit /tmp/commit "$dst" || fail "minting $dst"
}
base=$(mint /cas/base "conversation base ($SALT)")
initial_head=$(mint /cas/head "{\"base\":\"$base\",\"status\":\"idle\"}" "$base")

test_run_id="$(date +%s%N)-$$-$RANDOM"
success_ref="refs/caos/v2/conversations/${test_run_id}-run-update-success/head"
failure_ref="refs/caos/v2/conversations/${test_run_id}-run-update-failure/head"
# The commits are already objects on the server (put-commit stored them), so
# the refs are set by pushing hashes the remote already has.
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$initial_head" \
  || fail "fetching the initial head back"
git push -q caos "$initial_head:$success_ref" "$initial_head:$failure_ref" \
  || fail "publishing the two conversation heads"
echo "  ok: both refs at $initial_head" >&2

echo "== exact Q appends complete and preserves R's exact result ==" >&2
# A WORKER forming Q from the CURRIED wrapper image, which is the claim
# `prepare.sh` used to carry: this script is a worker, so it makes the call.
success_task=$(caos prepare-request --base:hash="$UPDATER" \
  --subreq="$success_request" --target-ref="$success_ref") \
  || fail "preparing the successful Q"
assert_oid "$success_task" "successful Q"
caos sub-run "$success_task" >/dev/null || fail "dispatching the successful Q"
success_head=$(wait_moved "$success_ref" "$initial_head") \
  || fail "the successful Q never appended an event"
only_status_event "$initial_head" "$success_head" "$success_task" complete "$success_result"

# ...and the result the event names really is R's tree, byte for byte.
caos get-hash "$success_result" /cas/actual \
  || fail "the recorded result is not fetchable"
caos get -r /cas/actual || fail "materializing the recorded result"
diff -r /tmp/expected /cas/actual >/dev/null || fail "Q changed R's result contents"
echo "  ok: one tree-neutral complete event; exact result passed through" >&2

echo "== failure appends failed and returns a structured failure ==" >&2
failure_task=$(caos prepare-request --base:hash="$UPDATER" \
  --subreq="$failure_request" --target-ref="$failure_ref") \
  || fail "preparing the failing Q"
assert_oid "$failure_task" "failing Q"
caos sub-run "$failure_task" >/dev/null || fail "dispatching the failing Q"
failure_head=$(wait_moved "$failure_ref" "$initial_head") \
  || fail "the failing Q never appended an event"
# The event names the failure result; read it and check it carried R's exit.
# Fetched FIRST: the ref moved on the server, but nothing is in this repo until
# it is asked for, and `git show` on an unfetched head is "fatal: bad object".
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$failure_head" \
  || fail "fetching the failed event $failure_head"
failure_identity=$(git show -s --format=%B "$failure_head" \
  | jq -r --arg task "$failure_task" \
    'select((has("v") | not) and (has("base") | not) and .async.task == $task)
     | .async.result')
assert_oid "$failure_identity" "the failure result the event names"
only_status_event "$initial_head" "$failure_head" "$failure_task" failed "$failure_identity"
# A result TREE, not a blob — `get -r` walking it is what settles that, and the
# two files below are the structured failure the claim is about.
caos get-hash "$failure_identity" /cas/failed \
  || fail "the recorded failure result is not fetchable"
caos get -r /cas/failed || fail "materializing the failure result"
[ -d /cas/failed ] || fail "failed Q did not return a result tree"
[ "$(cat /cas/failed/status)" = failed ] || fail "failed result has no failed status"
grep -q "exit status: 23" /cas/failed/error \
  || fail "structured failure lost R's exit status: $(cat /cas/failed/error)"
echo "  ok: one failed event; structured error is addressable through Q" >&2

echo "== replaying a settled Q appends nothing ==" >&2
# Dispatched BEFORE the failure flow's work would have finished if it were going
# to move anything, and checked at the end: a settled Q is a cache hit, so there
# is no completion to wait for — the evidence is the ref standing still while
# the rest of this test drives the server.
caos sub-run "$success_task" >/dev/null || fail "replaying the successful Q"
caos sub-run "$failure_task" >/dev/null || fail "replaying the failing Q"
for _ in $(seq 1 30); do
  [ "$(remote_head "$success_ref")" = "$success_head" ] \
    || fail "replaying the completed Q appended another event"
  [ "$(remote_head "$failure_ref")" = "$failure_head" ] \
    || fail "replaying the failed Q appended another event"
  sleep 0.1
done
echo "  ok: neither ref moved" >&2

printf 'run-and-update-ref: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
