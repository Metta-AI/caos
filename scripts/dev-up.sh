#!/usr/bin/env bash
#
# dev-up.sh — load the caos images, start the object server against a repo, and
# print a one-shot bash-client command.
#
# Usage:
#   scripts/dev-up.sh <repo-path>
#
# Env overrides:
#   CAOS_NET   docker network name           (default: caos-net)
#   CAOS_PORT  host port for the object server (default: 8080)

set -euo pipefail

NET="${CAOS_NET:-caos-net}"
PORT="${CAOS_PORT:-8080}"
SERVER_NAME="caos-object-server"

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

# --- print the one-shot client command --------------------------------------

cat <<EOF

Object server is up. Run a one-shot interactive bash client with:

  docker run --rm -it --network $NET caos-client-bash:latest

Then inside the container, e.g.:

  caos get-hash <hash> /cas/foo

(It reaches the server via \$CAOS_OBJECT_SERVER_URL, which defaults to
http://$SERVER_NAME:8080.)

Stop the server with:  docker rm -f $SERVER_NAME
EOF
