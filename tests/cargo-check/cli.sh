#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# Exercises the cargo worker (worker-cargo, design/cargo-workers.md): a
# whole-workspace `cargo check/test` over a source tree, `--offline` atop the
# image's baked toolchain + deps. Asserts: a passing `test` run reports its
# result as a value ({exit, stdout, stderr}); a compile error is likewise a
# VALUE (nonzero exit, diagnostics on stderr), never a run error; and an
# identical tree re-run returns the identical (cached) value.
#
# WHY THIS MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI).
# The cargo worker is not a plain script we can shell out to and diff — it is a
# std tool that only exists as a computation ON the inner stack, and the inner
# stack has exactly one driver: the tested client (run-test.sh: `caos-cli` "must
# be the one every test command goes through"). `caos-cli run` is what
# materializes the {base, tree} args, ingests DEEP-DEPS/cargo, dispatches the
# job to the inner runner, and hands back the result tree. There is no
# CLI-less path to the worker at all; running `cargo` directly here would test
# host cargo, not the worker, and would bypass everything below that the client
# is responsible for.
#
# And what we assert is not "does cargo compile" — cargo's own success is
# incidental. Each assertion pins a property of the TESTED CLIENT's round-trip
# that cargo itself has no concept of:
#   - The run returns a VALUE TREE (r*/exit, r*/stdout, r*/stderr). That framing
#     is the client+runner's, not cargo's.
#   - A compile error comes back as a value (nonzero exit + diagnostics) rather
#     than a run/job error. The value-vs-run-error distinction is precisely the
#     client/runner contract (design/cargo-workers.md); cargo just exits
#     nonzero. Only driving it through $CAOS_CLI can observe which side of that
#     line the failure lands on.
#   - An identical arg tree yields the byte-identical (cached) result. That is
#     the client's content-addressed memoization round-trip — invisible without
#     the client, meaningless to cargo.
# So the CLI is essential, not incidental: it is both the sole way to reach the
# subject and the thing whose behaviour these assertions pin down.
#
# The mini projects here have no dependencies, so they exercise the worker's
# materialize-and-run path without touching the baked caos deps; the full
# dogfood (cargo check of the caos workspace itself) is tests/cargo-self.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; } # epoch milliseconds

# Every test runs in the Linux stack, so build musl (statics run there) — the
# system's one target. No host build has a consumer.
tgt="$(uname -m)-unknown-linux-musl"

echo "== cargo test: a passing package ==" >&2
t0=$(ms)
"$CAOS_CLI" run r1 --base:@=DEEP-DEPS/cargo --tree:@=test/mini --cmd=test "--target=$tgt"
t1=$(ms)
[ "$(cat r1/exit)" = "0" ] || fail "test: exit $(cat r1/exit); stderr: $(cat r1/stderr)"
grep -q "test result: ok. 1 passed" r1/stdout \
  || fail "no passing test output: $(cat r1/stdout)"
echo "  ok: tests ran and passed ($((t1 - t0))ms)" >&2

echo "== cargo check: a compile error is a value, not a run error ==" >&2
"$CAOS_CLI" run r2 --base:@=DEEP-DEPS/cargo --tree:@=test/broken --cmd=check "--target=$tgt"
[ "$(cat r2/exit)" != "0" ] || fail "broken check exited 0"
grep -q "mismatched types" r2/stderr || fail "no diagnostics: $(cat r2/stderr)"
echo "  ok: diagnostics surfaced, exit $(cat r2/exit)" >&2

echo "== identical tree: the cached value comes back ==" >&2
t2=$(ms)
"$CAOS_CLI" run r3 --base:@=DEEP-DEPS/cargo --tree:@=test/mini --cmd=test "--target=$tgt"
t3=$(ms)
cmp -s r1/exit r3/exit && cmp -s r1/stdout r3/stdout \
  || fail "re-run of an identical tree differed"
echo "  ok: identical result (first $((t1 - t0))ms, cached $((t3 - t2))ms)" >&2
