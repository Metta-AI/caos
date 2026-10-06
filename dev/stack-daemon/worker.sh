#!/usr/bin/env bash
# dev/stack-daemon/worker.sh — a caos dev stack as a RESIDENT WORKER
# (design/daemons.md, Part 2).
#
# One container per TREE UNDER TEST. A message is an ordinary job whose reserved
# `affinity` arg is that tree's oid, so the server hands every message for the
# tree to this one process, one at a time, in arrival order, and this script
# answers each and calls `caos next`. The first message for a tree wakes the
# container; later ones find the dev stack it brought up still running.
#
# THE OPS (the `op` arg):
#
#   start       bring the stack up if it is not, publish an iroh listener on it,
#               and answer with the ticket a cloud session reaches it by. Needs
#               `relay`, a relay of our own (design/iroh-transport.md).
#   run-tests   bring the stack up if it is not, run the suite on it, answer with
#               the suite's result tree. What `caos-test` was.
#   status      what is up, and for how long.
#   logs        a stack member's log from a cursor.
#   harvest     export refs from the stack's git to the outer server, under
#               refs/stacks/<instance>/ — the stack's conversations die with it.
#   stop        stop the stack and leave.
#
# `status`, `logs`, `harvest` and `stop` NEVER start a stack: asked of a tree
# with none, they say so. That costs a container start, and the container then
# exits, because nothing is resident for it to keep.
#
# TWO LEVELS OF CAOS, as in the old caos-test, and keeping them straight is the
# shape of this file:
#
#   OUT HERE   `caos` is the host's. /cas/args is this message's, `caos next` is
#              this container's runner, and what this puts at /cas/out goes back
#              to whoever sent the message.
#   IN THERE   the dev stack `dev/stack-up` brings up from the tree, driven by
#              the client that tree compiled, at CAOS_SERVER_URL=http://127.0.0.1.
#
# WHAT CHANGES FROM ONE MESSAGE TO THE NEXT, and so is read afresh each time:
# /cas/args (the runner replaces it), including its `salt`, and the job nonce, which
# `caos` reads from a file the runner rewrites. No worker has either in its
# environment: a daemon outlives its first job, so a copy there would go stale.
# The one place this script passes a salt on is to the inner client, below.
set -euo pipefail

fail() { echo "STACK-DAEMON FAIL: $*" >&2; exit 1; }

# PHASE MARKERS, as everywhere in this tree: the container log is the only
# account of where a minute went.
T0=$SECONDS
phase() { echo "==> [+$((SECONDS - T0))s] $*" >&2; }

# Where this daemon keeps its own state. /tmp survives `caos next` (the runner's
# narrow reset leaves the scratch directories alone) and dies with the container,
# which is the lifetime of everything recorded here.
WS=/tmp/ws                     # the tree under test, as a writable git workspace
UP=/tmp/stack.up               # exists once the dev stack is ready
STARTED=/tmp/stack.started     # when, in $SECONDS terms of this process
IROH_PID=/tmp/iroh.pid
HARVEST_REFS=/tmp/harvest.refs # what the last `harvest` asked for, for the last one
CLI=/caos-run/bin/caos-cli
RUN=/caos-run
# Where this stack's logs are on the HOST, which is what a caller can read: /caos-run
# is a name that exists only in here.
HOST_LOGS=/caos-dev/runs/$(cat /etc/hostname)/logs

# ---- reading this message ---------------------------------------------------

have() { [ -e "/cas/args/$1" ]; }
arg() { caos get "/cas/args/$1" >/dev/null; cat "/cas/args/$1"; }
opt() { if have "$1"; then arg "$1"; fi; }

# The text this message is answered with. A BLOB, printed verbatim by both
# callers (SPEC, "Returning a result").
reply() { caos put "$1" /cas/out >/dev/null; }
reply_text() { printf '%s\n' "$*" > /tmp/reply; reply /tmp/reply; }

# ---- the stack --------------------------------------------------------------

# This message's salt, for the inner client. See the header.
salt() { opt salt; }

