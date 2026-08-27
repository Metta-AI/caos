#!/bin/bash
# tests/dirs-only — a WORKER test: no client, no repo.
#
# Exercises the dirs-only worker, a filter that keeps only a node's directory
# children and drops its files. The fixture tree/ holds 6 files across nested
# dirs plus two files at the top: running dirs-only over tree/ must yield a
# tree of just its directory children (dirA, dirB), with their subtrees intact.
# (Worker-to-worker composition is covered by tests/run-then and
# tests/rust-worker.)
#
# The worker under test is a TEST FIXTURE, not a std entry: this test carries
# its source (./worker.rs) and builds it with rustc — memoized, so the compile
# happens once per source edit, not per run.
#
# THREE STAGES: neither the build nor the filter can be waited on, so each
# assertion is the `then` of the run it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
# EVERYTHING A LATER STAGE READS IS FORWARDED: `/cas/args/base` is the reserved
# base entry — the bash image — not this job's ArgTree, so currying onto it
# carries none of what the expression bound. --test-salt rides in every stage
# for the same reason.
next() { caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 \
  --stage="$1" --test-salt:@=/cas/args/test-salt --tree:@=/cas/args/tree; }

case "$stage" in

start)
  echo "== build the fixture worker from its source ==" >&2
  # No --runner: rustc DEPENDS on the runner pool (std/rustc/DEPS) and curries
  # the built binary onto it itself, so a caller says only what it is building.
  # `--src` is CURRIED because run-then binds its subject to `--in`, and rustc
  # reads `--src`.
  img=$(caos curry --base:@=/cas/args/rustc --src:@=/cas/args/src) \
    || fail "currying the build"
  caos run-then /cas/args/src --run:hash="$img" --then:hash="$(next filter)"
  ;;

filter)
  echo "== dirs-only keeps directories, drops files ==" >&2
  # --result is the built worker image; run it over the fixture.
  caos run-then /cas/args/tree --run:hash="$(caos hash /cas/args/result)" \
    --then:hash="$(next check)"
  ;;

check)
  R=/cas/args/result
  caos get -r "$R" || fail "reading the filtered tree"
  ls -la "$R" >&2

  [ -d "$R/dirA" ] || fail "dirA (a directory) was dropped"
  [ -d "$R/dirB" ] || fail "dirB (a directory) was dropped"
  [ ! -e "$R/top1.txt" ] || fail "top1.txt (a file) was kept"
  [ ! -e "$R/top2.txt" ] || fail "top2.txt (a file) was kept"
  # A kept directory keeps its original subtree (the contents, not just the
  # name); dirs-only filters one level.
  [ -f "$R/dirA/a1.txt" ] || fail "dirA lost its contents"
  [ -f "$R/dirB/subdir/s1.txt" ] || fail "dirB lost its nested contents"
  echo "  ok: only dirA, dirB survived, with subtrees intact" >&2

  echo "== the filtered tree holds only files under kept dirs ==" >&2
  d=$(find "$R" -type f | wc -l)
  # dirA has 2 files, dirB has 2 (one nested) — the top-level files are gone
  # (the fixture holds 6 files total).
  [ "$d" = "4" ] || fail "expected 4 files under kept dirs, got: $d"
  echo "  ok: filtered tree holds 4 files" >&2

  printf 'dirs-only: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
