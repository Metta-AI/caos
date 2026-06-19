#!/usr/bin/env bash
# Run the caos-worker-bash debugging image: an interactive shell with /cas set
# up by `caos entrypoint`, wired to the object and compute servers.
#
# The daemon URLs are passed in as env vars (the images bake in none), defaulting
# to the docker-network service names; override by exporting them before running.
set -euo pipefail

NET="${CAOS_DOCKER_NETWORK:-caos-net}"
OBJECT_URL="${CAOS_OBJECT_SERVER_URL:-http://caos-object-server:8080}"
COMPUTE_URL="${CAOS_COMPUTE_SERVER_URL:-http://caos-compute-server:9090}"

exec docker run --rm -it \
  --network "$NET" \
  -e "CAOS_OBJECT_SERVER_URL=$OBJECT_URL" \
  -e "CAOS_COMPUTE_SERVER_URL=$COMPUTE_URL" \
  caos-worker-bash:latest
