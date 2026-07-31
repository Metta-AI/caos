#!/usr/bin/env bash
#@doc Build the test stack image from the tree and run the whole test suite —
#@doc the unit tests and every tests/<name> integration suite — as cached
#@doc jobs: an unchanged test never re-runs. Returns the report; each test's
#@doc complete record (full output, inner-stack logs) rides in the result
#@doc tree. Nothing is handed in from the host: the stack under test is
#@doc compiled from these sources, inside workers.
#
# The test worker: the workspace tree in (--in), the suite result out
# ({report, results/<test>/...}). A thin tail call into the suite worker
# (tests/lib/suite.sh) carried BY that same tree — so the suite that runs is
# the one the tree defines, and its first act is running the build worker
# (caos-tools/build.sh), sharing its job with `build` calls. Optional args
# pass through: --api-key (chat-online's real turn), --only (a test-name
# filter), --test-salt (re-run every test, reusing the build — see below).
set -euo pipefail

caos get /cas/args/in
caos get /cas/args/in/tests
caos get /cas/args/in/tests/lib

extra=()
[ -e /cas/args/api-key ] && extra+=("--api-key:@=/cas/args/api-key")
[ -e /cas/args/only ] && extra+=("--only:@=/cas/args/only")
# --test-salt: re-run the TESTS without re-running anything else. CAOS_SALT
# cannot do this — it threads into every sub-run, so it re-keys the reduce, the
# compile, the std publish and the image alongside the tests (measured: 47s
# against 35s), and fills the cache with entries nothing will hit again. This
# one rides only in each per-test wrapper, so the build stays a cache hit.
#
# It exists because the alternative people reach for is editing a tracked file
# to bust the key — which is how `# rekey <timestamp>` once ended up committed
# to tests/lib/run-test.sh.
if [ -e /cas/args/test-salt ]; then extra+=("--test-salt:@=/cas/args/test-salt"); fi
suite=$(caos curry /cas/std/bash -- \
  "--worker1:@=/cas/args/in/tests/lib/suite.sh" \
  "--workspace:@=/cas/args/in" "${extra[@]}")
caos run-then /cas/args/in -- --run="$suite"
