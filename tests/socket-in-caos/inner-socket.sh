#!/usr/bin/env bash
# Runs INSIDE a testenv worker, as ROOT (the image's CAOS_WORKER_UID=0 grant).
# This is caos-in-caos with the PHASE-4 backend: the inner runnerd delegates
# its worker containers to the OUTER engine through a bind-mounted API socket
# (design/cargo-workers.md, phase 4), so they run as *siblings* of this worker
# rather than as a nested runtime — sidestepping the mount_too_revealing wall
# that blocks podman-in-podman here. Each sibling joins THIS worker's network
# namespace (`--network container:<self>`), so the inner server on 127.0.0.1 is
# reachable by them exactly as if they were local.
#
# Unlike the process backend (test-in-caos), this runs real image-based
# workers: the sibling is the runner image, and the worker binary rides in as
# the job's `bin` arg (fetched from the inner server over the shared netns).
set -euo pipefail

fail() { echo "SOCKET-IN-CAOS FAIL: $*" >&2; exit 1; }

# Materialize the inner-stack binaries (CAS materialization drops the exec bit).
caos get -r /cas/args/bins
mkdir -p /pt
cp /cas/args/bins/* /pt/
chmod +x /pt/*

# The runner injects the OUTER run's std/salt; the inner requests must not
# inherit them (see test-in-caos for the full rationale).
unset CAOS_STD CAOS_SALT

INNER=http://127.0.0.1
SOCK=${CAOS_ENGINE_SOCKET:?the outer runnerd must grant an engine socket}
# The image the siblings run: a self-contained runner image the OUTER engine
# already has. The inner server passes docker://<ref> straight through
# (resolve_image handles it before any convert), so no inner registry is
# needed for this first cut.
RUNNER_IMAGE=${CAOS_PHASE4_RUNNER_IMAGE:?runner image ref not provided}

[ -S "$SOCK" ] || fail "engine socket $SOCK is not a socket"

echo "== inner server (docker:// passthrough; private git + dead redis) =="
mkdir -p /tmp/inner-git
CAOS_GIT_DIR=/tmp/inner-git CAOS_IMAGE_RESOLVE=none CAOS_REDIS_ADDR=127.0.0.1:6399 \
  /pt/server >/tmp/server.log 2>&1 &
ok=""
for _ in $(seq 1 30); do
  if git ls-remote "$INNER" >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ -n "$ok" ] || { cat /tmp/server.log >&2; fail "inner server never came up"; }

echo "== inner runnerd (socket-delegation: siblings via the outer engine) =="
# CAOS_DOCKER_ARGS puts `--remote --url` before `run` so podman talks to the
# outer service; CAOS_DOCKER_NETWORK=container:<self> makes each sibling share
# this worker's netns, so CAOS_SERVER_URL=http://127.0.0.1 resolves for them.
SELF=$(cat /etc/hostname)
CAOS_SERVER_URL=$INNER \
  CAOS_DOCKER_BIN=podman \
  CAOS_DOCKER_ARGS="--remote --url unix://$SOCK" \
  CAOS_DOCKER_NETWORK="container:$SELF" \
  CAOS_RUNNER_SLOTS=2 \
  /pt/runnerd >/tmp/runnerd.log 2>&1 &
sleep 1
grep -q "slots, server" /tmp/runnerd.log || { cat /tmp/runnerd.log >&2; fail "runnerd did not start"; }

echo "== inner client + workload =="
mkdir -p /tmp/client && cd /tmp/client
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$INNER"
mkdir -p dummy tree/sub
echo "inner base marker" > dummy/marker
printf 'a needle here\nnothing\n' > tree/a.txt
printf 'no match at all\n' > tree/b.txt
printf 'deep needle too\n' > tree/sub/c.txt
cp /pt/worker-rgrep rgrep-bin
git add -A
git commit -qm 'inner workload'

echo "== rgrep fold through the inner stack (sibling image workers) =="
# curry the rgrep bin onto the runner IMAGE (docker://): own_image() unwraps to
# the image the sibling runs; the bin arrives as a job arg at runtime.
curried=$(CAOS_SERVER_URL=$INNER /pt/caos-cli curry "docker://$RUNNER_IMAGE" \
  -- --bin:@=rgrep-bin --pattern=needle)
if ! CAOS_SERVER_URL=$INNER /pt/caos-cli run "$curried" out -- --in:@=tree 2>/tmp/run.err; then
  cat /tmp/run.err >&2
  echo "--- server ---" >&2; tail -20 /tmp/server.log >&2
  echo "--- runnerd ---" >&2; tail -20 /tmp/runnerd.log >&2
  fail "inner rgrep run failed"
fi

grep -q '1:a needle here' out/a.txt || fail "flat match missing"
grep -q '1:deep needle too' out/sub/c.txt || fail "recursive match missing"
[ ! -e out/b.txt ] || fail "sparse result carries a matchless file"
echo "  ok: inner stack computed the fold via socket-delegated siblings"

echo "SOCKET-IN-CAOS: ALL PASS" > /tmp/verdict
caos put /tmp/verdict /cas/out
