#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# Proves git's executable bit round-trips through the worker CAS as METADATA,
# not as a placeholder permission: it is recorded on the placeholder (an xattr)
# and only becomes a real +x mode bit once the file is fetched. The assertions
# are about the modes a *worker* sees in a real /cas, so they live in check.sh
# and run inside a bash worker; this script just builds the workspace and
# launches it.
#
# A STAGED TEST: the worker is launched with `stage_next`, not `$CAOS_CLI run`,
# so this container exits instead of parking on the job. check.sh's own exit
# status is the verdict — it fails the run, the harness catches it, and the
# unexpected-failure path turns it into this test's FAIL.
set -euo pipefail

case "$STAGE" in

start)
  # An executable file and a plain one. chmod HERE (not a committed fixture
  # mode) so the exec bit is unambiguous regardless of how the harness stored
  # the tree.
  mkdir -p ws
  printf '#!/bin/sh\necho hi\n' > ws/run.sh
  chmod +x ws/run.sh
  echo plain > ws/plain.txt
  git add -A && git -c user.email=test@caos -c user.name=caos commit -qm exec-bit-ws

  img=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/check.sh --ws:@=ws)
  stage_next checked "$img"
  ;;

checked)
  echo "exec-bit: ALL PASS" >&2
  ;;

*) echo "FAIL: unknown stage: $STAGE" >&2; exit 1 ;;
esac
