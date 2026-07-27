#!/usr/bin/env bash
#@doc Build the tree's images from the nix-built binaries and run the whole
#@doc test suite — the unit tests and every tests/<name> integration suite —
#@doc as cached jobs: an unchanged test never re-runs. Returns the report;
#@doc each test's complete record (full output, inner-stack logs) rides in
#@doc the result tree. The deploy (caosd up) publishes the nix-built
#@doc binaries as refs/caos/bins; run-tool passes the hash as --bins.
#
# The test worker: the workspace tree in (--in) and the published bin
# tree (--bins, a hash), the suite result out
# ({report, results/<test>/...}). A thin
# tail call into the suite worker (tests/lib/suite.sh) carried BY that same
# tree — so the suite that runs is the one the tree defines, and its first
# act is running the build worker (caos-tools/build.sh), sharing every
# image job with `build` calls. Optional args pass through: --api_key
# (chat-online's real turn), --only (a test-name filter).
set -euo pipefail

if [ ! -e /cas/args/bins ]; then
  echo "test: no --bins. The deploy publishes the nix-built binaries as" >&2
  echo "test: refs/caos/bins (caosd up); run-tool passes the hash along." >&2
  exit 1
fi
caos get /cas/args/bins
caos get /cas/args/in
caos get /cas/args/in/tests
caos get /cas/args/in/tests/lib

extra=()
[ -e /cas/args/api_key ] && extra+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && extra+=("--only:@=/cas/args/only")
suite=$(caos curry /cas/std/bash -- \
  "--worker1:@=/cas/args/in/tests/lib/suite.sh" \
  "--workspace:@=/cas/args/in" "--bins=$(cat /cas/args/bins)" "${extra[@]}")
caos run-then /cas/args/in -- --run="$suite"
