#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI)
# -----------------------------------------------------------
# Its subject is run-then itself — the single-valued continuation
# `{in, run?, then?, catch?}` (design/map-then.md) — and that subject exists
# ONLY as behaviour of a real request evaluated by the tested client against
# the inner server. There is no file on disk to lint and no shortcut that would
# still be checking the thing: the properties below are what the interpreter
# DOES when a continuation runs, so the only way to pin them down is to make the
# tested client submit one and watch what comes back.
#
#   - a tail call (--run only) delivers the sub-run's value AS the request's
#     result — a claim about how the client encodes the continuation and how the
#     server resolves it, observable only by running it;
#   - the sub-run's result threads into `then` as --result — a data-flow the
#     server performs between two jobs, invisible unless a real second job sees
#     it;
#   - a promise returned from the run position is COLLAPSED by the server before
#     `then` sees --result — a server-side fixpoint with no on-disk shadow;
#   - a run-then CYCLE is detected by the SERVER (not the client), so only a
#     real self-recursive request in flight can trip it;
#   - --catch reroutes a failing run to `then` as --error instead of failing the
#     request, with the uncaught default asserted right beside it so neither can
#     silently become the other — a difference only a live failing run reveals;
#   - even the client-side flag validation (--map/--run exclusivity, --run/--then
#     requirements) is a property OF the tested client's own arg parser, so it is
#     exactly $CAOS_CLI's behaviour under test.
#
# The CLI is therefore essential, not incidental: swap in a different client and
# every assertion here is about a different thing. The workers are curried bash
# scripts (see the *.sh fixtures), so no new images are needed.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }   # epoch milliseconds

echo "21" > in.txt
git add in.txt
git -c user.email=test@caos -c user.name=caos commit -qm 'input'

# The run/then steps: double.sh writes 2*<in>; combine.sh writes
# "in=<in> result=<result>". driver.sh run-thens over --in with whatever
# run-img/then-img were curried into it.
double=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/double.sh)
combine=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/combine.sh)

echo "== run with no then: a plain tail call to run ==" >&2
tail_driver=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/driver.sh --run-img="$double")
t0=$(ms); n=$("$CAOS_CLI" run --base:hash="$tail_driver" --in:@=in.txt); t1=$(ms)
[ "$n" = "42" ] || fail "expected 42, got: $n"
echo "  ok: run(--in=21) -> 42 is the request's result" >&2

echo "== run + then: the result threads into then as --result ==" >&2
both_driver=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/driver.sh --run-img="$double" --then-img="$combine")
t2=$(ms); s=$("$CAOS_CLI" run --base:hash="$both_driver" --in:@=in.txt); t3=$(ms)
[ "$s" = "in=21 result=42" ] || fail "expected 'in=21 result=42', got: $s"
echo "  ok: then saw --in=21 and --result=42" >&2

echo "== an identical request is a cache hit with the same value ==" >&2
t4=$(ms); s2=$("$CAOS_CLI" run --base:hash="$both_driver" --in:@=in.txt); t5=$(ms)
[ "$s2" = "$s" ] || fail "cached rerun differs: $s2 vs $s"
echo "  ok: rerun -> same value" >&2

echo "== a nested promise from the run position resolves ==" >&2
# outer.sh's whole body is itself a run-then (over the curried double), so the
# driver's `run` sub-run returns a promise the server must collapse before
# combine sees --result.
outer=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/outer.sh --inner-img="$double")
nested_driver=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/driver.sh --run-img="$outer" --then-img="$combine")
s=$("$CAOS_CLI" run --base:hash="$nested_driver" --in:@=in.txt)
[ "$s" = "in=21 result=42" ] || fail "nested promise: expected 'in=21 result=42', got: $s"
echo "  ok: run's promise collapsed to 42 before then" >&2

echo "== --map/--run exclusivity and missing --run are rejected client-side ==" >&2
ok=$("$CAOS_CLI" run --base:@=DEEP-DEPS/bash --worker1:@=test/checks.sh --in:@=in.txt)
[ "$ok" = "ok" ] || fail "checks.sh did not pass: $ok"
echo "  ok: bad flag combinations refused before anything is recorded" >&2

echo "== without --catch, a failing run fails the whole request ==" >&2
# The default, asserted so `--catch` below can't quietly become the behaviour
# everywhere: a pipeline that loses a step has no business reporting success.
boom=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/boom.sh)
catcher=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/catcher.sh)
uncaught_driver=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/driver.sh --run-img="$boom" --then-img="$catcher")
if "$CAOS_CLI" run --base:hash="$uncaught_driver" --in:@=in.txt 2>boom.err; then
  fail "expected the failing run to fail the request, but it succeeded"
fi
grep -q "exit status: 1" boom.err \
  || fail "no worker failure reported; got: $(cat boom.err)"
echo "  ok: the run's failure propagated" >&2

echo "== with --catch, the failure reaches then as --error ==" >&2
# Same failing run, same then — only the flag differs, so what changes is the
# interpreter's handling and nothing else.
caught_driver=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/driver.sh --run-img="$boom" --then-img="$catcher" --catch=1)
c=$("$CAOS_CLI" run --base:hash="$caught_driver" --in:@=in.txt) \
  || fail "expected --catch to make the request succeed"
[ "$c" = "in=21 caught=yes" ] || fail "expected 'in=21 caught=yes', got: $c"
echo "  ok: then ran with --in and --error, and the request succeeded" >&2

echo "== a run-then cycle is detected (by the server) ==" >&2
# cycle.sh re-curries itself (content-addressed, so the sub-request is
# byte-identical to the in-flight one) and run-thens the same input.
cyc=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/cycle.sh)
if "$CAOS_CLI" run --base:hash="$cyc" --in:@=in.txt 2>cyc.err; then
  fail "expected the self-recursive run-then to fail, but the run succeeded"
fi
grep -q "run cycle detected" cyc.err || fail "no cycle reported; got: $(cat cyc.err)"
echo "  ok: run failed with a run-cycle error" >&2

# tail = 2 cold jobs (driver + run); then = 3; rerun = 0 (pure cache hit).
echo "run-then perf (ms):" >&2
echo "  tail=$((t1 - t0))  then=$((t3 - t2))  cached=$((t5 - t4))" >&2
echo "run-then: ALL PASS" >&2
