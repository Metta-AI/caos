#!/bin/bash
# tests/rust-worker — a WORKER test: no client, no repo.
#
# Proves the rustc builder loop: a Rust source file -> the builder compiles it
# (glibc/gnu, linking the vendored worker-common) and emits a ready-to-run
# worker = curry(runner, bin=<compiled binary>) -> that runs as an ordinary
# worker in the shared runner. Then it edits the source and rebuilds to confirm
# a distinct, independently-working worker.
#
# THE SALT IS --test-salt, not a fresh `date +%s%N`. The point of salting the
# sources is that a NOVEL binary gets compiled, so the run after it is a genuine
# cold path rather than a memo hit. Minting a new one per run made that true
# always and made the compile un-cacheable; taking it from --test-salt makes it
# true exactly when the suite is asked to re-run, which is what the flag means.
# Empty (no --test-salt) compiles the checked-in sources, and hits.
#
# FIVE STAGES: build, run, assert, build again, run, assert — and no run can be
# waited on, so each assertion is the `then` of the run it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --rustc:@=/cas/args/rustc --greeter:@=/cas/args/greeter \
  --edited:@=/cas/args/edited "$@"; }

# Salt a source and hand it to rustc. Bash parameter expansion, not sed:
# std/bash carries no sed (CLAUDE.md), and this needs no more than a literal
# substitution.
build() { # <arg-name> <text-to-salt> -> the build ArgTree
  local src marker
  caos get "/cas/args/$1" || fail "reading $1"
  caos get /cas/args/test-salt || true
  marker=$(cat /cas/args/test-salt 2>/dev/null || true)
  src=$(cat "/cas/args/$1")
  printf '%s\n' "${src//$2/$2 $marker}" > /tmp/salted.rs
  caos put /tmp/salted.rs /cas/salted || fail "publishing the salted source"
  caos curry --base:@=/cas/args/rustc --src:@=/cas/salted
}

case "$stage" in

start)
  echo "build greeter.rs -> runnable worker -> run" >&2
  caos run-then /cas/args/greeter \
    --run:hash="$(build greeter 'source-built worker')" --then:hash="$(next run-first)"
  ;;

run-first)
  # --result is the built worker image; run it with nothing bound.
  caos run-then /cas/args/greeter --run:hash="$(caos hash /cas/args/result)" \
    --then:hash="$(next check-first)"
  ;;

check-first)
  caos get -r /cas/args/result || fail "reading the greeting"
  grep -q "source-built worker" /cas/args/result/greeting \
    || fail "built worker did not produce the expected output"
  cp /cas/args/result/greeting /tmp/first-greeting
  caos put /tmp/first-greeting /cas/first || fail "keeping the first greeting"

  echo "edit source -> a distinct worker" >&2
  caos run-then /cas/args/edited \
    --run:hash="$(build edited 'different greeting entirely')" \
    --then:hash="$(next run-second --first:@=/cas/first)"
  ;;

run-second)
  caos run-then /cas/args/edited --run:hash="$(caos hash /cas/args/result)" \
    --then:hash="$(next check-second --first:@=/cas/args/first)"
  ;;

check-second)
  caos get -r /cas/args/result || fail "reading the greeting"
  caos get /cas/args/first
  grep -q "different greeting" /cas/args/result/greeting \
    || fail "edited worker did not produce the new output"
  grep -q "different greeting" /cas/args/first \
    && fail "the new output leaked into the original worker's result"
  printf 'rust-worker: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
