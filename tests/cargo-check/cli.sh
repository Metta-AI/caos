#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set — normally INSIDE a testenv worker, as the suite's per-test job
# (tests/lib/run-nested.sh); tests/run.sh runs it on the host against the
# outer stack for interactive debugging.
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
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; } # epoch milliseconds

echo "== cargo test: a passing package ==" >&2
t0=$(ms)
"$CAOS_CLI" run /cas/std/cargo r1 -- --tree:@=test/mini --cmd=test
t1=$(ms)
[ "$(cat r1/exit)" = "0" ] || fail "test: exit $(cat r1/exit); stderr: $(cat r1/stderr)"
grep -q "test result: ok. 1 passed" r1/stdout \
  || fail "no passing test output: $(cat r1/stdout)"
echo "  ok: tests ran and passed ($((t1 - t0))ms)" >&2

echo "== cargo check: a compile error is a value, not a run error ==" >&2
"$CAOS_CLI" run /cas/std/cargo r2 -- --tree:@=test/broken --cmd=check
[ "$(cat r2/exit)" != "0" ] || fail "broken check exited 0"
grep -q "mismatched types" r2/stderr || fail "no diagnostics: $(cat r2/stderr)"
echo "  ok: diagnostics surfaced, exit $(cat r2/exit)" >&2

echo "== identical tree: the cached value comes back ==" >&2
t2=$(ms)
"$CAOS_CLI" run /cas/std/cargo r3 -- --tree:@=test/mini --cmd=test
t3=$(ms)
cmp -s r1/exit r3/exit && cmp -s r1/stdout r3/stdout \
  || fail "re-run of an identical tree differed"
echo "  ok: identical result (first $((t1 - t0))ms, cached $((t3 - t2))ms)" >&2
