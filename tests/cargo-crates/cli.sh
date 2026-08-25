#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# Per-crate decomposition (mode=all, design/cargo-workers.md) over a two-crate
# workspace where b depends on a. Asserts: a clean check and a clean test; a
# broken dependency propagating to its dependent AS A VALUE, with both crates'
# sections and the real diagnostics; an edit confined to the dependent; and an
# identical tree served from cache.
#
# A STAGED TEST (dev/run-test/run-test.sh's header): six runs, seven stages,
# and no container parked on a compile.
#
# THE FIXTURE EDITS ARE APPLIED FROM PRISTINE, NOT ACCUMULATED. The old shape
# `sed -i`'d the tree in place and each run saw whatever the previous edits had
# left. cli.sh now runs once per stage in a fresh client repo, so `test/ws`
# starts pristine every time and each stage applies exactly the edits its own
# run needs. That is not a workaround: it makes each run's input a stated
# function of the fixtures rather than of everything that happened before it.
#
# It also makes the "fix a" run honest about itself. Fixing a returns the tree
# to pristine, so that run has the SAME key as the first check and is a cache
# hit — which was true before too, just not visible.
#
# The timings are gone with the blocking calls: `cold`, `b-edit` and `cached`
# each span a container round trip now, so a number here would measure the
# harness rather than per-crate caching. What they illustrated the assertions
# already prove.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# Every test runs in the Linux stack, so build musl (statics run there) — the
# system's one target. No host build has a consumer.
tgt="$(uname -m)-unknown-linux-musl"

# The fixture edits, each applied to a PRISTINE test/ws.
break_a() { sed -i 's/x \* 2/x * "two"/' test/ws/a/src/lib.rs; }
edit_b()  { sed -i 's/b says/b announces/' test/ws/b/src/main.rs; }

cargo_job() { # <cmd> -> the image; test/ws rides as the subject (--in)
  "$CAOS_CLI" curry --base:@=DEEP-DEPS/cargo --cmd="$1" --mode=all "--target=$tgt"
}

case "$STAGE" in

start)
  echo "== mode=all check: a clean two-crate workspace ==" >&2
  stage_next checked "$(cargo_job check)" test/ws
  ;;

checked)
  fetch_result
  [ "$(cat "$RESULT/exit")" = "0" ] \
    || fail "check: exit $(cat "$RESULT/exit"); stderr: $(cat "$RESULT/stderr")"
  echo "  ok: clean check" >&2

  echo "== mode=all test: b's unit test runs ==" >&2
  stage_next tested "$(cargo_job test)" test/ws
  ;;

tested)
  fetch_result
  [ "$(cat "$RESULT/exit")" = "0" ] \
    || fail "test: exit $(cat "$RESULT/exit"); stderr: $(cat "$RESULT/stderr")"
  grep -q "test result: ok. 1 passed" "$RESULT/stdout" \
    || fail "b's test didn't run: $(cat "$RESULT/stdout")"
  echo "  ok: tests ran" >&2

  echo "== a broken dep propagates to its dependent as a value ==" >&2
  break_a
  stage_next broken "$(cargo_job check)" test/ws
  ;;

broken)
  fetch_result
  [ "$(cat "$RESULT/exit")" != "0" ] || fail "broken dep: exit 0"
  grep -q "── a ──" "$RESULT/stderr" || fail "no a section: $(cat "$RESULT/stderr")"
  grep -q "── b ──" "$RESULT/stderr" \
    || fail "no b section (propagation): $(cat "$RESULT/stderr")"
  # b's section carries a's diagnostics — the failure bubbled as a value.
  grep -q "cannot multiply" "$RESULT/stderr" \
    || fail "no diagnostics: $(cat "$RESULT/stderr")"
  echo "  ok: dep failure propagated with diagnostics" >&2

  # `fix a` IS the pristine tree — nothing to apply.
  echo "== fix; edit only b; a's jobs are cache hits ==" >&2
  stage_next fixed "$(cargo_job check)" test/ws
  ;;

fixed)
  fetch_result
  [ "$(cat "$RESULT/exit")" = "0" ] || fail "fixed check failed: $(cat "$RESULT/stderr")"

  edit_b
  stage_next bedit "$(cargo_job check)" test/ws
  ;;

bedit)
  fetch_result
  [ "$(cat "$RESULT/exit")" = "0" ] || fail "b-edit check failed: $(cat "$RESULT/stderr")"
  echo "  ok: b-only edit checked" >&2
  printf '%s\n' "$RESULT_HASH" > "$CARRY_OUT/bedit"

  echo "== identical tree: the cached value comes back ==" >&2
  edit_b
  stage_next cached "$(cargo_job check)" test/ws
  ;;

cached)
  # The whole result by oid: content-addressed, so an equal hash says more than
  # the old `cmp` on two `exit` files did.
  [ "$RESULT_HASH" = "$(cat "$CARRY/bedit")" ] \
    || fail "cached rerun differed: $RESULT_HASH vs $(cat "$CARRY/bedit")"
  echo "  ok: cached" >&2
  echo "cargo-crates: ALL PASS" >&2
  ;;

*) fail "unknown stage: $STAGE" ;;
esac
