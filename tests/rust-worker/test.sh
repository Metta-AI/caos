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

# Curry the worker-base into the rustc builder so each build call only passes src.
builder=$(caos curry /cas/std/rustc -- --base:@=/cas/std/base)

echo "build greeter.rs -> worker image -> run" >&2
t0=$(ms); caos run "$builder" /cas/img -- --src:@="$T/greeter.rs"; t1=$(ms)
caos run /cas/img /cas/a --; t2=$(ms)
caos get -r /cas/a
grep -q "source-built worker" /cas/a/greeting \
  || fail "built worker did not produce the expected output"

echo "edit source -> a distinct worker" >&2
t3=$(ms); caos run "$builder" /cas/img2 -- --src:@="$T/greeter-edited.rs"; t4=$(ms)
caos run /cas/img2 /cas/c --; t5=$(ms)
caos get -r /cas/c
grep -q "different greeting" /cas/c/greeting \
  || fail "edited worker did not produce the new output"
grep -q "different greeting" /cas/a/greeting \
  && fail "the new output leaked into the original worker's result"

echo "rust-worker perf: greeter build=$((t1 - t0))ms first-run=$((t2 - t1))ms;" \
     "edited build=$((t4 - t3))ms first-run=$((t5 - t4))ms" >&2
echo "rust-worker: ALL PASS" >&2
