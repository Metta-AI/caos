#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# Proves the rustc builder loop: a Rust source file -> the builder compiles it
# (glibc/gnu, linking the vendored worker-common) and emits a ready-to-run worker
# = curry(runner, bin=<compiled binary>) -> that runs as an ordinary worker in the
# shared runner. Then it edits the source and rebuilds to confirm a distinct,
# independently-working worker.
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI), essentially and
# not incidentally:
#
#   The subject here is NOT a script or a file on disk — it is a *server-side
#   computation* that only exists when reached through the client protocol. The
#   rustc builder is a curry over the runner pool that the client resolves,
#   dispatches, and materializes; there is no way to "compile this source into a
#   runnable worker and run it" except by driving `curry`/`run` on the tested
#   client against the inner stack. So exercising the loop at all IS exercising
#   the client end-to-end.
#
#   Every step below pins a property that lives in the client:
#     - `curry --base:@=DEEP-DEPS/rustc` makes the client INGEST the declared
#       dep tree, evaluate its .caos-expr, and produce a builder image hash —
#       the client's dependency-naming and closure walk.
#     - `run img --base:hash=$builder --src:@=g1.rs` makes the client push the
#       arg-tree closure (source blob + the builder's runner-delta objects it
#       only holds by hash), dispatch the build to the inner server, and check
#       the resulting worker image out as files — the client's push/resolve/
#       checkout round-trip against a real stack.
#     - `run a --base:hash=$(tree_hash img)` runs that freshly built worker,
#       proving the emitted curry(runner,bin=...) actually dispatches and
#       executes in the shared runner — reachable only via the client.
#     - the edit-and-rebuild half proves a changed source yields a DISTINCT,
#       independently-working worker with no cross-contamination of results:
#       content-addressed identity and cache behavior that only the client's
#       hashing/dispatch path can demonstrate.
#
#   None of this is verifiable "directly" (the std-lint way): the artifact under
#   test is produced and run by the stack, so removing the client would remove
#   the subject. The CLI is the instrument, not an incidental driver.
#
# It also times each phase: `build` (compile a source into a runnable worker) and
# `first-run` (that new worker's first execution). On a warm server the runner is
# already provisioned, so first-run reflects a warm dispatch; against a fresh stack
# it's a cold provision.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }   # epoch milliseconds
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# Salt each source with the per-run marker so every run compiles a NOVEL worker
# binary — then `first-run` is always a genuine cold path, never a cache hit
# from a previous run.
test_run_id="$(date +%s%N)-$$-$RANDOM"
uniq=$(printf '%s' "$test_run_id" | tr -cd '0-9a-zA-Z')
greeter=$(<test/greeter.rs)
edited=$(<test/greeter-edited.rs)
printf '%s\n' "${greeter//source-built worker/source-built worker $uniq}" >g1.rs
printf '%s\n' "${edited//different greeting entirely/different greeting entirely $uniq}" >g2.rs
commit "salted sources"

# Curry the runner into the rustc builder so each build call only passes src; the
# builder compiles src and curries the result into this runner.
# No --runner: rustc DEPENDS on the runner pool (std/rustc/DEPS) and curries
# the built binary onto it itself, so a caller says only what it is building.
builder=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/rustc)

# The builder's result is a worker image (a curry node over the runner). The CLI
# checks results out as files, so re-ingest the checkout through git to get the
# image tree's hash back — content-addressed, so it round-trips exactly.
tree_hash() { # <dir> -> the git tree hash of its committed contents
  git rev-parse "HEAD:$1"
}

echo "build greeter.rs -> runnable worker -> run" >&2
t0=$(ms); "$CAOS_CLI" run img --base:hash="$builder" --src:@=g1.rs; t1=$(ms)
commit "built image 1"
"$CAOS_CLI" run a --base:hash="$(tree_hash img)"; t2=$(ms)
grep -q "source-built worker" a/greeting \
  || fail "built worker did not produce the expected output"

echo "edit source -> a distinct worker" >&2
t3=$(ms); "$CAOS_CLI" run img2 --base:hash="$builder" --src:@=g2.rs; t4=$(ms)
commit "built image 2"
"$CAOS_CLI" run c --base:hash="$(tree_hash img2)"; t5=$(ms)
grep -q "different greeting" c/greeting \
  || fail "edited worker did not produce the new output"
grep -q "different greeting" a/greeting \
  && fail "the new output leaked into the original worker's result"

echo "rust-worker perf (ms):" >&2
echo "  greeter  build=$((t1 - t0))  first-run=$((t2 - t1))" >&2
echo "  edited   build=$((t4 - t3))  first-run=$((t5 - t4))" >&2
echo "rust-worker: ALL PASS" >&2
