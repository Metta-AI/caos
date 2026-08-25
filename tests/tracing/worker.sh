#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (tests/lib stages the repo, then runs this).
#
# Exercises tracing (SPEC.md "Tracing"): the server's per-ArgTree trace record
# and the `/status` view over it.
#
# It has to assert while the run is STILL GOING. `/status` is the live view —
# a node whose work is wholly finished is skipped — so a completed run renders
# as `null`, and a test that only looked afterwards would pass against a server
# that recorded nothing at all. Hence the background run over deliberately slow
# map children, and the poll against the ArgTree hash `prepare-request` hands
# back before anything is dispatched.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }   # epoch milliseconds

# THIS TEST MUST NOT CACHE, and nothing outside it arranges that. The suite's
# `--test-salt` re-keys the test JOB; the requests this script makes inside the
# stack are keyed by their own content, and the stack is SHARED and long-lived —
# so the second suite run to reach here would be served from the first one's
# memo. A cache hit runs nothing and therefore RECORDS nothing, which is exactly
# what the live view has nothing to show for: the test passed once and then
# failed forever, on an unchanged tree. Own salt, per invocation (tests/cargo-self
# does the same for the same reason).
CAOS_SALT="tracing-$(date +%s%N)-$$"
export CAOS_SALT

# Long enough that the poll below reliably lands inside the window, short
# enough not to pad the suite. The children run in parallel, so this is the
# whole cost, not twice it.
HOLD=6

echo "== build the fixture images ==" >&2
combine=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/combine.sh)
# `--hold` is part of the map image's ArgTree, so the two holds below are two
# DIFFERENT requests. That is deliberate: the sanity run must not warm the
# cache for the live one, because a cache hit runs nothing and records nothing,
# which would make the live assertion unfalsifiable.
mkdriver() { # <hold>
  local slow
  slow=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/slow.sh --hold="$1")
  "$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
    --worker1:@=test/driver.sh --map-img="$slow" --then-img="$combine"
}
driver=$(mkdriver "$HOLD")

echo "== the fixture pipeline works at all ==" >&2
# Synchronous, and first: everything below is timing-sensitive, and a broken
# fixture (a map-then that never forms, an out-trace put that is refused) would
# otherwise surface as "the poll timed out" — a symptom that points nowhere.
quick=$(mkdriver 0)
"$CAOS_CLI" run --base:hash="$quick" --in:@=test/tree > quick.out 2> quick.err \
  || fail "the fixture run failed: $(tail -c 2000 quick.err)"
