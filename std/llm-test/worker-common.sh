#!/bin/bash
# Shared worker-side plumbing for the scripted v3 conversation tests.

fail() { echo "FAIL: $*" >&2; exit 1; }

LLM_TEST_T0=$(date +%s%3N)
LLM_TEST_PIDS=()

stage() {
  echo "== $* == [+$(( $(date +%s%3N) - LLM_TEST_T0 ))ms]" >&2
}

pass() {
  local name=$1
  stage "done"
  printf '%s: ALL PASS\n' "$name" > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
}

llm_test_cleanup() {
  local pid
  for pid in "${LLM_TEST_PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
}
trap llm_test_cleanup EXIT

assert_oid() {
  [ "${#1}" -eq 40 ] || fail "$2 is not an oid: $1"
  case "$1" in *[!0-9a-f]*) fail "$2 is not an oid: $1" ;; esac
}

publish_tree() {
  local source=$1 destination=$2 failure=$3
  caos put "$source" "$destination" >/dev/null || fail "$failure"
  caos hash "$destination"
}

fetch_code() {
  local oid=$1 failure=$2
  git -c fetch.negotiationAlgorithm=noop fetch -q caos "$oid" || fail "$failure"
}

wait_for_file() {
  local path=$1 tries=${2:-300} attempt
  for ((attempt = 0; attempt < tries; attempt++)); do
    if [ -e "$path" ]; then return 0; fi
    sleep 0.2
  done
  return 1
}

LLM_TEST_TS=1700000000

llm_test_setup() {
  caos get -r /cas/args/stub || fail "reading the llm-stub entry"
  caos get -r /cas/args/tool || fail "reading the llm-test-tool entry"
  caos get /cas/args/test-salt || fail "reading --test-salt"
  SALT=$(cat /cas/args/test-salt)
  stub_bin=/tmp/llm-stub
  TOOL=/tmp/llm-test-tool
  install -m 755 /cas/args/stub/bin/llm-stub "$stub_bin" \
    || fail "staging llm-stub"
  install -m 755 /cas/args/tool/bin/llm-test-tool "$TOOL" \
    || fail "staging llm-test-tool"
  [ -e /cas/args/secret-hash ] \
    || fail "this job carries no secret-hash: caos-tools/test must grant dev/worker-test the mock key"
  LLM_TEST_SECRET_HASH=$(caos hash /cas/args/secret-hash) \
    || fail "reading the secret-hash identity"
  LLM_TEST_LLM_STEP=$(caos hash /cas/args/llm-step) \
    || fail "reading the llm-step identity"
  assert_oid "$LLM_TEST_SECRET_HASH" "the secret-hash identity"
  assert_oid "$LLM_TEST_LLM_STEP" "the llm-step identity"

  local self ip names
  self=$(cat /etc/hostname) || fail "no /etc/hostname"
  stub_host=""
  while read -r ip names; do
    case " $names " in *" $self "*) stub_host=$ip; break ;; esac
  done < /etc/hosts
  [ -n "$stub_host" ] || fail "no /etc/hosts entry for $self"

  : "${CAOS_SERVER_URL:?these tests need CAOS_SERVER_URL from the runner}"
  rm -rf /tmp/repo
  mkdir -p /tmp/repo
  cd /tmp/repo || fail "entering the test repository"
  git init -q .
  git config gc.auto 0
  git remote add caos "$CAOS_SERVER_URL"
  LLM_TEST_NEW_CONVERSATION=1
}

start_stub() {
  local fixture_dir=$1 pid_variable=${2:-STUB_PID} port_variable=${3:-STUB_PORT}
  local candidate_pid candidate_port ready

  for _ in 1 2 3 4 5; do
    candidate_port=$((20000 + RANDOM % 20000))
    "$stub_bin" "0.0.0.0:$candidate_port" "$fixture_dir" 2>"$fixture_dir/log" &
    candidate_pid=$!
    ready=0
    for _ in {1..400}; do
      if ! kill -0 "$candidate_pid" 2>/dev/null; then break; fi
      if (exec 3<>"/dev/tcp/127.0.0.1/$candidate_port") 2>/dev/null; then
        ready=1
        break
      fi
      sleep 0.005
    done
    if [ "$ready" = 1 ]; then
      LLM_TEST_PIDS+=("$candidate_pid")
      printf -v "$pid_variable" '%s' "$candidate_pid"
      printf -v "$port_variable" '%s' "$candidate_port"
      return
    fi
    kill "$candidate_pid" 2>/dev/null || true
    wait "$candidate_pid" 2>/dev/null || true
  done

  fail "could not start llm-stub for $fixture_dir: $(cat "$fixture_dir/log")"
}