# Bring the dev stack up from the tree under test, once. Everything that stood in
# std/caos-test/worker.sh above "the suite" is here, unchanged but for being a
# function: the stack's processes are children of THIS container and outlive the
# message that started them, which is the whole point.
ensure_stack() {
  if [ -e "$UP" ]; then return 0; fi

  phase "materializing the tree under test"
  caos get -r /cas/args/in || fail "materializing the source tree"
  # A COPY, because /cas/args goes with this message and the stack's repo has to
  # live as long as the stack. Writable: stack-up makes it a git repo.
  rm -rf "$WS"
  cp -R /cas/args/in "$WS"
  chmod -R u+w "$WS"
  cd "$WS"

  # The wrong tree is a CLEAN RESULT, not an error: caos-test and caos-stack are
  # offered on every conversation, and run on one that is not caos they have no
  # stack to build. The routers say so before sending a message at all; this is
  # the same guard for a message that arrived without them.
  if [ ! -f flake.nix ] || [ ! -x dev/stack-up ]; then
    fail "this tree is not the caos codebase (no flake.nix / dev/stack-up)"
  fi

  # ONE nix build for everything the stack needs; stack-up resolves nothing.
  phase "building the stack inputs"
  inputs=$(nix build "path:$PWD#caos-test-stack-inputs" --no-link --print-out-paths) \
    || fail "building the stack inputs"
  echo "$inputs" > /tmp/stack.inputs
  phase "bringing the dev stack up"
  ./dev/stack-up --inputs="$inputs" >&2 || fail "bringing the dev stack up"
  [ -x "$CLI" ] || fail "no client at $CLI after stack-up"
  echo "$SECONDS" > "$STARTED"
  touch "$UP"
  phase "the dev stack is up"
}

# Put the iroh listener in front of the stack, if it is not, and print its
# ticket. A MEMBER OF THIS CONTAINER like the rest of the stack, started here
# rather than through stack/serve's CAOS_STACK_IROH because the stack may already
# be up, brought up by a `run-tests`, when the first `start` arrives — and a
# listener needs only the server, which is.
#
# The state directory is this stack's own (under $RUN), so the key and token are
# minted on first use here and the daemon needs no secret and takes no ticket in.
ensure_iroh() {
  local relay=$1 advertise=${2:-}
  local state=$RUN/iroh
  if [ -e "$IROH_PID" ] && kill -0 "$(cat "$IROH_PID")" 2>/dev/null; then
    cat "$state/ticket"
    return 0
  fi
  mkdir -p "$state"
  rm -f "$state/ticket"
  local inputs
  inputs=$(cat /tmp/stack.inputs)
  local flags=()
  local addr
  for addr in $advertise; do
    flags+=(--advertise "$addr")
  done
  # --git-dir is the repo the dev stack's server uses (stack/serve:
  # $CAOS_STACK_STATE/git, and stack-up's state is /caos-dev).
  "$inputs/bin/caos-iroh" serve \
    --state "$state" \
    --to 127.0.0.1:80 \
    --git-dir /caos-dev/git \
    --relay "$relay" \
    "${flags[@]}" > "$RUN/logs/iroh.log" 2>&1 &
  echo $! > "$IROH_PID"
  local _
  for _ in $(seq 1 600); do
    if [ -s "$state/ticket" ]; then cat "$state/ticket"; return 0; fi
    if ! kill -0 "$(cat "$IROH_PID")" 2>/dev/null; then
      cat "$RUN/logs/iroh.log" >&2
      fail "the iroh listener died (relay $relay)"
    fi
    sleep 0.1
  done
  cat "$RUN/logs/iroh.log" >&2
  fail "the iroh listener published no ticket in 60s (relay $relay)"
}

# ---- ops --------------------------------------------------------------------

op_start() {
  local relay
  relay=$(opt relay)
  # Before anything is brought up: a refused message should cost nothing.
  if [ -z "$relay" ]; then
    fail "start needs a relay: a caos:// ticket is reached through a relay of your own, and n0's are never used"
  fi
  ensure_stack
  local ticket
  ticket=$(ensure_iroh "$relay" "$(opt advertise)")
  {
    echo "ticket=$ticket"
    echo "phase=ready"
    echo "instance=$(arg affinity)"
    echo "logs=$HOST_LOGS"
  } > /tmp/reply
  reply /tmp/reply
}

