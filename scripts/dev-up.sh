#!/usr/bin/env bash
#
# dev-up.sh — load the caos images, start the object + compute servers against a
# repo, and print a one-shot bash-client command.
#
# Usage:
#   scripts/dev-up.sh <repo-path>
#
# Env overrides:
#   CAOS_NET           docker network name             (default: caos-net)
#   CAOS_PORT          host port for the object server  (default: 8080)
#   CAOS_COMPUTE_PORT  host port for the compute server (default: 9090)

set -euo pipefail

NET="${CAOS_NET:-caos-net}"
PORT="${CAOS_PORT:-8080}"
COMPUTE_PORT="${CAOS_COMPUTE_PORT:-9090}"
SERVER_NAME="caos-object-server"
COMPUTE_NAME="caos-compute-server"

# --- args -------------------------------------------------------------------

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <repo-path>" >&2
  exit 2
fi

REPO="$(cd "$1" 2>/dev/null && pwd)" || {
  echo "error: repo path not found: $1" >&2
  exit 1
}

if ! git -C "$REPO" rev-parse --git-dir >/dev/null 2>&1; then
  echo "error: $REPO does not look like a git repository" >&2
  exit 1
fi

# The flake lives at the repo root, one level up from this script.
FLAKE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --- load images ------------------------------------------------------------

echo "==> Loading images from $FLAKE"
nix run "$FLAKE#load-caos-object-server"
nix run "$FLAKE#load-caos-client"
nix run "$FLAKE#load-caos-client-bash"
nix run "$FLAKE#load-caos-compute-server"
nix run "$FLAKE#load-caos-hello-worker"

# --- (re)start the object server --------------------------------------------

echo "==> Creating network $NET (if needed)"
docker network create "$NET" >/dev/null 2>&1 || true

echo "==> Starting $SERVER_NAME against $REPO"
docker rm -f "$SERVER_NAME" >/dev/null 2>&1 || true
docker run -d --rm \
  --name "$SERVER_NAME" \
  --network "$NET" \
  -p "$PORT:8080" \
  -v "$REPO:/git" \
  caos-object-server:latest >/dev/null

# Give it a moment, then confirm it's actually up (a bad repo exits at once).
sleep 1
if ! docker ps --filter "name=^/${SERVER_NAME}$" --filter status=running -q \
     | grep -q .; then
  echo "error: $SERVER_NAME failed to start; logs:" >&2
  docker logs "$SERVER_NAME" >&2 || true
  exit 1
fi

echo "    serving repo $REPO on http://localhost:$PORT (and $SERVER_NAME:8080 inside $NET)"

# --- (re)start the compute server -------------------------------------------

# It shells out to `docker run` to launch worker containers, so it needs the
# host's docker socket. The workers it spawns join $NET, where they resolve the
# object server by name.
echo "==> Starting $COMPUTE_NAME"
docker rm -f "$COMPUTE_NAME" >/dev/null 2>&1 || true
docker run -d --rm \
  --name "$COMPUTE_NAME" \
  --network "$NET" \
  -p "$COMPUTE_PORT:9090" \
  -e "CAOS_DOCKER_NETWORK=$NET" \
  -v /var/run/docker.sock:/var/run/docker.sock \
  caos-compute-server:latest >/dev/null

sleep 1
if ! docker ps --filter "name=^/${COMPUTE_NAME}$" --filter status=running -q \
     | grep -q .; then
  echo "error: $COMPUTE_NAME failed to start; logs:" >&2
  docker logs "$COMPUTE_NAME" >&2 || true
  exit 1
fi

echo "    compute server up on http://localhost:$COMPUTE_PORT (and $COMPUTE_NAME:9090 inside $NET)"

# --- print the one-shot client command --------------------------------------

cat <<EOF

Servers are up. Run a one-shot interactive bash client with:

  docker run --rm -it --network $NET caos-client-bash:latest bash

Then inside the container, e.g.:

  caos get-hash <hash> /cas/foo
  mkdir -p /tmp && printf hello > /tmp/in
  caos put /tmp/in /cas/in
  caos run caos-hello-worker:latest /cas/out -- --in=/cas/in --greeting=hi
  caos get /cas/out/greeting && cat /cas/out/greeting

(It reaches the servers via \$CAOS_OBJECT_SERVER_URL / \$CAOS_COMPUTE_SERVER_URL,
defaulting to http://$SERVER_NAME:8080 and http://$COMPUTE_NAME:9090.)

\`caos run\` asks the compute server to run the image; the compute server forces
\`caos entrypoint\`, which populates /cas/args and runs /worker. The
caos-hello-worker image is a real worker: it copies each arg into /cas/out. (The
caos-client-bash image's /worker is just bash, handy for poking around but it
won't write /cas/out.)

Stop the servers with:  docker rm -f $SERVER_NAME $COMPUTE_NAME
EOF
