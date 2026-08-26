#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, inside a test stack (dev/cli-test stages the repo, then runs this).
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
remote_head() {
  local ref=$1 line
  line=$(git ls-remote caos "$ref")
  [ -n "$line" ] || fail "remote ref $ref is absent"
  printf '%s\n' "${line%%[[:space:]]*}"
}
marker() {
  local prefix=$1 file=$2 found=
  while IFS= read -r line; do
    case "$line" in
      "$prefix"*) found=${line#"$prefix"} ;;
    esac
  done < "$file"
  [ "${#found}" -eq 40 ] && [[ "$found" =~ ^[0-9a-f]+$ ]] \
    || fail "could not find $prefix<request> in $file"
  printf '%s\n' "$found"
}
only_status_event() {
  local before=$1 after=$2 task=$3 status=$4 result=$5
  [ "$(git rev-parse "$before^{tree}")" = "$(git rev-parse "$after^{tree}")" ] \
    || fail "status event changed the workspace"
  [ "$(git show -s --format=%B "$after" | jq -r --arg task "$task" \
      'select((has("v") | not) and (has("base") | not) and .async.task == $task) | [.async.status, .async.result] | @tsv')" \
      = "$status"$'\t'"$result" ] \
    || fail "status event did not record $task as $status with result $result"
}

echo "== build exact successful and failing subrequests R ==" >&2
success_image=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/succeed.sh)
success_identity=$("$CAOS_CLI" run expected --base:hash="$success_image" --case=success)
success_request=$(cat expected/request)
[ "${#success_request}" -eq 40 ] && [[ "$success_request" =~ ^[0-9a-f]+$ ]] \
  || fail "successful R returned malformed request identity: $success_request"

failure_image=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/fail.sh)
if "$CAOS_CLI" run --base:hash="$failure_image" --case=failure 2>failure.err; then
  fail "failing R unexpectedly succeeded"
fi
failure_request=$(marker EXACT_FAILED_REQUEST= failure.err)
echo "  ok: exact R identities recovered" >&2

echo "== initialize two conversation heads ==" >&2
printf 'workspace survives\n' > workspace.txt
commit "conversation base"
base=$(git rev-parse HEAD)
initial_head=$(printf '%s\n' "{\"base\":\"$base\",\"status\":\"idle\"}" \
  | git -c user.email=test@caos -c user.name=caos \
    commit-tree "$base^{tree}" -p "$base")
test_run_id="$(date +%s%N)-$$-$RANDOM"
success_ref="refs/caos/v2/conversations/${test_run_id}-run-update-success/head"
failure_ref="refs/caos/v2/conversations/${test_run_id}-run-update-failure/head"
git push -q caos "$initial_head:$success_ref" "$initial_head:$failure_ref"

worker=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/run-and-update-ref)
prepare_worker=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/prepare.sh)

echo "== worker prepares flat Q from curried wrapper image ==" >&2
success_task=$("$CAOS_CLI" run --base:hash="$prepare_worker" \
  --worker="$worker" --subreq="$success_request" --target-ref="$success_ref")
[ "${#success_task}" -eq 40 ] && [[ "$success_task" =~ ^[0-9a-f]+$ ]] \
  || fail "prepare-request returned malformed Q: $success_task"

echo "== exact Q appends complete and preserves R's exact result ==" >&2
actual_identity=$("$CAOS_CLI" run actual --base:hash="$success_task")
[ "$actual_identity" = "$success_identity" ] \
  || fail "Q changed R's result identity: $actual_identity != $success_identity"
diff -r expected actual >/dev/null || fail "Q changed R's result contents"

success_head=$(remote_head "$success_ref")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$success_head"
[ "$(git rev-parse "$success_head^")" = "$initial_head" ] \
  || fail "complete event is not based on the conversation head"
[ "$(git show "$success_head:workspace.txt")" = "workspace survives" ] \
  || fail "status append lost the workspace"
success_result=${actual_identity#* }
only_status_event "$initial_head" "$success_head" "$success_task" complete "$success_result"

again_identity=$("$CAOS_CLI" run actual-again --base:hash="$success_task")
[ "$again_identity" = "$success_identity" ] || fail "cached Q result changed"
[ "$(remote_head "$success_ref")" = "$success_head" ] \
  || fail "replaying completed Q appended another event"
echo "  ok: one tree-neutral complete event; exact result passed through" >&2

echo "== failure appends failed and returns structured failure ==" >&2
failure_task=$("$CAOS_CLI" run --base:hash="$prepare_worker" \
  --worker="$worker" --subreq="$failure_request" --target-ref="$failure_ref")
[ "${#failure_task}" -eq 40 ] && [[ "$failure_task" =~ ^[0-9a-f]+$ ]] \
  || fail "prepare-request returned malformed failure Q: $failure_task"
failed_identity=$("$CAOS_CLI" run failed --base:hash="$failure_task")
case "$failed_identity" in tree\ *) ;; *) fail "failed Q did not return a result tree" ;; esac
[ "$(cat failed/status)" = failed ] || fail "failed result has no failed status"
grep -q "exit status: 23" failed/error \
  || fail "structured failure lost R's exit status: $(cat failed/error)"

failure_head=$(remote_head "$failure_ref")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$failure_head"
[ "$(git rev-parse "$failure_head^")" = "$initial_head" ] \
  || fail "failed event is not based on the conversation head"
failure_result=${failed_identity#* }
only_status_event "$initial_head" "$failure_head" "$failure_task" failed "$failure_result"

"$CAOS_CLI" run failed-again --base:hash="$failure_task" >/dev/null
[ "$(remote_head "$failure_ref")" = "$failure_head" ] \
  || fail "replaying failed Q appended another event"
echo "  ok: one failed event; structured error is addressable through Q" >&2

echo "run-and-update-ref: ALL PASS" >&2
