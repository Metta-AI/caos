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
# FOUR STAGES, AND NOT ONE POLL. The first version of this ran Q with `sub-run`
# and then waited for the ref to move, which meant the test held a runner slot
# while the job it was waiting for needed one — the hold-and-wait that
# design/map-then.md's "this cannot deadlock" argument rules out, reintroduced
# by the test. `run-request-then` gives a `then` when Q finishes, so the ref can
# simply be READ in the next container, with nothing waiting anywhere.
#
# R'S RESULT IS COMPUTED, NOT OBSERVED. succeed.sh returns a tree that is a pure
# function of its own ArgTree (`payload from R` plus that hash), and this test
# forms that ArgTree with `prepare-request` — so the exact tree Q must pass
# through can be built here and hashed, without running R separately.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

caos get /cas/args/test-salt || fail "reading --test-salt"
SALT=$(cat /cas/args/test-salt)
TS=1700000000

caos get /cas/args/succeed || fail "reading succeed.sh"
caos get /cas/args/fail    || fail "reading fail.sh"
caos get -r /cas/args/bash    || fail "reading the bash image"
caos get -r /cas/args/updater || fail "reading the run-and-update-ref image"
BASH=$(caos hash /cas/args/bash)
UPDATER=$(caos hash /cas/args/updater)

# Everything a later stage reads has to be re-bound by name: `/cas/args/base` is
# the IMAGE, not this job's ArgTree, so a curry off it carries nothing else.
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --bash:@=/cas/args/bash --updater:@=/cas/args/updater \
  --succeed:@=/cas/args/succeed --fail:@=/cas/args/fail "$@"; }

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
# The event a settled Q appends: tree-neutral, and recording task/status/result.
only_status_event() { # <before> <after> <task> <status> <result>
  local before=$1 after=$2 task=$3 status=$4 result=$5 got
  git -c fetch.negotiationAlgorithm=noop fetch -q caos "$after" \
    || fail "fetching the appended event $after"
  [ "$(git rev-parse "$after^")" = "$before" ] \
    || fail "the $status event is not based on the conversation head"
  [ "$(git rev-parse "$before^{tree}")" = "$(git rev-parse "$after^{tree}")" ] \
    || fail "the $status event changed the workspace"
  [ "$(git show "$after:workspace.txt")" = "workspace survives" ] \
    || fail "the status append lost the workspace"
  got=$(git show -s --format=%B "$after" | jq -r --arg task "$task" \
      'select((has("v") | not) and (has("base") | not) and .async.task == $task)
       | [.async.status, .async.result] | @tsv')
  [ "$got" = "$status"$'\t'"$result" ] || fail "the event recorded
  $got
instead of
  $status	$result"
}

# R's IDENTITY, formed here rather than recovered from a run: `prepare-request`
# produces the very ArgTree the run would have, which is what succeed.sh hashes
# and returns, and what fail.sh prints before dying. Deterministic, so every
# stage recomputes the same values instead of forwarding them.
success_request=$(caos prepare-request --base:hash="$BASH" \
  --worker1:@=/cas/args/succeed --case=success) || fail "preparing the successful R"
failure_request=$(caos prepare-request --base:hash="$BASH" \
  --worker1:@=/cas/args/fail --case=failure) || fail "preparing the failing R"

# R's exact RESULT, likewise computed: succeed.sh writes `payload from R` and
# its own ArgTree hash, so the tree is a pure function of the identity above.
# The event records a BARE oid, not the client `run`'s "tree <oid>" rendering.
rm -rf /tmp/expected && mkdir -p /tmp/expected
printf 'payload from R\n' > /tmp/expected/payload
printf '%s\n' "$success_request" > /tmp/expected/request
caos put /tmp/expected /cas/expected >/dev/null || fail "building R's expected result"
success_result=$(caos hash /cas/expected)

case "$stage" in

start)
  echo "== build exact successful and failing subrequests R ==" >&2
  assert_oid "$success_request" "successful R"
  assert_oid "$failure_request" "failing R"
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
  caos run-request-then "$success_task" \
    --then:hash="$(next succeeded --initial="$initial_head" \
      --success-ref="$success_ref" --failure-ref="$failure_ref" \
      --success-task="$success_task")"
  ;;

succeeded)
  for a in initial success-ref failure-ref success-task; do
    caos get "/cas/args/$a" || fail "reading --$a"
  done
  initial_head=$(cat /cas/args/initial)
  success_ref=$(cat /cas/args/success-ref)
  failure_ref=$(cat /cas/args/failure-ref)
  success_task=$(cat /cas/args/success-task)

  # Q HAS FINISHED — that is what being the `then` of it means — so the ref is
  # simply read. No waiting, and no slot held while waiting.
  success_head=$(remote_head "$success_ref") || fail "the successful Q appended no event"
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
  caos run-request-then "$failure_task" \
    --then:hash="$(next failed --initial="$initial_head" \
      --success-ref="$success_ref" --failure-ref="$failure_ref" \
      --success-task="$success_task" --failure-task="$failure_task" \
      --success-head="$success_head")"
  ;;

failed)
  for a in initial success-ref failure-ref success-task failure-task success-head; do
    caos get "/cas/args/$a" || fail "reading --$a"
  done
  initial_head=$(cat /cas/args/initial)
  failure_ref=$(cat /cas/args/failure-ref)
  failure_task=$(cat /cas/args/failure-task)

  failure_head=$(remote_head "$failure_ref") || fail "the failing Q appended no event"
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
  # STRONGER THAN THE OLD HOLD-AND-WAIT VERSION, which dispatched the replays and
  # then watched the refs stand still for three seconds — evidence that the
  # replay had not YET appended anything. As a `then`, the replay has provably
  # finished before the next stage looks.
  caos run-request-then "$(cat /cas/args/success-task)" \
    --then:hash="$(next replayed --success-ref="$(cat /cas/args/success-ref)" \
      --failure-ref="$failure_ref" --failure-task="$failure_task" \
      --success-head="$(cat /cas/args/success-head)" --failure-head="$failure_head")"
  ;;

replayed)
  for a in success-ref failure-ref failure-task success-head failure-head; do
    caos get "/cas/args/$a" || fail "reading --$a"
  done
  [ "$(remote_head "$(cat /cas/args/success-ref)")" = "$(cat /cas/args/success-head)" ] \
    || fail "replaying the completed Q appended another event"
  # The failing Q's replay, checked the same way in the same container.
  caos run-request-then "$(cat /cas/args/failure-task)" \
    --then:hash="$(next replayed-failure \
      --failure-ref="$(cat /cas/args/failure-ref)" \
      --failure-head="$(cat /cas/args/failure-head)")"
  ;;

replayed-failure)
  caos get /cas/args/failure-ref; caos get /cas/args/failure-head
  [ "$(remote_head "$(cat /cas/args/failure-ref)")" = "$(cat /cas/args/failure-head)" ] \
    || fail "replaying the failed Q appended another event"
  echo "  ok: neither ref moved" >&2

  printf 'run-and-update-ref: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