mint_commit() {
  local dst=$1 tree=$2 message=$3
  shift 3
  local p
  { printf 'tree %s\n' "$tree"
    for p in "$@"; do printf 'parent %s\n' "$p"; done
    printf 'author caos <test@caos> %s +0000\n' "$LLM_TEST_TS"
    printf 'committer caos <test@caos> %s +0000\n' "$LLM_TEST_TS"
    printf '\n%s\n' "$message"
  } > /tmp/llm-test-commit
  caos put-commit /tmp/llm-test-commit "$dst" || fail "minting $dst"
}

new_llm_conversation() {
  local suffix=$1 stub_port=$2 tree=$3 system=${4:-You are a coding agent.}
  local existing_base=${5:-} curry_failure=${6:-currying llm-step} test_run_id
  test_run_id="$(date +%s%N)-$$-$RANDOM"
  conv="${test_run_id}-${suffix}"
  conversation_ref=""
  LLM_TEST_ROOT_WORKSPACE=""
  if [ "$tree" != - ]; then
    base=$(mint_commit "/cas/conv-base-$suffix" "$tree" "base ($SALT)")
    LLM_TEST_ROOT_WORKSPACE=$base
  elif [ -n "$existing_base" ]; then
    base=$existing_base
    LLM_TEST_ROOT_WORKSPACE=$base
  fi
  head=""
  LLM_TEST_NEW_CONVERSATION=1
  printf '%s' "$system" > /tmp/system.txt
  caos put /tmp/system.txt "/cas/system-$suffix" >/dev/null || fail "publishing the system prompt"
  llm=$(caos curry --base:hash="$LLM_TEST_LLM_STEP" \
    --system:@="/cas/system-$suffix" --model=test-model \
    --base-url="http://$stub_host:$stub_port" --conversation="$conv") \
    || fail "$curry_failure"
}

admit_turn() {
  local message=$1 output username key value
  local parsed_head="" parsed_request="" parsed_human="" parsed_ref=""
  local -a turn_args
  username=${LLM_TEST_USERNAME:-tester}
  turn_args=(turn --repo /tmp/repo --id "$conv" --user "$username" --title "$conv"
    --actor "$username" --text "$message" --secret-hash "$LLM_TEST_SECRET_HASH"
    --model test-model --configuration "$llm")
  if [ "$LLM_TEST_NEW_CONVERSATION" -eq 1 ]; then
    if [ -n "$LLM_TEST_ROOT_WORKSPACE" ]; then
      turn_args+=(--workspace "main=$LLM_TEST_ROOT_WORKSPACE")
    fi
  else
    turn_args+=(--head "$head")
  fi
  output=$($TOOL "${turn_args[@]}") || fail "admitting and publishing the turn"
  while read -r key value; do
    case "$key" in
      head) parsed_head=$value ;;
      request) parsed_request=$value ;;
      human) parsed_human=$value ;;
      ref) parsed_ref=$value ;;
    esac
  done <<<"$output"
  assert_oid "$parsed_head" "the admitted conversation head"
  assert_oid "$parsed_request" "the prepared turn request"
  assert_oid "$parsed_human" "the human message head"
  [ -n "$parsed_ref" ] || fail "the turn helper did not report the conversation ref"
  admitted=$parsed_head
  request=$parsed_request
  conversation_ref=$parsed_ref
  head=$admitted
  LLM_TEST_NEW_CONVERSATION=0
}

start_turn() {
  caos sub-run "$request" >/dev/null || fail "dispatching the turn"
}

dispatch_turn() {
  admit_turn "$1"
  start_turn
}

remote_tip() {
  local line
  line=$(git ls-remote --refs caos "$1") || return 1
  [ -n "$line" ] || return 1
  printf '%s\n' "${line%%[[:space:]]*}"
}

