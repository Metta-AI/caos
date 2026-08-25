#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# Exercises the cargo worker (worker-cargo, design/cargo-workers.md): a
# whole-workspace `cargo check/test` over a source tree, `--offline` atop the
# image's baked toolchain + deps. Asserts: a passing `test` run reports its
# result as a value ({exit, stdout, stderr}); a compile error is likewise a
# VALUE (nonzero exit, diagnostics on stderr), never a run error; and an
# identical tree re-run returns the identical (cached) value.
#
# The mini projects here have no dependencies, so they exercise the worker's
# materialize-and-run path without touching the baked caos deps; the full
# dogfood (cargo check of the caos workspace itself) is tests/cargo-self.
#
# A STAGED TEST (dev/run-test/run-test.sh's header): each run is a `stage_next`
# tail call, so this container never parks on a cargo job. Note that NONE of
# these needs `--may-fail` — the broken package is the whole point of the second
# case and it still SUCCEEDS as a run: the worker reports the compile error as a
# value with a nonzero `exit`, which is exactly the property being asserted.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# Every test runs in the Linux stack, so build musl (statics run there) — the
# system's one target. No host build has a consumer.
tgt="$(uname -m)-unknown-linux-musl"

cargo_job() { # <cmd> -> the image; the TREE rides as the subject (--in)
  "$CAOS_CLI" curry --base:@=DEEP-DEPS/cargo --cmd="$1" "--target=$tgt"
}

case "$STAGE" in

start)
  echo "== cargo test: a passing package ==" >&2
  stage_next passed "$(cargo_job test)" test/mini
  ;;

passed)
  fetch_result
  [ "$(cat "$RESULT/exit")" = "0" ] \
    || fail "test: exit $(cat "$RESULT/exit"); stderr: $(cat "$RESULT/stderr")"
  grep -q "test result: ok. 1 passed" "$RESULT/stdout" \
    || fail "no passing test output: $(cat "$RESULT/stdout")"
  echo "  ok: tests ran and passed" >&2
  # The passing run's value, to compare the cached re-run against.
  printf '%s\n' "$RESULT_HASH" > "$CARRY_OUT/mini"

  echo "== cargo check: a compile error is a value, not a run error ==" >&2
  stage_next broken "$(cargo_job check)" test/broken
  ;;

broken)
  fetch_result
  [ "$(cat "$RESULT/exit")" != "0" ] || fail "broken check exited 0"
  grep -q "mismatched types" "$RESULT/stderr" \
    || fail "no diagnostics: $(cat "$RESULT/stderr")"
  echo "  ok: diagnostics surfaced, exit $(cat "$RESULT/exit")" >&2

  echo "== identical tree: the cached value comes back ==" >&2
  stage_next cached "$(cargo_job test)" test/mini
  ;;

cached)
  # The whole result by oid, rather than `cmp` on two checkouts: results are
  # content-addressed, so an equal hash is a stronger statement than an equal
  # exit and stdout, and it needs nothing carried but the hash.
  [ "$RESULT_HASH" = "$(cat "$CARRY/mini")" ] \
    || fail "re-run of an identical tree differed: $RESULT_HASH vs $(cat "$CARRY/mini")"
  echo "  ok: identical result" >&2
  echo "cargo-check: ALL PASS" >&2
  ;;

*) fail "unknown stage: $STAGE" ;;
esac
