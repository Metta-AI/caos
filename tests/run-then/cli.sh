#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# Exercises run-then — the single-valued map-then (the continuation
# `{in, run?, then?}`): a plain tail call (--run only), the sub-run's result
# threading into `then` as --result, a nested promise from the run position,
# the client-side --map/--run mutual exclusion, and run-cycle detection. The
# workers are curried bash scripts (see the *.sh fixtures), so no new images
# are needed.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }   # epoch milliseconds

echo "21" > in.txt
git add in.txt
git -c user.email=test@caos -c user.name=caos commit -qm 'input'

# The run/then steps: double.sh writes 2*<in>; combine.sh writes
# "in=<in> result=<result>". driver.sh run-thens over --in with whatever
# run-img/then-img were curried into it.
double=$("$CAOS_CLI" curry /cas/std/bash -- --script:@=test/double.sh)
combine=$("$CAOS_CLI" curry /cas/std/bash -- --script:@=test/combine.sh)

echo "== run with no then: a plain tail call to run ==" >&2
tail_driver=$("$CAOS_CLI" curry /cas/std/bash -- \
  --script:@=test/driver.sh --run-img="$double")
t0=$(ms); n=$("$CAOS_CLI" run "$tail_driver" -- --in:@=in.txt); t1=$(ms)
[ "$n" = "42" ] || fail "expected 42, got: $n"
echo "  ok: run(--in=21) -> 42 is the request's result" >&2

echo "== run + then: the result threads into then as --result ==" >&2
both_driver=$("$CAOS_CLI" curry /cas/std/bash -- \
  --script:@=test/driver.sh --run-img="$double" --then-img="$combine")
t2=$(ms); s=$("$CAOS_CLI" run "$both_driver" -- --in:@=in.txt); t3=$(ms)
[ "$s" = "in=21 result=42" ] || fail "expected 'in=21 result=42', got: $s"
echo "  ok: then saw --in=21 and --result=42" >&2

echo "== an identical request is a cache hit with the same value ==" >&2
t4=$(ms); s2=$("$CAOS_CLI" run "$both_driver" -- --in:@=in.txt); t5=$(ms)
[ "$s2" = "$s" ] || fail "cached rerun differs: $s2 vs $s"
echo "  ok: rerun -> same value" >&2

echo "== a nested promise from the run position resolves ==" >&2
# outer.sh's whole body is itself a run-then (over the curried double), so the
# driver's `run` sub-run returns a promise the server must collapse before
# combine sees --result.
outer=$("$CAOS_CLI" curry /cas/std/bash -- \
  --script:@=test/outer.sh --inner-img="$double")
nested_driver=$("$CAOS_CLI" curry /cas/std/bash -- \
  --script:@=test/driver.sh --run-img="$outer" --then-img="$combine")
s=$("$CAOS_CLI" run "$nested_driver" -- --in:@=in.txt)
[ "$s" = "in=21 result=42" ] || fail "nested promise: expected 'in=21 result=42', got: $s"
echo "  ok: run's promise collapsed to 42 before then" >&2

echo "== --map/--run exclusivity and missing --run are rejected client-side ==" >&2
ok=$("$CAOS_CLI" run /cas/std/bash -- --script:@=test/checks.sh --in:@=in.txt)
[ "$ok" = "ok" ] || fail "checks.sh did not pass: $ok"
echo "  ok: bad flag combinations refused before anything is recorded" >&2

echo "== a run-then cycle is detected (by the server) ==" >&2
# cycle.sh re-curries itself (content-addressed, so the sub-request is
# byte-identical to the in-flight one) and run-thens the same input.
cyc=$("$CAOS_CLI" curry /cas/std/bash -- --script:@=test/cycle.sh)
if "$CAOS_CLI" run "$cyc" -- --in:@=in.txt 2>cyc.err; then
  fail "expected the self-recursive run-then to fail, but the run succeeded"
fi
grep -q "run cycle detected" cyc.err || fail "no cycle reported; got: $(cat cyc.err)"
echo "  ok: run failed with a run-cycle error" >&2

# tail = 2 cold jobs (driver + run); then = 3; rerun = 0 (pure cache hit).
echo "run-then perf (ms):" >&2
echo "  tail=$((t1 - t0))  then=$((t3 - t2))  cached=$((t5 - t4))" >&2
echo "run-then: ALL PASS" >&2
