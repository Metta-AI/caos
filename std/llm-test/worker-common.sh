#!/bin/bash
# Shared mechanics for the scripted-LLM tests, WORKER-SIDE. The behaviour under
# test stays in each test's worker.sh; this file owns the plumbing that a
# conversation scenario would otherwise copy.
#
# It is the worker counterpart of common.sh, and the four differences from it
# are the whole reason these tests stopped being client tests:
#
#   THE TURN IS DISPATCHED, NOT AWAITED.  A worker may not block on a run, but
#   the stub HTTP server only exists while this container does — so the turn
#   cannot be a `run-then` continuation either (that records and exits, killing
#   the stub before the model is ever called). `dispatch_turn` uses `sub-run`
#   and `wait_turn` watches the CONVERSATION REF, which is the same thing the
#   client was watching.
#
#   THE SECRET-HASH IS COPIED ACROSS.  `caos prepare-request` in a worker folds
#   no `secret-hash` (it has no store), and the server fail-closes when a job's
#   ArgTree lacks the digest its matched grants imply — so a request a worker
#   forms is refused. Having the server fold one in instead (`run_image`, behind
#   `run-then`) does not help either: llm-step's admission protocol requires the
#   conversation to name the EXACT request hash before the run starts, and that
#   hash would then be unpredictable. What resolves it is that caos-tools/test
#   grants the same MOCK key, under the SAME entropy, to std/llm-step,
#   std/llm-call AND dev/worker-test — so this job's own `secret-hash` entry
#   holds the very digest llm-step's job implies, and `dispatch_turn` binds it
#   onto the request it forms.
#
#   COMMITS ARE MINTED, NOT COMMITTED.  `caos put` publishes a tree and
#   `caos put-commit` mints a commit from raw bytes; there is no worktree and
#   nothing is added or committed.
#
#   THE IMAGE IS dev/worker-test.  git is needed for exactly one thing — reading
#   and writing refs on the server, which caos gives a worker no verb for.

fail() { echo "FAIL: $*" >&2; exit 1; }

LLM_TEST_T0=$(date +%s%3N)
LLM_TEST_PIDS=()

stage() {
  echo "== $* == [+$(( $(date +%s%3N) - LLM_TEST_T0 ))ms]" >&2
}

llm_test_cleanup() {
  local pid
  for pid in "${LLM_TEST_PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
}
trap llm_test_cleanup EXIT

assert_oid() { # <value> <description>
  [ "${#1}" -eq 40 ] || fail "$2 is not an oid: $1"
  case "$1" in *[!0-9a-f]*) fail "$2 is not an oid: $1" ;; esac
}

# A fixed author time. Nothing here asserts on it, and pinning it keeps a
# re-minted commit byte-identical; uniqueness comes from --test-salt in the
# message where a test wants a genuinely fresh conversation.
LLM_TEST_TS=1700000000

# The stub binary, this container's address, and a repo wired to the server.
# `--stub` (std/llm-stub, a cargo `--cmd=build` result, executable at bin/<name>)
# and `--test-salt` must be bound by the caller's expression.
llm_test_setup() {
  caos get -r /cas/args/stub || fail "reading the llm-stub entry"
  caos get /cas/args/test-salt || fail "reading --test-salt"
  SALT=$(cat /cas/args/test-salt)
  # Copied out because materialized CAS content is read-only and owner-only —
  # exec straight from /cas is "Permission denied".
  stub_bin=/tmp/llm-stub
  install -m 755 /cas/args/stub/bin/llm-stub "$stub_bin" || fail "staging llm-stub"

  # THIS CONTAINER'S OWN ADDRESS, not 127.0.0.1: the llm-step worker is a
  # separate container, so loopback would be its own. Read from /etc/hosts in
  # bash — there is no `hostname`, `getent` or `ip` here, and an absent binary
  # is exit 127 at runtime, not a build error.
  local self ip names
  self=$(cat /etc/hostname) || fail "no /etc/hostname"
  stub_host=""
  while read -r ip names; do
    case " $names " in *" $self "*) stub_host=$ip; break ;; esac
  done < /etc/hosts
  [ -n "$stub_host" ] || fail "no /etc/hosts entry for $self"

  : "${CAOS_SERVER_URL:?these tests need CAOS_SERVER_URL from the runner}"
  rm -rf /tmp/repo && mkdir -p /tmp/repo && cd /tmp/repo
  git init -q .
  git config user.email test@caos
  git config user.name caos
  git config gc.auto 0
  git remote add caos "$CAOS_SERVER_URL"
}

