#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, inside a test stack (tests/lib/run-test.sh).
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
remote_head() {
  local ref=$1 line
  line=$(git ls-remote caos "$ref")
  [ -n "$line" ] || fail "remote ref $ref is absent"
  printf '%s\n' "${line%%[[:space:]]*}"
}
remote_exact_ref() { # <ref>
  curl -fsS -X POST -H 'content-type: application/json' \
    --data "{\"ref\":\"$1\"}" "$CAOS_SERVER_URL/ref/read"
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
  local before=$1 after=$2 task=$3 status=$4
  [ "$(git rev-parse "$before^{tree}")" = "$(git rev-parse "$after^{tree}")" ] \
    || fail "status event changed the workspace"
  [ "$(git show -s --format=%B "$after" | jq -r --arg task "$task" \
      'select(.async.task == $task) | .async.status')" = "$status" ] \
    || fail "status event did not record $task as $status"
}

echo "== build exact successful and failing subrequests R ==" >&2
success_image=$("$CAOS_CLI" curry DEEP-DEPS/bash -- --worker1:@=test/succeed.sh)
success_identity=$("$CAOS_CLI" run "$success_image" expected -- --case=success)
success_request=$(cat expected/request)
[ "${#success_request}" -eq 40 ] && [[ "$success_request" =~ ^[0-9a-f]+$ ]] \
  || fail "successful R returned malformed request identity: $success_request"

failure_image=$("$CAOS_CLI" curry DEEP-DEPS/bash -- --worker1:@=test/fail.sh)
if "$CAOS_CLI" run "$failure_image" -- --case=failure 2>failure.err; then
  fail "failing R unexpectedly succeeded"
fi
failure_request=$(marker EXACT_FAILED_REQUEST= failure.err)
echo "  ok: exact R identities recovered" >&2

echo "== initialize two conversation heads ==" >&2
printf 'workspace survives\n' > workspace.txt
commit "conversation base"
base=$(git rev-parse HEAD)
suffix=${base:0:12}
success_ref="refs/caos/conversations/run-update-success-$suffix/head"
failure_ref="refs/caos/conversations/run-update-failure-$suffix/head"
git push -q caos "HEAD:$success_ref" "HEAD:$failure_ref"

worker=$("$CAOS_CLI" curry DEEP-DEPS/run-and-update-ref --)
prepare_worker=$("$CAOS_CLI" curry DEEP-DEPS/bash -- --worker1:@=test/prepare.sh)

echo "== worker prepares flat Q from curried wrapper image ==" >&2
success_task=$("$CAOS_CLI" run "$prepare_worker" -- \
  --worker="$worker" --subreq="$success_request" --target-ref="$success_ref")
[ "${#success_task}" -eq 40 ] && [[ "$success_task" =~ ^[0-9a-f]+$ ]] \
  || fail "prepare-request returned malformed Q: $success_task"

echo "== exact Q appends complete and preserves R's exact result ==" >&2
actual_identity=$("$CAOS_CLI" run "$success_task" actual --)
[ "$actual_identity" = "$success_identity" ] \
  || fail "Q changed R's result identity: $actual_identity != $success_identity"
diff -r expected actual >/dev/null || fail "Q changed R's result contents"

success_head=$(remote_head "$success_ref")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$success_head"
[ "$(git rev-parse "$success_head^")" = "$base" ] \
  || fail "complete event is not based on the conversation head"
[ "$(git show "$success_head:workspace.txt")" = "workspace survives" ] \
  || fail "status append lost the workspace"
only_status_event "$base" "$success_head" "$success_task" complete
[ -n "$(remote_exact_ref "refs/caos/res/$success_task")" ] \
  || fail "status event is not named by Q"

again_identity=$("$CAOS_CLI" run "$success_task" actual-again --)
[ "$again_identity" = "$success_identity" ] || fail "cached Q result changed"
[ "$(remote_head "$success_ref")" = "$success_head" ] \
  || fail "replaying completed Q appended another event"
echo "  ok: one tree-neutral complete event; exact result passed through" >&2

echo "== failure appends failed and returns structured failure ==" >&2
failure_task=$("$CAOS_CLI" run "$prepare_worker" -- \
  --worker="$worker" --subreq="$failure_request" --target-ref="$failure_ref")
[ "${#failure_task}" -eq 40 ] && [[ "$failure_task" =~ ^[0-9a-f]+$ ]] \
  || fail "prepare-request returned malformed failure Q: $failure_task"
failed_identity=$("$CAOS_CLI" run "$failure_task" failed --)
case "$failed_identity" in tree\ *) ;; *) fail "failed Q did not return a result tree" ;; esac
[ "$(cat failed/status)" = failed ] || fail "failed result has no failed status"
grep -q "exit status: 23" failed/error \
  || fail "structured failure lost R's exit status: $(cat failed/error)"

failure_head=$(remote_head "$failure_ref")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$failure_head"
[ "$(git rev-parse "$failure_head^")" = "$base" ] \
  || fail "failed event is not based on the conversation head"
only_status_event "$base" "$failure_head" "$failure_task" failed
[ -n "$(remote_exact_ref "refs/caos/res/$failure_task")" ] \
  || fail "structured failure is not available through Q"

"$CAOS_CLI" run "$failure_task" failed-again -- >/dev/null
[ "$(remote_head "$failure_ref")" = "$failure_head" ] \
  || fail "replaying failed Q appended another event"
echo "  ok: one failed event; structured error is addressable through Q" >&2

echo "run-and-update-ref: ALL PASS" >&2
