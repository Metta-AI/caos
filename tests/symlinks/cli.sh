#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# The assertions here are about what a *worker* sees in a real /cas (symlink
# materialization), so they live in check.sh and run inside a bash worker; this
# script just launches it.
#
# A STAGED TEST: check.sh is launched with `stage_next`, not `$CAOS_CLI run`, so
# this container exits rather than parking on the job. check.sh's exit status
# fails the run, and the harness turns that unexpected failure into a FAIL.
set -euo pipefail

case "$STAGE" in

start)
  img=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/check.sh --test:@=test)
  stage_next checked "$img"
  ;;

checked)
  echo "symlinks: ALL PASS" >&2
  ;;

*) echo "FAIL: unknown stage: $STAGE" >&2; exit 1 ;;
esac
