#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set — normally INSIDE a testenv worker, as the suite's per-test job
# (tests/lib/run-nested.sh); tests/run.sh runs it on the host against the
# outer stack for interactive debugging.
#
# Exercises the per-crate decomposition (worker-cargo mode=all,
# design/cargo-workers.md phase 2) on a two-crate workspace, b -> a:
# a passing check/test; a broken *dep* (a) whose failure propagates to its
# dependent's job as a value (b's section shows a's diagnostics — no compile
# of b was attempted against a broken a); and per-crate caching — after a
# fix-and-rerun, an edit to b re-runs b's jobs while a's are cache hits
# (asserted by wall-clock: the b-only edit must be markedly cheaper than the
# cold run... on tiny crates both are fast, so the assertion is on results,
# with timings printed for the eyeball).
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

echo "== mode=all check: a clean two-crate workspace ==" >&2
t0=$(ms)
"$CAOS_CLI" run /cas/std/cargo r1 -- --tree:@=test/ws --cmd=check --mode=all
t1=$(ms)
[ "$(cat r1/exit)" = "0" ] || fail "check: exit $(cat r1/exit); stderr: $(cat r1/stderr)"
echo "  ok: clean check ($((t1 - t0))ms)" >&2

echo "== mode=all test: b's unit test runs ==" >&2
"$CAOS_CLI" run /cas/std/cargo r2 -- --tree:@=test/ws --cmd=test --mode=all
[ "$(cat r2/exit)" = "0" ] || fail "test: exit $(cat r2/exit); stderr: $(cat r2/stderr)"
grep -q "test result: ok. 1 passed" r2/stdout || fail "b's test didn't run: $(cat r2/stdout)"
echo "  ok: tests ran" >&2

echo "== a broken dep propagates to its dependent as a value ==" >&2
sed -i 's/x \* 2/x * "two"/' test/ws/a/src/lib.rs
commit "break a"
"$CAOS_CLI" run /cas/std/cargo r3 -- --tree:@=test/ws --cmd=check --mode=all
[ "$(cat r3/exit)" != "0" ] || fail "broken dep: exit 0"
grep -q "── a ──" r3/stderr || fail "no a section: $(cat r3/stderr)"
grep -q "── b ──" r3/stderr || fail "no b section (propagation): $(cat r3/stderr)"
# b's section carries a's diagnostics — the failure bubbled as a value.
grep -q "cannot multiply" r3/stderr || fail "no diagnostics: $(cat r3/stderr)"
echo "  ok: dep failure propagated with diagnostics" >&2

echo "== fix; edit only b; a's jobs are cache hits ==" >&2
sed -i 's/x \* "two"/x * 2/' test/ws/a/src/lib.rs
commit "fix a"
"$CAOS_CLI" run /cas/std/cargo r4 -- --tree:@=test/ws --cmd=check --mode=all
[ "$(cat r4/exit)" = "0" ] || fail "fixed check failed: $(cat r4/stderr)"
sed -i 's/b says/b announces/' test/ws/b/src/main.rs
commit "edit b"
t2=$(ms)
"$CAOS_CLI" run /cas/std/cargo r5 -- --tree:@=test/ws --cmd=check --mode=all
t3=$(ms)
[ "$(cat r5/exit)" = "0" ] || fail "b-edit check failed: $(cat r5/stderr)"
echo "  ok: b-only edit checked ($((t3 - t2))ms; cold was $((t1 - t0))ms)" >&2

echo "== identical tree: the cached value comes back ==" >&2
t4=$(ms)
"$CAOS_CLI" run /cas/std/cargo r6 -- --tree:@=test/ws --cmd=check --mode=all
t5=$(ms)
cmp -s r5/exit r6/exit || fail "cached rerun differed"
echo "  ok: cached ($((t5 - t4))ms)" >&2
