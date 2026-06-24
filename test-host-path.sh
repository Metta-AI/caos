#!/usr/bin/env bash
# Integration test: passing a host filesystem path straight to `caos-cli run`.
#
# caos-cli ingests the path's content via git — reusing git's recorded object for
# a clean, tracked path (no read), and hashing only the changed files of a dirty
# one (via a throwaway index). The worker (hello) receives that content under
# /cas/args and echoes it into its result, so we check both the delivered content
# and the hash caos chose for the arg.
#
# Requires the dev daemons running (`tilt up`): the caos server :9090, redis,
# registry — and a docker the server can reach.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

echo "building caos client + loading hello image..." >&2
nix build .#caos -o result-caos
nix run .#load-caos-worker-hello >/dev/null
caosbin=$PROJECT/result-caos/bin/caos-cli
export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}

# A per-run salt threads into every request, so this run is independent of any
# other without clearing Redis.
export CAOS_SALT="${CAOS_SALT:-$(date +%s%N)-$$}"

# A client working repo with the server as its `caos` remote; `caos` runs the CLI
# from inside it so its git transport finds it. The fixture lives *inside* this
# repo, so path ingestion can reuse git's objects.
CLIENT=$PROJECT/.caos-dev/host-path-client
rm -rf "$CLIENT"; git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "$CAOS_SERVER_URL"
caos-cli() { ( cd "$CLIENT" && "$caosbin" "$@" ); }

CAS=$PROJECT/.caos-dev/host-path-cas
rm -rf "$CAS"; mkdir -p "$CAS"
export CAOS_CAS_DIR=$CAS
trap 'rm -rf "$CAS" "$CLIENT"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# The hash caos records for `--data:@=data`, recovered from a `curry` node (curry
# runs the same arg-ingestion as run, but leaves an inspectable object locally).
arg_hash() {
  local c args
  c=$(caos-cli curry docker://unused -- --data:@=data)
  args=$(git -C "$CLIENT" ls-tree "$c" args | awk '{print $3}')
  git -C "$CLIENT" ls-tree "$args" data | awk '{print $3}'
}

# Fixture: a tracked directory `data/` inside the client repo.
mkdir -p "$CLIENT/data/sub"
echo one   > "$CLIENT/data/a.txt"
echo two   > "$CLIENT/data/b.txt"
echo three > "$CLIENT/data/sub/c.txt"
git -C "$CLIENT" add data
git -C "$CLIENT" -c user.email=t -c user.name=t commit -qm fixture

echo "== Phase A: clean tracked dir — content delivered, git's tree reused ==" >&2
caos-cli run docker://caos-worker-hello:latest "$CAS/out" -- --data:@=data >/dev/null
caos-cli get -r "$CAS/out" >/dev/null
grep -q "saw data" "$CAS/out/receipt" || fail "worker didn't see the data arg"
diff -r "$CLIENT/data" "$CAS/out/data" >/dev/null \
  || fail "delivered content doesn't match the host dir"
want=$(git -C "$CLIENT" rev-parse HEAD:data)
[ "$(arg_hash)" = "$want" ] \
  || fail "clean dir should reuse git's tree $want, got $(arg_hash)"
echo "  ok: content delivered; arg tree reused git's $want (no re-hash)" >&2

echo "== Phase B: dirty dir — change delivered, hashed incrementally ==" >&2
echo CHANGED > "$CLIENT/data/a.txt"   # modify a tracked file (uncommitted)
echo four    > "$CLIENT/data/d.txt"   # add an untracked file
rm -rf "$CAS/out"
caos-cli run docker://caos-worker-hello:latest "$CAS/out" -- --data:@=data >/dev/null
caos-cli get -r "$CAS/out" >/dev/null
diff -r "$CLIENT/data" "$CAS/out/data" >/dev/null \
  || fail "delivered content doesn't match the dirty host dir"
got=$(arg_hash)
[ "$got" != "$want" ] || fail "dirty dir should differ from the committed tree"
# It must equal exactly what git itself computes for the current (dirty) dir.
idx=$(mktemp); cp "$CLIENT/.git/index" "$idx"
GIT_INDEX_FILE=$idx git -C "$CLIENT" add data
exp=$(GIT_INDEX_FILE=$idx git -C "$CLIENT" write-tree --prefix=data/); rm -f "$idx"
[ "$got" = "$exp" ] || fail "dirty tree $got != git's $exp"
echo "  ok: change delivered; arg tree matches git's incremental $exp" >&2

echo "ALL PASS" >&2
