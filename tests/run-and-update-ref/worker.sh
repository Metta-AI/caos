#!/bin/bash
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

caos get /cas/args/test-salt || fail "reading --test-salt"
SALT=$(cat /cas/args/test-salt)
TS=1700000000

caos get /cas/args/succeed || fail "reading succeed.sh"
caos get /cas/args/fail || fail "reading fail.sh"
case "$stage" in
start|succeeded|failed)
  caos get -r /cas/args/tool || fail "reading llm-test-tool"
  TOOL=/tmp/llm-test-tool
  install -m 755 /cas/args/tool/bin/llm-test-tool "$TOOL" || fail "staging llm-test-tool"
  ;;
esac

next() {
  local next_stage=$1
  shift
  caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 \
    --stage="$next_stage" --test-salt:@=/cas/args/test-salt \
    --bash:@=/cas/args/bash --updater:@=/cas/args/updater --tool:@=/cas/args/tool \
    --succeed:@=/cas/args/succeed --fail:@=/cas/args/fail "$@"
}

: "${CAOS_SERVER_URL:?this test needs CAOS_SERVER_URL from the runner}"
rm -rf /tmp/repo
mkdir -p /tmp/repo
cd /tmp/repo
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$CAOS_SERVER_URL"

remote_head() {
  local line
  line=$(git ls-remote --refs caos "$1") || return 1
  [ -n "$line" ] || return 1
  printf '%s\n' "${line%%[[:space:]]*}"
}

assert_oid() {
  [ "${#1}" -eq 40 ] || fail "$2 is not an oid: $1"
  case "$1" in *[!0-9a-f]*) fail "$2 is not an oid: $1" ;; esac
}

only_status_event() {
  local before=$1 after=$2 task=$3 status=$4 result=$5 workspace=$6
  local changed record workspace_output current_workspace
  git -c fetch.negotiationAlgorithm=noop fetch -q caos "$after" \
    || fail "fetching terminal async head $after"
  [ "$(git rev-parse "$after^1")" = "$before" ] \
    || fail "the $status event did not append to the prior head"
  changed=$(git diff-tree --no-commit-id --name-only -r "$before" "$after")
  [ "$changed" = ".caos/async/$task.json" ] \
    || fail "the $status event changed paths other than its async record: $changed"
  record=$($TOOL async --repo /tmp/repo --head "$after" --task "$task") \
    || fail "reading terminal async record"
  [ "$(jq -r .status <<<"$record")" = "$status" ] \
    || fail "the async record has the wrong status"
  [ "$(jq -r .result <<<"$record")" = "$result" ] \
    || fail "the async record has the wrong result"
  workspace_output=$($TOOL workspace --repo /tmp/repo --head "$after" --name main) \
    || fail "reading main pointer"
  current_workspace=${workspace_output%%$'\n'*}
  current_workspace=${current_workspace#commit }
  [ "$current_workspace" = "$workspace" ] || fail "the status event changed main"
  git -c fetch.negotiationAlgorithm=noop fetch -q caos "$current_workspace" \
    || fail "fetching main workspace"
  [ "$(git show "$current_workspace:workspace.txt")" = "workspace survives" ] \
    || fail "workspace.txt changed"
}

case "$stage" in
start|succeeded)
  BASH=$(caos hash /cas/args/bash)
  UPDATER=$(caos hash /cas/args/updater)
  success_request=$(caos prepare-request --base:hash="$BASH" \
    --worker1:@=/cas/args/succeed --case=success) || fail "preparing the successful R"
  failure_request=$(caos prepare-request --base:hash="$BASH" \
    --worker1:@=/cas/args/fail --case=failure) || fail "preparing the failing R"

  rm -rf /tmp/expected
  mkdir -p /tmp/expected
  printf 'payload from R\n' > /tmp/expected/payload
  printf '%s\n' "$success_request" > /tmp/expected/request
  caos put /tmp/expected /cas/expected >/dev/null || fail "building R's expected result"
  success_result=$(caos hash /cas/expected)
  ;;
esac

