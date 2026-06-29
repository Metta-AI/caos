#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by tests/run.sh). The test directory is at
# /cas/args/test and builtins are at /cas/std/<name>, all in a real /cas.
#
# Proves the rustc builder loop: a Rust source file -> the builder compiles it
# (static musl, linking the vendored worker-common) and emits a git-docker worker
# image -> that image runs as an ordinary worker. Then it edits the source and
# rebuilds to confirm a distinct, independently-working worker.
#
# It also times each phase: `build` (compile a source into a git-docker worker
# image) and `first-run` (that new image's first execution). first-run is a cold
# provision when the image hasn't been built before — so against a FRESH stack
# these numbers measure cold start; on a warm server the image is already
# provisioned and first-run reflects a warm dispatch instead.
set -euo pipefail
T=/cas/args/test
caos get -r "$T"   # make the fixture sources readable and referenceable

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }   # epoch milliseconds

# Salt each source with the per-run marker so every run compiles a NOVEL worker
# image — then `first-run` is always a genuine cold provision, never a cache hit
# from a previous run. CAOS_SALT is unique per `tests/run.sh` invocation; injected
# into the greeting string (not a comment — comments are stripped, leaving the
# binary, hence the image, identical). The asserted substrings still match.
uniq=$(printf '%s' "${CAOS_SALT:-$(date +%s%N)}" | tr -cd '0-9a-zA-Z')
# Bash builtins only (the bash worker has no `sed`): read each source, replace the
# greeting string with a salted one, write it out, then `caos put` it into the CAS
# — inside a worker, `--src:@=` reads only /cas paths, not host paths.
greeter=$(<"$T/greeter.rs")
edited=$(<"$T/greeter-edited.rs")
printf '%s\n' "${greeter//source-built worker/source-built worker $uniq}" >/tmp/g1.rs
printf '%s\n' "${edited//different greeting entirely/different greeting entirely $uniq}" >/tmp/g2.rs
caos put /tmp/g1.rs /cas/g1
caos put /tmp/g2.rs /cas/g2

# Curry the worker-base into the rustc builder so each build call only passes src.
builder=$(caos curry /cas/std/rustc -- --base:@=/cas/std/base)

echo "build greeter.rs -> worker image -> run" >&2
t0=$(ms); caos run "$builder" /cas/img -- --src:@=/cas/g1; t1=$(ms)
caos run /cas/img /cas/a --; t2=$(ms)
caos get -r /cas/a
grep -q "source-built worker" /cas/a/greeting \
  || fail "built worker did not produce the expected output"

echo "edit source -> a distinct worker" >&2
t3=$(ms); caos run "$builder" /cas/img2 -- --src:@=/cas/g2; t4=$(ms)
caos run /cas/img2 /cas/c --; t5=$(ms)
caos get -r /cas/c
grep -q "different greeting" /cas/c/greeting \
  || fail "edited worker did not produce the new output"
grep -q "different greeting" /cas/a/greeting \
  && fail "the new output leaked into the original worker's result"

# Return the timings as the worker's result: a single file, which tests/run.sh
# prints to stdout. (A worker's stderr only reaches the host on failure, so the
# result file is how a passing test reports numbers.)
{
  echo "rust-worker perf (ms):"
  echo "  greeter  build=$((t1 - t0))  first-run=$((t2 - t1))"
  echo "  edited   build=$((t4 - t3))  first-run=$((t5 - t4))"
} >/tmp/result
caos put /tmp/result /cas/out
echo "rust-worker: ALL PASS" >&2
