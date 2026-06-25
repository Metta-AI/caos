#!/usr/bin/env bash
# Integration test for the `bash` worker (reached as /cas/std/bash): a worker that
# runs a shell script you hand it as `--script:@=file`. It proves that caos
# orchestration — staging an input, running another worker, producing a result —
# can run *inside* a worker (with a real /cas), instead of host-side.
#
# Requires the dev daemons running (`tilt up` / `caosd`): the caos server :9090,
# redis, registry — and a docker the server can reach.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

echo "building caos client + publishing std (bash, hello)..." >&2
nix build .#caos -o result-caos
caosbin=$PROJECT/result-caos/bin/caos-cli
export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}

# A per-run salt threads into every request (hence every cache key), so this run
# is independent of any other without ever clearing Redis.
export CAOS_SALT="${CAOS_SALT:-$(date +%s%N)-$$}"

# Publish the builtins this test uses to refs/caos/std on the server, so both the
# CLI and the worker reach them as /cas/std/<name>.
./build-builtins.sh bash hello >/dev/null

# A client working repo with the server as its `caos` remote; `caos-cli` runs from
# inside it so its git transport finds it (and can fetch refs/caos/std).
CLIENT=$PROJECT/.caos-dev/bash-client
rm -rf "$CLIENT"; git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "$CAOS_SERVER_URL"
caos-cli() { ( cd "$CLIENT" && "$caosbin" "$@" ); }

CAS=$PROJECT/.caos-dev/bash-cas
rm -rf "$CAS"; mkdir -p "$CAS"
export CAOS_CAS_DIR=$CAS
SCRIPT=$(mktemp)
trap 'rm -rf "$CLIENT" "$CAS" "$SCRIPT"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# The script the bash worker runs *inside* the sandbox: stage an input with `caos
# put`, run the hello builtin over it, and leave hello's result at /cas/out (the
# worker's result). All of this is ordinary in-sandbox caos — no host involved.
# It reaches hello as /cas/std/hello, the std library the server threaded in.
cat > "$SCRIPT" <<'SH'
set -euo pipefail
mkdir -p /tmp/in
echo "from inside the bash worker" > /tmp/in/note
caos put /tmp/in/note /cas/note
caos run /cas/std/hello /cas/out -- --note:@=/cas/note
SH

echo "== run a script inside the bash worker ==" >&2
caos-cli run /cas/std/bash "$CAS/out" -- --script:@="$SCRIPT" >/dev/null
# caos-cli run materializes the result, so $CAS/out is hello's result tree.
grep -q "saw note" "$CAS/out/receipt" \
  || fail "hello (run from inside the bash worker) didn't see the note arg"
grep -q "from inside the bash worker" "$CAS/out/note" \
  || fail "content staged + delivered inside the worker doesn't match"
echo "  ok: bash worker ran the script, which ran hello via /cas/std" >&2

echo "ALL PASS" >&2
