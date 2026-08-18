#!/usr/bin/env bash
# Shared mechanics for the scripted-LLM integration tests. The behavior under
# test stays in each cli.sh; this file only owns fixture plumbing that otherwise
# gets copied every time a conversation scenario is split for concurrency.

fail() { echo "FAIL: $*" >&2; exit 1; }

LLM_TEST_T0=$(date +%s%3N)
LLM_TEST_PIDS=()

stage() {
  echo "== $* == [+$(( $(date +%s%3N) - LLM_TEST_T0 ))ms]" >&2
}

gc() { git -c user.email=test@caos -c user.name=caos "$@"; }

commit() {
  git add -A
  gc commit -qm "$1"
}

mkcommit() { # <tree> <message> [parent] -> commit
  local tree=$1 message=$2 parent=${3:-}
  local parents=()
  if [ -n "$parent" ]; then parents=(-p "$parent"); fi
  gc commit-tree "$tree" "${parents[@]}" -m "$message"
}

remote_exact_ref() { # <ref>
  curl -fsS -X POST -H 'content-type: application/json' \
    --data "{\"ref\":\"$1\"}" "$CAOS_SERVER_URL/ref/read"
}

remote_tip() { # <ref>
  local lines
  lines=$(git ls-remote --refs caos "$1") || return 1
  [ -n "$lines" ] || return 1
  [ "${lines#*$'\n'}" = "$lines" ] || return 1
  printf '%s\n' "${lines%%[[:space:]]*}"
}

fetch_head() {
  local head
  head=$(remote_tip "$conversation_ref") \
    || fail "canonical conversation head is absent"
  git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$head" \
    || fail "fetching canonical conversation head"
  printf '%s\n' "$head"
}

assert_oid() { # <value> <description>
  local value=$1 description=$2
  [ "${#value}" -eq 40 ] && [[ "$value" =~ ^[0-9a-f]+$ ]] \
    || fail "$description is not an oid: $value"
}

llm_test_cleanup() {
  local pid
  for pid in "${LLM_TEST_PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
}
trap llm_test_cleanup EXIT

llm_test_setup() {
  "$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry \
    || fail "resolving llm-stub"
  stub_bin=/tmp/llm-stub-bin
  install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"
  stub_host=${CAOS_STUB_HOST:-host.containers.internal}
}

start_stub() { # <fixture-dir> <pid-variable> <port-variable>
  local fixture_dir=$1 pid_variable=$2 port_variable=$3
  local candidate_pid candidate_port ready

  for _ in 1 2 3 4 5; do
    candidate_port=$((20000 + RANDOM % 20000))
    "$stub_bin" "0.0.0.0:$candidate_port" "$PWD/$fixture_dir" \
      2>"$fixture_dir/log" &
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

new_llm_conversation() { # <suffix> <port>
  local suffix=$1 stub_port=$2 test_run_id
  test_run_id="$(date +%s%N)-$$-$RANDOM"
  # Globals are the function's interface to the sourcing test.
  # shellcheck disable=SC2034
  conv="${test_run_id}-${suffix}"
  # shellcheck disable=SC2034
  conversation_ref="refs/caos/v2/conversations/$conv/head"
  # shellcheck disable=SC2034
  llm=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/llm-step \
    --api-key=test-key --system:@=system.txt \
    --model=test-model --base-url="http://$stub_host:$stub_port" \
    --conversation="$conv")
}
