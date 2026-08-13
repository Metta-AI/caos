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
result_ids() {
  git ls-remote caos 'refs/caos/res/*' \
    | while IFS=$'\t' read -r _ ref; do printf '%s\n' "${ref##*/}"; done \
    | sort
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
only_status_changed() {
  local before=$1 after=$2 task=$3 changed
  changed=$(git diff-tree --no-commit-id --name-only -r "$before" "$after")
  [ "$changed" = ".caos/async/$task/status" ] \
    || fail "status append changed unexpected paths: $changed"
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
mkdir -p .caos/async/existing
printf 'workspace survives\n' > workspace.txt
printf 'pending\n' > .caos/async/existing/status
commit "conversation base"
base=$(git rev-parse HEAD)
suffix=$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')
success_ref="refs/caos/conversations/run-update-success-$suffix/head"
failure_ref="refs/caos/conversations/run-update-failure-$suffix/head"
git push -q caos "HEAD:$success_ref" "HEAD:$failure_ref"

worker=$("$CAOS_CLI" curry DEEP-DEPS/run-and-update-ref --)

echo "== success appends complete and preserves R's exact result ==" >&2
result_ids > before-success-results
actual_identity=$("$CAOS_CLI" run "$worker" actual -- \
  --subreq="$success_request" --target-ref="$success_ref")
[ "$actual_identity" = "$success_identity" ] \
  || fail "Q changed R's result identity: $actual_identity != $success_identity"
diff -r expected actual >/dev/null || fail "Q changed R's result contents"

success_head=$(remote_head "$success_ref")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$success_head"
[ "$(git rev-parse "$success_head^")" = "$base" ] \
  || fail "complete event is not based on the conversation head"
mapfile -t success_tasks < <(git ls-tree --name-only "$success_head:.caos/async" | grep -E '^[0-9a-f]{40}$')
[ "${#success_tasks[@]}" -eq 1 ] || fail "expected one completed task"
success_task=${success_tasks[0]}
result_ids > after-success-results
mapfile -t new_success_results < <(comm -13 before-success-results after-success-results)
[ "${#new_success_results[@]}" -eq 1 ] \
  || fail "success Q did not add exactly one result ref"
[ "${new_success_results[0]}" = "$success_task" ] \
  || fail "status task is not Q: ${success_task} != ${new_success_results[0]}"
[ "$(git show "$success_head:.caos/async/$success_task/status")" = complete ] \
  || fail "successful task was not marked complete"
[ "$(git show "$success_head:workspace.txt")" = "workspace survives" ] \
  || fail "status append lost the workspace"
[ "$(git show "$success_head:.caos/async/existing/status")" = pending ] \
  || fail "status append lost existing .caos state"
only_status_changed "$base" "$success_head" "$success_task"
git show -s --format=%B "$success_head" \
  | grep -qF "\"v\":2,\"async_task\":\"$success_task\",\"async_status\":\"complete\"" \
  || fail "complete event message is missing task metadata"
[ -n "$(git ls-remote caos "refs/caos/res/$success_task")" ] \
  || fail "status directory is not named by Q"

again_identity=$("$CAOS_CLI" run "$worker" actual-again -- \
  --subreq="$success_request" --target-ref="$success_ref")
[ "$again_identity" = "$success_identity" ] || fail "cached Q result changed"
[ "$(remote_head "$success_ref")" = "$success_head" ] \
  || fail "replaying completed Q appended another event"
echo "  ok: one tree-neutral complete event; exact result passed through" >&2

echo "== failure appends failed and returns structured failure ==" >&2
result_ids > before-failure-results
failed_identity=$("$CAOS_CLI" run "$worker" failed -- \
  --subreq="$failure_request" --target-ref="$failure_ref")
case "$failed_identity" in tree\ *) ;; *) fail "failed Q did not return a result tree" ;; esac
[ "$(cat failed/status)" = failed ] || fail "failed result has no failed status"
grep -q "exit status: 23" failed/error \
  || fail "structured failure lost R's exit status: $(cat failed/error)"

failure_head=$(remote_head "$failure_ref")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$failure_head"
[ "$(git rev-parse "$failure_head^")" = "$base" ] \
  || fail "failed event is not based on the conversation head"
mapfile -t failure_tasks < <(git ls-tree --name-only "$failure_head:.caos/async" | grep -E '^[0-9a-f]{40}$')
[ "${#failure_tasks[@]}" -eq 1 ] || fail "expected one failed task"
failure_task=${failure_tasks[0]}
result_ids > after-failure-results
mapfile -t new_failure_results < <(comm -13 before-failure-results after-failure-results)
[ "${#new_failure_results[@]}" -eq 1 ] \
  || fail "failure Q did not add exactly one result ref"
[ "${new_failure_results[0]}" = "$failure_task" ] \
  || fail "failed status task is not Q: ${failure_task} != ${new_failure_results[0]}"
[ "$(git show "$failure_head:.caos/async/$failure_task/status")" = failed ] \
  || fail "failed task was not marked failed"
only_status_changed "$base" "$failure_head" "$failure_task"
[ -n "$(git ls-remote caos "refs/caos/res/$failure_task")" ] \
  || fail "structured failure is not available through Q"

"$CAOS_CLI" run "$worker" failed-again -- \
  --subreq="$failure_request" --target-ref="$failure_ref" >/dev/null
[ "$(remote_head "$failure_ref")" = "$failure_head" ] \
  || fail "replaying failed Q appended another event"
echo "  ok: one failed event; structured error is addressable through Q" >&2

echo "== a late finish never replaces canceled ==" >&2
cancel_task=cccccccccccccccccccccccccccccccccccccccc
mkdir -p ".caos/async/$cancel_task"
printf 'canceled\n' > ".caos/async/$cancel_task/status"
git add ".caos/async/$cancel_task/status"
git -c user.email=test@caos -c user.name=caos commit -qm "canceled async task"
cancel_base=$(git rev-parse HEAD)
cancel_ref="refs/caos/conversations/run-update-cancel-$suffix/head"
git push -q caos "HEAD:$cancel_ref"
printf 'late result\n' > cancel-input
cancel_identity=$("$CAOS_CLI" run "$worker" cancel-output -- \
  --subreq="$success_request" --target-ref="$cancel_ref" \
  --task="$cancel_task" --result:@=cancel-input)
case "$cancel_identity" in blob\ *) ;; *) fail "late finish did not preserve blob result" ;; esac
diff cancel-input cancel-output >/dev/null || fail "late finish changed its result"
[ "$(remote_head "$cancel_ref")" = "$cancel_base" ] \
  || fail "late finish advanced the canceled task's ref"
echo "  ok: canceled stayed canceled; late result still passed through" >&2

echo "run-and-update-ref: ALL PASS" >&2
