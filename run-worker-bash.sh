#!/usr/bin/env bash
# Run the caos-worker-bash debugging image: an interactive shell with /cas set
# up by `caos entrypoint`, wired to the object and compute servers.
#
# Any --name=value arguments become the worker's inputs under /cas/args, the same
# shape `caos run` produces. A value that names an existing path (relative to the
# current directory) is stored from the filesystem and referenced by its git
# hash; anything else is stored as a literal string. For example:
#
#   ./run-worker-bash.sh --greeting=hi --conf=Cargo.toml --src=crates/client
#
# then, inside the shell:  caos get /cas/args/conf && cat /cas/args/conf
#
# The daemon URLs are passed in as env vars (the images bake in none), defaulting
# to the docker-network service names; override by exporting them before running.
set -euo pipefail

NET="${CAOS_DOCKER_NETWORK:-caos-net}"
OBJECT_URL="${CAOS_OBJECT_SERVER_URL:-http://caos-object-server}"
COMPUTE_URL="${CAOS_COMPUTE_SERVER_URL:-http://caos-compute-server}"
IMAGE="${CAOS_WORKER_BASH_IMAGE:-caos-worker-bash:latest}"

# Turn any --name=value args into an args tree and pass its hash as --args, so
# `caos entrypoint` materializes them at /cas/args. We build the tree with
# `caos build-args` in a throwaway container that shares the docker network (to
# reach the object server by name) and mounts the current directory read-only at
# /work (so path values resolve relative to your pwd).
args=()
if [ "$#" -gt 0 ]; then
  hash=$(docker run --rm --network "$NET" \
    -e "CAOS_OBJECT_SERVER_URL=$OBJECT_URL" \
    -v "$PWD:/work:ro" -w /work \
    --entrypoint /bin/caos "$IMAGE" build-args "$@")
  args=("--args=$hash")
fi

exec docker run --rm -it \
  --network "$NET" \
  -e "CAOS_OBJECT_SERVER_URL=$OBJECT_URL" \
  -e "CAOS_COMPUTE_SERVER_URL=$COMPUTE_URL" \
  "$IMAGE" "${args[@]}"