dump_conversation() {
  local parent_ref=$1 parent_request=$2 parent_tip payload child_id child_ref child_tip child_request
  echo "--- diagnostics: parent and child state ---" >&2; parent_tip=$(remote_tip "$parent_ref") || parent_tip=""
  $TOOL fetch --repo /tmp/repo --ref "$parent_ref" >&2 || true; $TOOL request --repo /tmp/repo --head "$parent_tip" --id "$parent_request" >&2 || true
  $TOOL tools --repo /tmp/repo --head "$parent_tip" --request "$parent_request" > /tmp/parent-tools.jsonl 2>&2 || true; cat /tmp/parent-tools.jsonl >&2 || true
  echo "--- parent tool observations ---" >&2; jq -r '.result.observation // .result.error // empty' /tmp/parent-tools.jsonl 2>/dev/null | while read -r payload; do echo "[$payload]" >&2; $TOOL read --repo /tmp/repo --head "$parent_tip" --path "$payload" >&2 || true; echo >&2; done || true
  echo "--- parent transcript ---" >&2; $TOOL transcript --repo /tmp/repo --head "$parent_tip" >&2 || true
  echo "--- parent run trace ---" >&2; curl -s "$CAOS_SERVER_URL/status/$parent_request?all=1" 2>/dev/null | head -c 6000 >&2 || true; echo >&2
  $TOOL children --repo /tmp/repo --head "$parent_tip" > /tmp/children.jsonl 2>&2 || true; cat /tmp/children.jsonl >&2 || true; child_id=$(jq -r '.id' /tmp/children.jsonl 2>/dev/null | head -1) || true
  if [ -n "$child_id" ]; then
    child_ref=$($TOOL ref --id "$child_id") || child_ref=""; child_ref=${child_ref#ref }
    child_tip=$($TOOL fetch --repo /tmp/repo --ref "$child_ref" 2>&2) || child_tip=""; child_tip=${child_tip#head }
    echo "child ref $child_ref tip ${child_tip:-absent}" >&2
    if [ -n "$child_tip" ]; then $TOOL request --repo /tmp/repo --head "$child_tip" >&2 || true; $TOOL transcript --repo /tmp/repo --head "$child_tip" >&2 || true; fi
    child_request=$(jq -r '.request' /tmp/children.jsonl 2>/dev/null | head -1) || true
    echo "--- child run trace ---" >&2; curl -s "$CAOS_SERVER_URL/status/$child_request?all=1" 2>/dev/null | head -c 6000 >&2 || true; echo >&2
  fi
}

wait_turn() {
  local limit=${1:-120} output rc=0 key value terminal="" status=""
  output=$($TOOL wait-terminal --repo /tmp/repo --ref "$conversation_ref" \
    --request "$request" --timeout-secs "$limit") || rc=$?
  while read -r key value; do
    case "$key" in
      head) terminal=$value ;;
      status) status=$value ;;
      error) echo "request error: $value" >&2 ;;
    esac
  done <<<"$output"
  if [ "$rc" -ne 0 ]; then
    fail "the turn did not finish successfully (status ${status:-unknown}, exit $rc)"
  fi
  [ "$status" = idle ] || fail "the turn ended with status ${status:-unknown}"
  assert_oid "$terminal" "the terminal conversation head"
  head=$terminal
  printf '%s\n' "$terminal"
}

record() {
  $TOOL read --repo /tmp/repo --head "$1" --path "$2"
}

workspace_commit() {
  local conversation_head=$1 name=${2:-main} output key value commit=""
  output=$($TOOL workspace --repo /tmp/repo --head "$conversation_head" --name "$name") \
    || fail "reading workspace $name"
  while read -r key value; do
    if [ "$key" = commit ]; then commit=$value; fi
  done <<<"$output"
  assert_oid "$commit" "workspace $name"
  printf '%s\n' "$commit"
}

assert_spine() {
  local conversation_head=$1 parents last="" oid kind
  parents=$($TOOL parents --repo /tmp/repo --head "$conversation_head" --validate) \
    || fail "walking the conversation spine"
  while read -r oid kind; do
    assert_oid "$oid" "spine commit"
    [ -n "$kind" ] || fail "spine commit $oid has no registered kind"
    last=$kind
  done <<<"$parents"
  [ "$last" = conversation.root ] || fail "the conversation spine did not end at its root"
}

transcript_text() {
  local conversation_head=$1 _ordinal _role _actor _message_id encoded
  while read -r _ordinal _role _actor _message_id encoded; do
    printf '%s\n' "$encoded" | jq -r .
  done < <($TOOL transcript --repo /tmp/repo --head "$conversation_head")
}