op_run_tests() {
  ensure_stack
  cd "$WS"

  # THE TESTED CLIENT GOES INTO THE SOURCE TREE. `:@=` ingests git-tracked paths
  # inside the worktree and nothing else, so a client sitting in /caos-run is
  # rejected as "outside the git worktree". The client is part of what the suite
  # TESTS, so a tree that carries it re-keys when it changes. `add -f`, because
  # .gitignore knows nothing about this path.
  install -m 755 "$CLI" ./.caos-test-cli || fail "staging the tested client"
  git add -f ./.caos-test-cli || fail "tracking the tested client"

  local args=()
  if have test-salt; then args+=("--test-salt=$(arg test-salt)"); fi
  if have only; then args+=("--only=$(arg only)"); fi
  # HOW MANY TESTS RUN AT ONCE, passed to the suite's `map-then`, which is the
  # only place that can bound it.
  if have max-parallel; then args+=("--max-parallel=$(arg max-parallel)"); fi

  # THE TREE THE SUITE RUNS IS A COMMIT THE DEV STACK HOLDS, so a secret can be
  # granted to a path in it by locator (SPEC, "Secrets"). Committed at a fixed
  # date, like stack-up's, so an unchanged tree is an unchanged commit — which is
  # also why the second run-tests of a tree repeats this harmlessly.
  GIT_AUTHOR_DATE="@0 +0000" GIT_COMMITTER_DATE="@0 +0000" \
    git -c user.name=caos -c user.email=dev@caos commit -q --allow-empty -m "tested client" \
    || fail "committing the tested client"
  git push -q http://127.0.0.1 "HEAD:refs/heads/caos-test/$(git rev-parse HEAD)" \
    || fail "pushing the workspace commit to the dev stack"

  # A MOCK KEY for std/llm-call, std/llm-step, dev/worker-test and std/go — not a
  # secret: the value is a constant, and the only thing that ever sees it is a
  # stub HTTP server a test starts in its own container. Granted by locator to
  # paths in the commit above, so it is pushed ONCE: the store is a function of
  # that commit and a second push of an unchanged one is a no-op the server
  # sequence check would refuse.
  local store=/tmp/caos-test-secrets
  if [ ! -e /tmp/secrets.pushed ]; then
    local key
    key=$("$CLI" secrets-init --dir="$store") || fail "creating the test store"
    git config caos.secret-readers "$key"
    local at="git+http://caos.invalid/caos-test?rev=$(git rev-parse HEAD)&dir"
    {
      printf 'name=anthropic-api-key\n'
      printf 'value=mock-key-for-the-llm-call-stub\n'
      printf 'entropy=fedcba9876543210fedcba9876543210\n'
      printf 'reader:@@=%s=std/llm-call\n' "$at"
      printf 'reader:@@=%s=std/llm-step\n' "$at"
      printf 'reader:@@=%s=dev/worker-test\n' "$at"
      printf 'reader:@@=%s=std/go\n' "$at"
    } > "$store/llm-mock"
    "$CLI" secrets-push --dir="$store" --server=http://127.0.0.1 >/dev/null || fail "pushing the test store"
    touch /tmp/secrets.pushed
    echo "==> granting llm-call, llm-step, dev/worker-test and std/go a mock key" >&2
  fi

  # THE SUITE'S REQUEST, NAMED BEFORE IT RUNS, so the two stacks' traces join up.
  # The dev stack writes its trace records to the SAME redis as the host — a trace
  # key carries no cache namespace — so both sets already sit side by side; what
  # was missing was an edge from this job to the suite's, and `caos trace-child`
  # records exactly that. `prepare-request` forms and pushes the very ArgTree the
  # `run` below will form, so the hash is known before any work starts.
  #
  # CAOS_SALT IS SET HERE, for the inner client, from this message's file. The
  # (A worker has no CAOS_SALT of its own; see the header.)
  #
  # NO COMMENT INSIDE THE BLOCKS BELOW: a continuation followed by a comment
  # joins INTO the comment and severs the environment prefix.
  phase "forming the suite request"
  local suite_req
  suite_req=$(CAOS_SALT="$(salt)" CAOS_SERVER_URL=http://127.0.0.1 \
    "$CLI" prepare-request --base:@=dev/run-tests --in:@=. --cli:@=.caos-test-cli "${args[@]}") \
    || fail "forming the suite request"
  caos trace-child suite "$suite_req" || fail "linking the suite's trace to this job"
  echo "==> suite request $suite_req (caos-cli status --all $suite_req)" >&2
  echo "==> full trace JSON:" >&2
  echo "    curl -s localhost:9090/status/$suite_req?all=1 | jless" >&2

  phase "running the suite"
  # A previous run's checkout is read-only, and this one is its replacement.
  if [ -e /tmp/suite ]; then chmod -R u+w /tmp/suite; fi
  rm -rf /tmp/suite
  local status=0
  CAOS_SALT="$(salt)" CAOS_SERVER_URL=http://127.0.0.1 \
    "$CLI" run /tmp/suite --base:@=dev/run-tests --in:@=. --cli:@=.caos-test-cli "${args[@]}" \
      >/tmp/run.out 2>/tmp/run.err || status=$?
  if [ "$status" -ne 0 ]; then
    cat /tmp/run.err >&2
    fail "the suite did not produce a report (exit $status)"
  fi

  # THE TRACE COMMAND GOES IN THE REPORT, not just on stderr above: this job's
  # stderr is relayed only when it FAILS, and the run you most want to take apart
  # is a green one that was slower than it should have been. The result tree is
  # a checkout, so its files are read-only.
  chmod u+w /tmp/suite/report
  {
    echo
    echo "full trace (the host reads the dev stack's records — a trace key carries"
    echo "no cache namespace, so both stacks write to the one redis):"
    echo "  curl -s localhost:9090/status/$suite_req?all=1 | jless"
    echo
    echo "and while a run is in flight, CAOS_WATCH_LINES=0 shows every node"
    echo "instead of the first 16."
    echo
    if [ -f "$RUN/logs/runnerd.log" ]; then
      echo "runnerd: $(grep -c 'running job' "$RUN/logs/runnerd.log" || true) claimed," \
        "$(wc -l < "$RUN/logs/runnerd.log") lines"
      echo "this stack's logs, on the host: /caos-dev/runs/$(cat /etc/hostname)/logs"
    fi
  } >> /tmp/suite/report

  # THE WHOLE RESULT TREE, not just the report. A tree with a `report` file has
  # the report printed, and a FAILED banner in it marks the call a failure, while
  # `results/<test>` stays addressable, which is what lets `caos-test-result
  # <hash>` read one test's full output.
  phase "done"
  reply /tmp/suite
}

