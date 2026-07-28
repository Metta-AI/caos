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
# pass through: --api_key (chat-online's real turn), --only (a test-name
# filter).
set -euo pipefail

caos get /cas/args/in
caos get /cas/args/in/tests
caos get /cas/args/in/tests/lib

extra=()
[ -e /cas/args/api_key ] && extra+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && extra+=("--only:@=/cas/args/only")
suite=$(caos curry /cas/std/bash -- \
  "--worker1:@=/cas/args/in/tests/lib/suite.sh" \
  "--workspace:@=/cas/args/in" "${extra[@]}")
caos run-then /cas/args/in -- --run="$suite"
