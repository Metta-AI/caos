#!/usr/bin/env bash
# Manual phase-4 validation: run the inner socket-delegation stack inside a
# hand-launched worker container (no outer caos stack), proving the inner
# runnerd launches sibling image workers via the engine socket and they report
# back to the inner server over the shared netns.
#
# This is a SPIKE reproducer, not a suite test: it hard-codes host nix-store
# out-links. Prerequisites (build the inner-stack binaries as /tmp/p4-* links):
#   nix build .#server        -o /tmp/p4-server
#   nix build .#runnerd       -o /tmp/p4-runnerd
#   nix build .#caos          -o /tmp/p4-caos
#   nix build .#worker-runner -o /tmp/p4-wrunner
#   nix build .#worker-rgrep  -o /tmp/p4-rgrep2
# Then: bash tests/socket-in-caos/manual-spike.sh  (expect a final "PASS").
# The suite version (tests/socket-in-caos/cli.sh + inner-socket.sh) drives the
# same path as a real caos job once the testenv image carries a podman client.
set -euo pipefail
say(){ echo "[p4-manual] $*" >&2; }

PODMAN=$(readlink -f "$(command -v podman)")
DOCKER=/nix/store/pm3lk3zx4cndraxhw5r4k2fn19sc333g-docker/bin/docker
BINS=/tmp/p4-bins
SOCK=/tmp/p4-podman.sock
RUNNER_IMAGE=localhost/caos-runner:phase4
WORKER=p4worker

cleanup(){ $DOCKER rm -f "$WORKER" 2>/dev/null || true; kill "${SVC:-0}" 2>/dev/null || true; rm -f "$SOCK"; }
trap cleanup EXIT

say "stage inner binaries -> $BINS"
rm -rf "$BINS"; mkdir -p "$BINS"
cp -L /tmp/p4-server/bin/server /tmp/p4-runnerd/bin/runnerd \
      /tmp/p4-caos/bin/caos /tmp/p4-caos/bin/caos-cli \
      /tmp/p4-wrunner/bin/worker-runner /tmp/p4-rgrep2/bin/worker-rgrep "$BINS/"
chmod +x "$BINS"/*

say "build self-contained runner image -> $RUNNER_IMAGE (static bins on debian base)"
CTX=/tmp/p4-imgctx; rm -rf "$CTX"; mkdir -p "$CTX"
cp -L "$BINS/caos" "$CTX/caos"
cp -L "$BINS/worker-runner" "$CTX/worker"
cat > "$CTX/Containerfile" <<EOF
FROM debian:stable-slim
COPY caos /bin/caos
COPY worker /worker
# caos must be setuid-root: the worker drops to uid 1000 and reaches the
# root-owned /cas (incl. its user xattrs) only through this setuid binary.
RUN chmod 4755 /bin/caos && chmod 0755 /worker
EOF
$DOCKER build -t "$RUNNER_IMAGE" "$CTX" >/tmp/p4-imgbuild.log 2>&1 \
  || { tail -20 /tmp/p4-imgbuild.log >&2; exit 1; }
say "  built"

say "start rootless podman API service -> $SOCK"
rm -f "$SOCK"
"$PODMAN" system service --time=0 "unix://$SOCK" >/tmp/p4-podsvc.log 2>&1 &
SVC=$!
for _ in $(seq 1 20); do [ -S "$SOCK" ] && break; sleep 0.3; done
[ -S "$SOCK" ] || { say "socket never appeared"; cat /tmp/p4-podsvc.log >&2; exit 1; }

say "launch worker container $WORKER (socket + /nix/store + binaries + script)"
$DOCKER rm -f "$WORKER" 2>/dev/null || true
$DOCKER run -d --name "$WORKER" \
  -v "$SOCK":/run/caos/engine.sock \
  -v /nix/store:/nix/store:ro \
  -v "$BINS":/pt:ro \
  -v "$PODMAN":/usr/local/bin/podman:ro \
  -v "$(dirname "$0")/manual-inner.sh":/inner.sh:ro \
  -e CAOS_ENGINE_SOCKET=/run/caos/engine.sock \
  -e CAOS_PHASE4_RUNNER_IMAGE="$RUNNER_IMAGE" \
  -e P4_GIT_PATH="$(dirname "$(readlink -f "$(command -v git)")")" \
  docker.io/library/debian:stable-slim sleep 300 >/dev/null

say "run inner stack"
if $DOCKER exec "$WORKER" bash /inner.sh; then
  say "PASS"
else
  say "FAIL — dumping logs"
  $DOCKER exec "$WORKER" sh -c 'echo ===server===; tail -30 /tmp/server.log; echo ===runnerd===; tail -30 /tmp/runnerd.log' >&2 || true
  exit 1
fi