[ "$(cat quick.out)" = "first=alpha
second=beta" ] || fail "fixture produced: $(cat quick.out)"
echo "  ok: driver -> map -> then produces the combined children" >&2

echo "== the request's identity is known before it is dispatched ==" >&2
# The same ArgTree `run` will form: prepare-request pushes it and prints its
# hash, and the hash IS the key the trace is recorded under. Without this the
# test would have to guess what to poll.
req=$("$CAOS_CLI" prepare-request --base:hash="$driver" --in:@=test/tree)
[ "${#req}" -eq 40 ] || fail "expected a 40-character ArgTree hash, got: $req"
echo "  ok: request $req" >&2

echo "== nothing is recorded before the work is requested ==" >&2
# The record is written by the run, so an ArgTree that has never run has no
# node — and asking is an answer, not an error.
"$CAOS_CLI" status "$req" > before.json 2> before.err \
  || fail "status on an unrun ArgTree failed: $(cat before.err)"
[ ! -s before.json ] || fail "expected no work under an unrun ArgTree, got: $(cat before.json)"
grep -q "no current work" before.err \
  || fail "expected 'no current work', got: $(cat before.err)"
echo "  ok: an unrun ArgTree has no node" >&2

echo "== the live view shows the fan-out while it is running ==" >&2
t0=$(ms)
# `set +e` inside the subshell: it inherits this script's `set -e`, so a failing
# run would kill the subshell BEFORE it could record the status the test wants
# to report — turning a legible assertion into a silent background death.
(
  set +e
  "$CAOS_CLI" run --base:hash="$driver" --in:@=test/tree > run.out 2> run.err
  echo $? > run.rc
) &
runner=$!

# Two polls, not one. "No node at all" and "a node but no children" are
# different failures — the first means nothing was recorded, the second means
# the child edges were not — and a single combined wait reports them
# identically, which costs a whole debugging round trip to tell apart.
poll() { # <jq-predicate> <what>
  local i=0
  while [ "$i" -lt 60 ]; do
    if "$CAOS_CLI" status "$req" > live.json 2> live.err; then
      if [ -s live.json ] && jq -e "$1" live.json > /dev/null 2>&1; then
        return 0
      fi
    fi
    i=$((i + 1))
    sleep 0.5
  done
  # Everything known about why, in one place: the run's own stderr (it may have
  # died before recording anything), and what /status last said.
  echo "---- run.err ----" >&2
  tail -c 3000 run.err >&2 || true
  echo "---- run.rc: $(cat run.rc 2>/dev/null || echo '(still running)') ----" >&2
  echo "---- last status stdout ----" >&2
  cat live.json >&2 || true
  echo "---- last status stderr ----" >&2
  cat live.err >&2 || true
  fail "never saw $2 under $req"
}

poll 'has("name")' "any node"
echo "  ok: the request has a node while it runs" >&2
poll '(.children // []) | length >= 2' "the map's children"

# The root is the driver, and its children are the map entries BY NAME. The
# map name is prepended to each child's own name, which is the only thing that
# makes a wide fan-out readable — a bare hash per child would not.
names=$(jq -r '(.children // [])[].name' live.json | sort | tr '\n' ' ')
# EXACTLY the entry names, with nothing appended. These children carry no
# `help` (they are curried from `base`, not from a tool's own tree), and the
# image they run is the same for all of them — so appending it would give every
# line of a fan-out the same sixty characters after its one useful word.
[ "$names" = "first second " ] \
  || fail "expected children named for the map entries, got: $names"
echo "  ok: children are $names" >&2

# Every node carries its ArgTree, so a reader can go straight from a node to
# the key that makes it.
[ "$(jq -r '.arg_tree' live.json)" = "$req" ] \
  || fail "the root node is not the request: $(jq -r '.arg_tree' live.json)"

echo "== a queued or running child says which it is ==" >&2
# `requested` is written before dispatch and `started` when a runner claims the
# job, so the pair distinguishes "waiting for capacity" from "running" — the
# state that is otherwise invisible until it times out.
jq -e '.children[] | has("requested")' live.json > /dev/null \
  || fail "a child has no requested time: $(cat live.json)"
echo "  ok: children carry their admission times" >&2

echo "== the parent's /cas/out-trace is on its node ==" >&2
# driver.sh put perf data at /cas/out-trace before tail-calling. It arrived
# with the worker's result and is NOT in the result — a result is
# content-addressed and keys everything downstream of it.
oid=$(jq -r '.out_trace[0] // empty' live.json)
[ -n "$oid" ] || fail "no out-trace on the driver node: $(cat live.json)"
"$CAOS_CLI" get "$oid" perf.txt 2>/dev/null || fail "cannot read out-trace $oid"
grep -q "driver-fanned-out" perf.txt \
  || fail "out-trace content is wrong: $(cat perf.txt)"
echo "  ok: out-trace $oid reads back" >&2

echo "== the run completes with the fan-out's results ==" >&2
wait "$runner" || true
t1=$(ms)
[ "$(cat run.rc)" = "0" ] || fail "the run failed: $(tail -c 2000 run.err)"
got=$(cat run.out)
[ "$got" = "first=alpha
second=beta" ] || fail "expected the combined children, got: $got"
echo "  ok: run -> $(echo "$got" | tr '\n' ' ')" >&2

echo "== a finished run has no current work ==" >&2
# The other half of the live view's contract, asserted so it cannot drift into
# showing stale nodes: once everything under the request is done, there is
# nothing current to show.
"$CAOS_CLI" status "$req" > after.json 2>/dev/null || true
[ ! -s after.json ] || fail "expected no current work after completion: $(cat after.json)"
echo "  ok: the completed run renders as nothing" >&2

echo "== a malformed hash is refused rather than guessed at ==" >&2
if "$CAOS_CLI" status not-a-hash > /dev/null 2> bad.err; then
  fail "expected a malformed hash to be refused"
fi
grep -q "40-character" bad.err || fail "unhelpful error: $(cat bad.err)"
echo "  ok: refused with a message that says what is wanted" >&2

echo "tracing perf (ms):" >&2
echo "  run=$((t1 - t0)) (holds ${HOLD}s in the map children)" >&2
echo "tracing: ALL PASS" >&2
