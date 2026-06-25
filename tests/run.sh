#!/usr/bin/env bash
# Run a caos integration test that lives in a directory.
#
# All boilerplate lives here: build the CLI, publish the std library, set up a
# throwaway client repo + CAS. The test itself is <dir>/test.sh, which runs
# *inside* a bash worker (via /cas/std/bash) with the whole test directory
# available at /cas/args/test. So a test does real caos work — running other
# workers, put/get, curry — in a real /cas, and makes its own assertions (the
# bash worker carries coreutils/diff/grep). No host-side log scraping: a failing
# assertion exits test.sh non-zero, which fails the run and surfaces its stderr.
#
# Each test reaches builtins as /cas/std/<name>; <dir>/builtins (optional) lists
# which to publish, one or more per line (default: all of them).
#
# Usage: tests/run.sh <test-dir>
# Requires the dev daemons running (`tilt up` / `caosd`): the caos server :9090,
# redis, registry — and a docker the server can reach.
set -euo pipefail
[ $# -eq 1 ] || { echo "usage: $0 <test-dir>" >&2; exit 2; }
DIR=$(cd "$1" && pwd)
PROJECT=$(cd "$(dirname "$0")/.." && pwd)
cd "$PROJECT"
[ -f "$DIR/test.sh" ] || { echo "no test.sh in $DIR" >&2; exit 2; }

echo "building caos client..." >&2
nix build .#caos -o result-caos
caosbin=$PROJECT/result-caos/bin/caos-cli
export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}

# A per-run salt threads into every request (hence every cache key), so this run
# is independent of any other without ever clearing Redis.
export CAOS_SALT="${CAOS_SALT:-$(date +%s%N)-$$}"

# Publish the builtins this test needs (its `builtins` file, or all of them) so it
# can reach them as /cas/std/<name>.
builtins=()
[ -f "$DIR/builtins" ] && read -ra builtins <<<"$(tr '\n' ' ' <"$DIR/builtins")"
echo "publishing std: ${builtins[*]:-<all>}..." >&2
./build-builtins.sh "${builtins[@]}" >/dev/null

# A throwaway client repo with the server as its `caos` remote (the host CLI
# pushes the run request from here) and a CAS for the materialized result.
CLIENT=$PROJECT/.caos-dev/test-client
rm -rf "$CLIENT"; git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "$CAOS_SERVER_URL"
CAS=$PROJECT/.caos-dev/test-cas
rm -rf "$CAS"; mkdir -p "$CAS"
export CAOS_CAS_DIR=$CAS
trap 'rm -rf "$CLIENT" "$CAS"' EXIT

# Run the test inside a bash worker: test.sh is the script; the whole directory
# rides along as `test` (materialized at /cas/args/test).
echo "running $1 inside a bash worker..." >&2
( cd "$CLIENT" && "$caosbin" run /cas/std/bash "$CAS/out" -- \
    --script:@="$DIR/test.sh" --test:@="$DIR" ) >/dev/null
echo "PASS: $1" >&2
