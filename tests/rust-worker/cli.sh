#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set — normally INSIDE a testenv worker, as the suite's per-test job
# (tests/lib/run-nested.sh); tests/run.sh runs it on the host against the
# outer stack for interactive debugging.
#
# Proves the rustc builder loop: a Rust source file -> the builder compiles it
# (glibc/gnu, linking the vendored worker-common) and emits a ready-to-run worker
# = curry(runner, bin=<compiled binary>) -> that runs as an ordinary worker in the
# shared runner. Then it edits the source and rebuilds to confirm a distinct,
# independently-working worker.
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
# from a previous run. CAOS_SALT is unique per `tests/run.sh` invocation; injected
# into the greeting string (not a comment — comments are stripped, leaving the
# binary identical). The asserted substrings still match.
uniq=$(printf '%s' "${CAOS_SALT:-$(date +%s%N)}" | tr -cd '0-9a-zA-Z')
greeter=$(<test/greeter.rs)
edited=$(<test/greeter-edited.rs)
printf '%s\n' "${greeter//source-built worker/source-built worker $uniq}" >g1.rs
printf '%s\n' "${edited//different greeting entirely/different greeting entirely $uniq}" >g2.rs
commit "salted sources"

# Curry the runner into the rustc builder so each build call only passes src; the
# builder compiles src and curries the result into this runner.
builder=$("$CAOS_CLI" curry /cas/std/rustc -- --runner:@=/cas/std/runner)

# The builder's result is a worker image (a curry node over the runner). The CLI
# checks results out as files, so re-ingest the checkout through git to get the
# image tree's hash back — content-addressed, so it round-trips exactly.
tree_hash() { # <dir> -> the git tree hash of its committed contents
  git rev-parse "HEAD:$1"
}

echo "build greeter.rs -> runnable worker -> run" >&2
t0=$(ms); "$CAOS_CLI" run "$builder" img -- --src:@=g1.rs; t1=$(ms)
commit "built image 1"
"$CAOS_CLI" run "$(tree_hash img)" a --; t2=$(ms)
grep -q "source-built worker" a/greeting \
  || fail "built worker did not produce the expected output"

echo "edit source -> a distinct worker" >&2
t3=$(ms); "$CAOS_CLI" run "$builder" img2 -- --src:@=g2.rs; t4=$(ms)
commit "built image 2"
"$CAOS_CLI" run "$(tree_hash img2)" c --; t5=$(ms)
grep -q "different greeting" c/greeting \
  || fail "edited worker did not produce the new output"
grep -q "different greeting" a/greeting \
  && fail "the new output leaked into the original worker's result"

echo "rust-worker perf (ms):" >&2
echo "  greeter  build=$((t1 - t0))  first-run=$((t2 - t1))" >&2
echo "  edited   build=$((t4 - t3))  first-run=$((t5 - t4))" >&2
echo "rust-worker: ALL PASS" >&2
