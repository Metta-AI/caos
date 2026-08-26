#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI)
# ----------------------------------------------------------
# The SUBJECT here is not "does cargo compile these crates" — that could be
# checked with a bare `cargo check`. The subject is the per-crate DECOMPOSITION
# worker-cargo performs (mode=all, design/cargo-workers.md phase 2): the way
# the runner splits a workspace into one job PER CRATE, wires each crate's job
# to its dependencies' jobs, propagates a dependency's failure to its dependent
# AS A VALUE, and CACHES each crate's job independently by content.
#
# That decomposition exists ONLY inside the inner stack. It is produced by the
# tree-under-test's runner/interpreter when a `run` is submitted, executed by
# the inner runnerd, and memoised in the inner server's registry. There is no
# artifact on disk a plain `bash`/`cargo` invocation could inspect to see it:
# running `cargo check --workspace` locally would compile the same crates but
# reveal NOTHING about job granularity, value-vs-error propagation, or per-crate
# cache keys. The only way to observe any of it is to drive a real computation
# through the tested client against the inner stack and read back the results it
# returns — which is exactly what `$CAOS_CLI run … --mode=all` does here.
#
# So the CLI is ESSENTIAL, not incidental: each assertion below reads a fact
# that is only true of a run routed through the tested client —
#   * clean check/test:   the workspace decomposes into crate jobs that succeed
#                         and b's unit test actually runs (r1, r2);
#   * dep propagation:    a broken *dep* (a) surfaces in its DEPENDENT's section
#                         with a's diagnostics — the failure crossed the job
#                         boundary as a value, and b was never compiled against
#                         a broken a (r3);
#   * per-crate caching:  after a fix, editing only b re-runs b's jobs while a's
#                         are cache hits, and an identical tree replays the
#                         cached value (r4-r6).
# None of these are properties of cargo; all are properties of how the tested
# client and the inner stack decompose, propagate, and cache the run. A version
# that shelled out to `cargo` directly would still be green on a client that had
# BROKEN every one of these behaviours — so bypassing $CAOS_CLI would gut the
# test.
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

# Every test runs in the Linux stack, so build musl (statics run there) — the
# system's one target. No host build has a consumer.
tgt="$(uname -m)-unknown-linux-musl"

echo "== mode=all check: a clean two-crate workspace ==" >&2
t0=$(ms)
"$CAOS_CLI" run r1 --base:@=DEEP-DEPS/cargo --tree:@=test/ws --cmd=check --mode=all "--target=$tgt"
t1=$(ms)
[ "$(cat r1/exit)" = "0" ] || fail "check: exit $(cat r1/exit); stderr: $(cat r1/stderr)"
echo "  ok: clean check ($((t1 - t0))ms)" >&2

echo "== mode=all test: b's unit test runs ==" >&2
"$CAOS_CLI" run r2 --base:@=DEEP-DEPS/cargo --tree:@=test/ws --cmd=test --mode=all "--target=$tgt"
[ "$(cat r2/exit)" = "0" ] || fail "test: exit $(cat r2/exit); stderr: $(cat r2/stderr)"
grep -q "test result: ok. 1 passed" r2/stdout || fail "b's test didn't run: $(cat r2/stdout)"
echo "  ok: tests ran" >&2

echo "== a broken dep propagates to its dependent as a value ==" >&2
sed -i 's/x \* 2/x * "two"/' test/ws/a/src/lib.rs
commit "break a"
"$CAOS_CLI" run r3 --base:@=DEEP-DEPS/cargo --tree:@=test/ws --cmd=check --mode=all "--target=$tgt"
[ "$(cat r3/exit)" != "0" ] || fail "broken dep: exit 0"
grep -q "── a ──" r3/stderr || fail "no a section: $(cat r3/stderr)"
grep -q "── b ──" r3/stderr || fail "no b section (propagation): $(cat r3/stderr)"
# b's section carries a's diagnostics — the failure bubbled as a value.
grep -q "cannot multiply" r3/stderr || fail "no diagnostics: $(cat r3/stderr)"
echo "  ok: dep failure propagated with diagnostics" >&2

echo "== fix; edit only b; a's jobs are cache hits ==" >&2
sed -i 's/x \* "two"/x * 2/' test/ws/a/src/lib.rs
commit "fix a"
"$CAOS_CLI" run r4 --base:@=DEEP-DEPS/cargo --tree:@=test/ws --cmd=check --mode=all "--target=$tgt"
[ "$(cat r4/exit)" = "0" ] || fail "fixed check failed: $(cat r4/stderr)"
sed -i 's/b says/b announces/' test/ws/b/src/main.rs
commit "edit b"
t2=$(ms)
"$CAOS_CLI" run r5 --base:@=DEEP-DEPS/cargo --tree:@=test/ws --cmd=check --mode=all "--target=$tgt"
t3=$(ms)
[ "$(cat r5/exit)" = "0" ] || fail "b-edit check failed: $(cat r5/stderr)"
echo "  ok: b-only edit checked ($((t3 - t2))ms; cold was $((t1 - t0))ms)" >&2

echo "== identical tree: the cached value comes back ==" >&2
t4=$(ms)
"$CAOS_CLI" run r6 --base:@=DEEP-DEPS/cargo --tree:@=test/ws --cmd=check --mode=all "--target=$tgt"
t5=$(ms)
cmp -s r5/exit r6/exit || fail "cached rerun differed"
echo "  ok: cached ($((t5 - t4))ms)" >&2