# Answer "not running" for an op that would not start a stack. True if it did.
not_running() {
  if [ -e "$UP" ]; then return 1; fi
  reply_text "not running: no stack has been started for $(arg affinity)"
  return 0
}

op_status() {
  if not_running; then return 0; fi
  local up=$((SECONDS - $(cat "$STARTED")))
  {
    echo "instance=$(arg affinity)"
    echo "stack=up"
    echo "uptime_seconds=$up"
    echo "container=$(cat /etc/hostname)"
    echo "logs=$HOST_LOGS"
    if [ -e "$IROH_PID" ] && kill -0 "$(cat "$IROH_PID")" 2>/dev/null; then
      echo "iroh=listening"
      echo "ticket=$(cat "$RUN/iroh/ticket")"
    else
      echo "iroh=off"
    fi
    # A member that died is the failure worth being loud about, and the log
    # names are the ones `logs` takes.
    echo "logs_available=$(ls "$RUN/logs" | tr '\n' ' ')"
  } > /tmp/reply
  reply /tmp/reply
}

# `logs`: bytes of one member's log from a cursor. The reply's FIRST line is the
# cursor to pass next time, so a caller that loops never repeats a line.
op_logs() {
  if not_running; then return 0; fi
  local name cursor file size
  name=$(opt log)
  name=${name:-serve}
  case "$name" in
    *[!a-z0-9-]* | "") fail "log name $name is not a log: use the name of a file in $RUN/logs, without .log" ;;
  esac
  cursor=$(opt cursor)
  cursor=${cursor:-0}
  case "$cursor" in
    *[!0-9]*) fail "cursor $cursor is not a byte offset" ;;
  esac
  file=$RUN/logs/$name.log
  if [ ! -f "$file" ]; then
    reply_text "no log $name; available: $(ls "$RUN/logs" | tr '\n' ' ')"
    return 0
  fi
  size=$(wc -c < "$file")
  # Capped, so one call cannot carry a whole log back as a result.
  local want=$((size - cursor))
  if [ "$want" -gt 65536 ]; then want=65536; fi
  if [ "$want" -lt 0 ]; then cursor=0; want=$size; fi
  {
    echo "cursor=$((cursor + want))"
    tail -c +"$((cursor + 1))" "$file" | head -c "$want"
  } > /tmp/reply
  reply /tmp/reply
}

