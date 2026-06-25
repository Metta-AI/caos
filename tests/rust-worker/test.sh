#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by tests/run.sh). The test directory is at
# /cas/args/test and builtins are at /cas/std/<name>, all in a real /cas.
#
# Proves the rustc builder loop: a Rust source file -> the builder compiles it
# (static musl, linking the vendored worker-common) and emits a git-docker worker
# image -> that image runs as an ordinary worker. Then it edits the source and
# rebuilds to confirm a distinct, independently-working worker.
set -euo pipefail
T=/cas/args/test
caos get -r "$T"   # make the fixture sources readable and referenceable

fail() { echo "FAIL: $*" >&2; exit 1; }

# Curry the worker-base into the rustc builder so each build call only passes src.
builder=$(caos curry /cas/std/rustc -- --base:@=/cas/std/base)

echo "build greeter.rs -> worker image -> run" >&2
caos run "$builder" /cas/img -- --src:@="$T/greeter.rs"
caos run /cas/img /cas/a --
caos get -r /cas/a
grep -q "source-built worker" /cas/a/greeting \
  || fail "built worker did not produce the expected output"

echo "edit source -> a distinct worker" >&2
caos run "$builder" /cas/img2 -- --src:@="$T/greeter-edited.rs"
caos run /cas/img2 /cas/c --
caos get -r /cas/c
grep -q "different greeting" /cas/c/greeting \
  || fail "edited worker did not produce the new output"
grep -q "different greeting" /cas/a/greeting \
  && fail "the new output leaked into the original worker's result"

echo "rust-worker: ALL PASS" >&2
