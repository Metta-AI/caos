#!/bin/bash
# One test, bracketed by two jobs. Mapped over the per-test trees the fan-out
# assembles (dev/run-tests/worker.sh), one child per tests/<name>.
#
# TWO STAGES, one script, selected by a curried --stage — the same shape
# dev/run-tests uses, and for the same reason: what the second stage needs is
# only knowable once the first one's work has finished.
#
#   eval     (default) `--in` is the test's tree: the deepened tests/<name>
#            with this run's `cli` and `salt` staged in. Evaluating its
#            `.caos-expr` IS running the test — `eval_path` on a `run` value
#            evaluates to the run's RESULT — so this stage records an
#            eval-path-then and exits.
#   verdict  the `then` of that: --result is what the test returned, or
#            --error what it failed with.
#
# `--catch` IS LOAD-BEARING. Without it a failing test fails this job, which
# fails the map, which discards every OTHER test's result — one broken test
# would report as a suite with no results at all. Caught, a failure is a value:
# this test's FAIL, with the failing worker's own stderr as the diagnostic.
set -euo pipefail

fail() { echo "RUN-TEST FAIL: $*" >&2; exit 1; }

stage=eval
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

case "$stage" in

eval)
  # The clock for this test's whole chain, stamped here because this is the
  # first thing that runs for it. It is WALL time across every job the test
  # spawns, which is what "which test is the long pole" wants to mean — the
  # old figure was in-container seconds and missed everything the test waited
  # on.
  next=$(caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 \
    --stage=verdict "--start=$(date +%s)") || fail "currying the verdict stage"
  caos eval-path-then /cas/args/in --eval=. --then:hash="$next" --catch
  ;;

verdict)
  caos get /cas/args/start || fail "reading --start"
  mkdir -p /tmp/out
  if [ -e /cas/args/error ]; then
    caos get /cas/args/error || fail "reading --error"
    echo "RUN-TEST: FAIL" > /tmp/out/verdict
    # The error text is the failing worker's stderr, relayed by the runner —
    # so the report's excerpt is the diagnostic from wherever in the test's
    # chain it actually broke, not this job's view of it.
    cp /cas/args/error /tmp/out/output
  else
    echo "RUN-TEST: PASS" > /tmp/out/verdict
    caos get /cas/args/result || fail "reading --result"
    # A test's result is conventionally a BLOB of its own narration, which is
    # what `test-result` prints. A test that returns a tree instead has nothing
    # to inline, and says so rather than dumping a directory listing.
    if [ -f /cas/args/result ]; then
      cp /cas/args/result /tmp/out/output
    else
      echo "(this test returned a tree, not a narration blob)" > /tmp/out/output
    fi
  fi
  echo $(($(date +%s) - $(cat /cas/args/start))) > /tmp/out/seconds
  caos put /tmp/out /cas/out
  ;;

*)
  fail "unknown --stage: $stage"
  ;;
esac