# Export refs from the stack's git to the OUTER server. The stack's repo is
# in this container and dies with it, and so would the conversations a cloud
# session recorded in it.
#
# The daemon does the pushing, with the OUTER client's server URL; the code under
# test never gets outer write access. Under refs/stacks/<instance>/, so a stack's
# harvest can overwrite only its own.
#
# The source repo, /caos-dev/git, is SHARED by every dev stack on the host, so a
# pattern broader than conversations may export a neighbor's refs too. The
# default is the conversations, which are named by hash and so are no one else's.
harvest() { # <refs, one pattern per line>
  local instance patterns pattern dst
  instance=$(arg affinity)
  patterns=$1
  : > /tmp/harvested
  while IFS= read -r pattern; do
    if [ -z "$pattern" ]; then continue; fi
    case "$pattern" in
      refs/*) ;;
      *) fail "harvest pattern $pattern must start with refs/" ;;
    esac
    dst=refs/stacks/$instance/${pattern#refs/}
    # A pattern that matches nothing is the normal case early in a session.
    if [ -n "$(git -C /caos-dev/git for-each-ref --count=1 --format=x "$pattern")" ]; then
      git -C /caos-dev/git push -q "$CAOS_SERVER_URL" "+$pattern:$dst" \
        || fail "pushing $pattern to the outer server"
      echo "$dst" >> /tmp/harvested
    fi
  done <<<"$patterns"
}

DEFAULT_HARVEST='refs/caos/v3/conversations/*'

op_harvest() {
  if not_running; then return 0; fi
  local refs
  refs=$(opt refs)
  refs=${refs:-$DEFAULT_HARVEST}
  # Remembered, so the last harvest — during the grace period after a leave — is
  # the same one the caller last asked for.
  printf '%s\n' "$refs" > "$HARVEST_REFS"
  harvest "$refs"
  {
    echo "harvested=$(grep -c . /tmp/harvested || true)"
    cat /tmp/harvested
  } > /tmp/reply
  reply /tmp/reply
}

# The last harvest, as the runner's SIGTERM-to-SIGKILL grace period allows: an
# eviction or a lifetime cap loses little. Never fatal — the stack is going away
# either way, and an error here would hide why.
final_harvest() {
  if [ ! -e "$UP" ] || [ ! -e "$HARVEST_REFS" ]; then return 0; fi
  phase "final harvest"
  (harvest "$(cat "$HARVEST_REFS")") || echo "final harvest failed" >&2
}

op_stop() {
  if not_running; then return 0; fi
  reply_text "stopped $(arg affinity)"
  # Leaving is exiting normally: the runner reaps what this container started —
  # the stack, the listener — and the container goes with them.
  exit 0
}

# ---- the loop ---------------------------------------------------------------

handle() {
  case "$(arg op)" in
    start) op_start ;;
    run-tests) op_run_tests ;;
    status) op_status ;;
    logs) op_logs ;;
    harvest) op_harvest ;;
    stop) op_stop ;;
    *) fail "unknown op $(arg op)" ;;
  esac
}

# A daemon is asked for its own tree and no other. The router names the tree
# twice — as `affinity` and as `in` — so the two disagreeing is a caller that
# formed the message by hand.
check_affinity() {
  if have in && [ "$(caos hash /cas/args/in)" != "$(arg affinity)" ]; then
    fail "affinity $(arg affinity) is not the hash of in $(caos hash /cas/args/in)"
  fi
}

n=0
while true; do
  n=$((n + 1))
  check_affinity
  handle

  # A message answered "not running" by a daemon with no stack has nothing to
  # keep resident: exit as any worker does, and the runner goes back to polling.
  if [ ! -e "$UP" ]; then exit 0; fi

  # `caos next` returns 0 with the next message's args at /cas/args, or 10 to say
  # leave — evicted, past its lifetime, or its lease lapsed. Anything else is an
  # error, and dying on it is right.
  if caos next; then continue; fi
  rc=$?
  if [ "$rc" -eq 10 ]; then
    final_harvest
    exit 0
  fi
  exit "$rc"
done
