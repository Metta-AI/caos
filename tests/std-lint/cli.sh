#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# The literal-tree lints (design/flake-images.md, part 2): std's checked-in
# redundancies must match what std/refresh.sh regenerates from their sources
# of truth — each std flake.lock re-derives byte-identically from the root
# flake.lock, std/testenv/worker equals std/bash/worker, and std/cargo's
# vendored manifests/lockfile/toolchain/target stubs match the workspace.
# The check IS the generator run in --check mode (one script, one code
# path), so the lint cannot drift from what a refresh would write. The
# workspace arrives in this test's wrapper, staged as $CAOS_PROJECT
# (suite-stage3.sh). Fast: no compiles, no caos jobs.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== std/refresh.sh --check: every checked-in std copy re-derives ==" >&2
bash "$CAOS_PROJECT/std/refresh.sh" --check || fail "checked-in std copies are stale"

echo "std-lint: ALL PASS" >&2