start_stub() { # <fixture-dir> <pid-variable> <port-variable>
  local fixture_dir=$1 pid_variable=$2 port_variable=$3
  local candidate_pid candidate_port ready

  for _ in 1 2 3 4 5; do
    candidate_port=$((20000 + RANDOM % 20000))
    "$stub_bin" "0.0.0.0:$candidate_port" "$fixture_dir" 2>"$fixture_dir/log" &
    candidate_pid=$!
    # Wait for the LISTENER, not a fixed interval: the stub binds in a few ms.
    # Probing the port also tells the two failures apart — a dead process (retry
    # on another port) against one still coming up.
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

# A commit, minted as raw bytes at a commit-typed CAS path (which is what makes
# `--name:@=` carry it as a gitlink). Prints its hash.
mint_commit() { # <cas-path> <tree-oid> <message> [parent...]
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

# A fresh conversation over <tree-oid>, and the llm-step image configured for
# the stub. Sets `conv`, `conversation_ref`, `base` and `llm` for the caller.
new_llm_conversation() { # <suffix> <port> <tree-oid> [system-text]
  local suffix=$1 stub_port=$2 tree=$3 system=${4:-You are a coding agent.}
  local test_run_id
  test_run_id="$(date +%s%N)-$$-$RANDOM"
  conv="${test_run_id}-${suffix}"
  conversation_ref="refs/caos/v2/conversations/$conv/head"
  base=$(mint_commit /cas/conv-base "$tree" "base ($SALT)")
  printf '%s' "$system" > /tmp/system.txt
  caos put /tmp/system.txt /cas/system >/dev/null || fail "publishing the system prompt"
  llm=$(caos curry --base:hash="$(caos hash /cas/args/llm-step)" \
    --system:@=/cas/system --model=test-model \
    --base-url="http://$stub_host:$stub_port" --conversation="$conv") \
    || fail "currying llm-step"
}

# Publish a human turn and the admission event the worker expects, then dispatch
# the request. Sets `human`, `request` and `admitted`; the conversation ref is
# left at `admitted`, and wait_turn takes it from there.
dispatch_turn() { # <tree-oid> <human-message> [parent-commit]
  local tree=$1 message=$2 parent=${3:-$base}
  # `human` is a global: tests assert the turn descends from it.
  human=$(mint_commit /cas/human "$tree" \
    "{\"base\":\"$parent\",\"author\":\"user\",\"content\":\"$message\"}" "$parent")
  # `--secret-hash` BOUND BY HAND, and it is what makes this work at all.
  # llm-step's admission protocol requires the conversation to name the EXACT
  # request hash before the run starts, and a secret-bearing request carries a
  # `secret-hash` entry. A worker's `prepare-request` folds none (it has no
  # store), and the server folding one in produces a hash nobody here could
  # predict. But this job is granted the same mock key under the same entropy,
  # so its OWN `secret-hash` entry holds the very digest llm-step's job implies
  # — copying it across is what makes the request the test names and the request
  # the server runs the same object.
  [ -e /cas/args/secret-hash ] \
    || fail "this job carries no secret-hash: caos-tools/test must grant dev/worker-test the mock key"
  request=$(caos prepare-request --base:hash="$llm" --head:@=/cas/human \
    --secret-hash:@=/cas/args/secret-hash) || fail "preparing the turn request"
  assert_oid "$request" "the prepared turn request"
  admitted=$(mint_commit /cas/admitted "$tree" \
    "{\"request\":\"$request\",\"request_head\":\"$human\",\"status\":\"queued\"}" "$human")
  git -c fetch.negotiationAlgorithm=noop fetch -q caos "$admitted" \
    || fail "fetching the admission commit back"
  git push -q caos "$admitted:$conversation_ref" \
    || fail "publishing the request admission"

  # DISPATCHED BY IDENTITY, deliberately: `sub-run` runs exactly the ArgTree
  # named in the admission, which is the one thing llm-step will accept. It also
  # returns immediately, which is required — the stub only lives as long as this
  # container, so the turn cannot be a `run-then` continuation.
  caos sub-run "$request" >/dev/null || fail "dispatching the turn"
}

remote_tip() { # <ref>
  local line
  line=$(git ls-remote --refs caos "$1") || return 1
  [ -n "$line" ] || return 1
  printf '%s\n' "${line%%[[:space:]]*}"
}

# Wait for this request's TERMINAL event and print its commit. This is the only
# completion signal a hosting worker has: it cannot block on the run, and the
# turn's commit oid is not predictable.
#
# TERMINAL, NOT "THE REF MOVED". llm-step appends as it goes — `running` first,
# then a commit per round — so the first movement is an event with no workspace
# changes in it at all. (Observed: `fatal: path 'notes/new.txt' does not exist`,
# on the `running` event.) A terminal event is one for this request whose status
# is `idle` or `failed` (llm-step's own `terminal_for_run`), and `failed` is
# reported here rather than left to time out.
wait_turn() { # [seconds]
  local limit=${1:-120} now status
  local ticks=$(( limit * 10 ))
  local seen=""
  for _ in $(seq 1 "$ticks"); do
    now=$(remote_tip "$conversation_ref" 2>/dev/null || true)
    if [ -n "$now" ] && [ "$now" != "$seen" ]; then
      seen=$now
      git -c fetch.negotiationAlgorithm=noop fetch -q caos "$now" \
        || fail "fetching the turn head $now"
      status=$(git show -s --format=%B "$now" \
        | jq -r --arg r "$request" 'select(.request == $r) | .status // empty' 2>/dev/null || true)
      case "$status" in
        idle) printf '%s\n' "$now"; return 0 ;;
        failed) fail "the turn ended failed: $(git show -s --format=%B "$now")" ;;
      esac
    fi
    sleep 0.1
  done
  return 1
}
