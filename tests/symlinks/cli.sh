#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI):
# The subject under test IS the tested client's symlink handling — there is no
# way to verify it except by driving the client, because the property only
# exists as the client moves a tree into and out of CAS. The fixture tree/ holds
# a real file and a git symlink to it (a mode-120000 blob), and the test pins
# down two round trips that are entirely the client's behaviour:
#
#   1. INGEST + MATERIALIZE. `$CAOS_CLI run` ingests this repo (the CLI turns
#      git-tracked paths into CAS objects) and the worker's `caos get -r`
#      materializes tree/ back into a real /cas. A worker must see link.txt as a
#      genuine symlink — not a regular file holding the target path, not a
#      dereferenced copy of the contents. That "genuine symlink survives" is a
#      guarantee of the CLI's ingest+get, not of git or the filesystem.
#   2. PUT of a STAGED git symlink (the regression this test guards). Workers
#      stage a result by symlinking already-fetched /cas entries into a scratch
#      tree and `caos put`ting it — how write/edit keep untouched siblings. When
#      a staged sibling is itself a git symlink, `caos put` must reuse it AS a
#      symlink rather than following it and recording a flattened regular copy.
#      Only the tested client's `put` can be exercised for this; a check that
#      bypassed $CAOS_CLI would test nothing the client actually does.
#
# So the CLI is essential, not incidental: the assertions are about what a
# *worker* sees in a real /cas after the client has round-tripped the tree.
# They live in check.sh and run inside a bash worker (where `caos` on PATH is
# this same tested client, per run-test.sh); this script just launches it.
set -euo pipefail

"$CAOS_CLI" run --base:@=DEEP-DEPS/bash --worker1:@=test/check.sh --test:@=test