case "$stage" in
start)
  echo "== initialize v3 conversation roots ==" >&2
  mkdir -p /tmp/ws
  printf 'workspace survives\n' > /tmp/ws/workspace.txt
  caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the conversation workspace"
  ws_tree=$(caos hash /cas/ws)
  { printf 'tree %s\n' "$ws_tree"
    printf 'author caos <test@caos> %s +0000\n' "$TS"
    printf 'committer caos <test@caos> %s +0000\n' "$TS"
    printf '\nworkspace base (%s)\n' "$SALT"
  } > /tmp/base.commit
  workspace=$(caos put-commit /tmp/base.commit /cas/workspace-base) \
    || fail "minting workspace commit"
  test_run_id="$(date +%s%N)-$$-$RANDOM"
  success_id="${test_run_id}-run-update-success"
  failure_id="${test_run_id}-run-update-failure"
  success_ref=$($TOOL ref --id "$success_id")
  success_ref=${success_ref#ref }
  failure_ref=$($TOOL ref --id "$failure_id")
  failure_ref=${failure_ref#ref }
  success_root=$($TOOL root --repo /tmp/repo --id "$success_id" --title "$success_id" \
    --workspace "main=$workspace")
  success_root=${success_root#head }
  failure_root=$($TOOL root --repo /tmp/repo --id "$failure_id" --title "$failure_id" \
    --workspace "main=$workspace")
  failure_root=${failure_root#head }
  $TOOL create --repo /tmp/repo --ref "$success_ref" --user tester \
    --id "$success_id" --new "$success_root" >/dev/null || fail "creating success conversation"
  $TOOL create --repo /tmp/repo --ref "$failure_ref" --user tester \
    --id "$failure_id" --new "$failure_root" >/dev/null || fail "creating failure conversation"

  success_task=$(caos prepare-request --base:hash="$UPDATER" \
    --subreq="$success_request" --target-ref="$success_ref") \
    || fail "preparing the successful Q"
  assert_oid "$success_task" "successful Q"
  success_pending=$($TOOL async-start --repo /tmp/repo --head "$success_root" \
    --task "$success_task" --target-ref "$success_ref" --ref "$success_ref") \
    || fail "recording successful Q as pending"
  success_pending=${success_pending#head }
  caos run-request-then "$success_task" \
    --then:hash="$(next succeeded --success-before="$success_pending" \
      --failure-root="$failure_root" --workspace="$workspace" \
      --success-ref="$success_ref" --failure-ref="$failure_ref" \
      --success-task="$success_task")"
  ;;

succeeded)
  for arg_name in success-before failure-root workspace success-ref failure-ref success-task; do
    caos get "/cas/args/$arg_name" || fail "reading --$arg_name"
  done
  success_before=$(cat /cas/args/success-before)
  failure_root=$(cat /cas/args/failure-root)
  workspace=$(cat /cas/args/workspace)
  success_ref=$(cat /cas/args/success-ref)
  failure_ref=$(cat /cas/args/failure-ref)
  success_task=$(cat /cas/args/success-task)
  success_head=$(remote_head "$success_ref") || fail "successful Q appended no event"
  only_status_event "$success_before" "$success_head" "$success_task" complete \
    "$success_result" "$workspace"
  caos get-hash "$success_result" /cas/actual || fail "result is not fetchable"
  caos get -r /cas/actual || fail "materializing the result"
  diff -r /tmp/expected /cas/actual >/dev/null || fail "Q changed R's result"

  failure_task=$(caos prepare-request --base:hash="$UPDATER" \
    --subreq="$failure_request" --target-ref="$failure_ref") \
    || fail "preparing the failing Q"
  assert_oid "$failure_task" "failing Q"
  failure_pending=$($TOOL async-start --repo /tmp/repo --head "$failure_root" \
    --task "$failure_task" --target-ref "$failure_ref" --ref "$failure_ref") \
    || fail "recording failing Q as pending"
  failure_pending=${failure_pending#head }
  caos run-request-then "$failure_task" \
    --then:hash="$(next failed --failure-before="$failure_pending" \
      --workspace="$workspace" --failure-ref="$failure_ref" \
      --failure-task="$failure_task")"
  ;;

failed)
  for arg_name in failure-before workspace failure-ref failure-task; do
    caos get "/cas/args/$arg_name" || fail "reading --$arg_name"
  done
  failure_before=$(cat /cas/args/failure-before)
  workspace=$(cat /cas/args/workspace)
  failure_ref=$(cat /cas/args/failure-ref)
  failure_task=$(cat /cas/args/failure-task)
  failure_head=$(remote_head "$failure_ref") || fail "failing Q appended no event"
  git -c fetch.negotiationAlgorithm=noop fetch -q caos "$failure_head" \
    || fail "fetching failed async head"
  failure_record=$($TOOL async --repo /tmp/repo --head "$failure_head" --task "$failure_task")
  failure_identity=$(jq -r .result <<<"$failure_record")
  assert_oid "$failure_identity" "failure result"
  only_status_event "$failure_before" "$failure_head" "$failure_task" failed \
    "$failure_identity" "$workspace"
  caos get-hash "$failure_identity" /cas/failed || fail "failure result is not fetchable"
  caos get -r /cas/failed || fail "materializing failure result"
  [ -d /cas/failed ] || fail "failed Q did not return a result tree"
  [ "$(cat /cas/failed/status)" = failed ] || fail "failed result has no failed status"
  grep -q "exit status: 23" /cas/failed/error \
    || fail "structured failure lost R's exit status: $(cat /cas/failed/error)"
  printf 'run-and-update-ref: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
