#!/bin/bash
# tests/file-count — an ordinary caos worker, run by evaluating this directory's
# `.caos-expr` (dev/run-test does the evaluating; that is all it does).
#
# Exercises the file-count worker: a file counts as 1, a tree recurses over its
# children through server-resolved map-then continuations (with itself on both
# sides) and sums the counts — so it totals a tree's leaf files, exercising the
# promise pipeline end to end. The fixture tree/ holds 5 files across nested
# dirs.
#
# The worker under test is a TEST FIXTURE, not a std entry: this test carries
# its source (./worker.rs) and builds it with rustc — memoized, so the compile
# happens once per source edit, not per run.
#
# FOUR STAGES, one script, selected by a curried --stage — the shape every
# staged worker in this tree uses. Nothing here blocks: a stage records a
# promise and exits, so no container is ever parked waiting on a job that needs
# a container of its own.
#
# `prepare-request` + `run-request-then`, not `run-then`: prepare-request forms
# the very ArgTree a `run` would, and run-request-then runs one unchanged — "no
# `--in` is added and no new request is assembled around an image". `run-then`
# would bind its subject as `--in` whether or not the callee wants one, which
# several workers read in preference to their real argument. `curry` is for
# building the NEXT STAGE'S IMAGE, which is what a curry node is; forming a run
# request with it produces an ArgTree the runner does not unwrap, and the
# symptom is the runner dying on `caos get /cas/args/worker1` — its own
# entrypoint arg, missing, because the chain below it was never merged.
#
# NO `--catch` ANYWHERE. A failing sub-run should fail this test, and
# dev/run-test already catches at the top: the failure becomes this test's FAIL
# with the failing worker's own stderr as the diagnostic, without touching any
# other test's result.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

# The next stage, curried off this job's base.
#
# `/cas/args/base` IS THE IMAGE, NOT THIS JOB'S ARG TREE. It is the reserved
# `base` entry — here the bash image the expression named — so currying onto it
# carries none of what the expression bound alongside it. Anything a later stage
# reads has to be re-bound by name, which is what `--tree` is doing below and
# what dev/run-tests' `forwarded()` does for the same reason.
#
# `--salt` rides in every stage, not just the first. It is unread; its presence
# in the ArgTree is what makes `--test-salt` re-run this test. Bound only at the
# top, a fresh salt would re-run `start` and then hit the memo for every stage
# after it — a "re-run" that re-ran one container.
next() { # <stage> [extra curry args...]
  local s=$1; shift
  caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 \
    --stage="$s" --tree:@=/cas/args/tree --salt:@=/cas/args/salt "$@"
}

case "$stage" in

start)
  echo "== build the fixture worker from its source ==" >&2
  # /cas/args/rustc is the BUILT builder: the expression evaluated
  # DEEP-DEPS/rustc on the way in. No --runner either — rustc depends on the
  # runner pool and curries the built binary onto it itself, so a caller says
  # only what it is building.
  req=$(caos prepare-request --base:@=/cas/args/rustc --src:@=/cas/args/src) \
    || fail "forming the build request"
  caos run-request-then "$req" --then:hash="$(next count-tree)"
  ;;

count-tree)
  echo "== a whole tree totals its leaf files ==" >&2
  # --result is the built worker image. Keep its oid as a literal for the
  # second count, three containers from here.
  worker=$(caos hash /cas/args/result) || fail "reading the built worker"
  req=$(caos prepare-request --base:hash="$worker" --in:@=/cas/args/tree) \
    || fail "forming the tree count"
  caos run-request-then "$req" --then:hash="$(next count-file --worker="$worker")"
  ;;

count-file)
  caos get /cas/args/result || fail "reading the count"
  n=$(cat /cas/args/result)
  [ "$n" = "5" ] || fail "expected 5 leaf files, got: $n"
  echo "  ok: tree -> 5" >&2

  echo "== a single file counts as 1 ==" >&2
  caos get /cas/args/tree || fail "expanding the fixture tree"
  caos get /cas/args/worker
  req=$(caos prepare-request --base:hash="$(cat /cas/args/worker)" --in:@=/cas/args/tree/a.txt) \
    || fail "forming the file count"
  caos run-request-then "$req" --then:hash="$(next check)"
  ;;

check)
  caos get /cas/args/result || fail "reading the count"
  n=$(cat /cas/args/result)
  [ "$n" = "1" ] || fail "expected 1, got: $n"
  echo "  ok: file -> 1" >&2

  # THE RESULT IS THE NARRATION, which is the convention dev/run-test's verdict
  # stage prints and `test-result` reads back. Written whole by the last stage
  # rather than accumulated: a staged test only GETS here by passing everything
  # before it, so the story is knowable from the fact of arriving.
  cat > /tmp/report <<'REPORT'
== build the fixture worker from its source ==
  ok: built
== a whole tree totals its leaf files ==
  ok: tree -> 5
== a single file counts as 1 ==
  ok: file -> 1
file-count: ALL PASS
REPORT
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
