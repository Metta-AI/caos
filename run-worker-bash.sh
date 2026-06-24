#!/usr/bin/env bash
# Run the caos-worker-bash debugging image: an interactive shell with /cas set
# up by `caos entrypoint`, wired to the caos server.
#
# Arguments become the worker's inputs under /cas/args, the same shape `caos run`
# produces. `--name=value` is a literal string; `--name:@=path` reads a filesystem
# path (relative to the current directory) and references its content by git hash.
# For example:
#
#   ./run-worker-bash.sh --greeting=hi --conf:@=Cargo.toml --src:@=crates/caos
#
# then, inside the shell:  caos get /cas/args/conf && cat /cas/args/conf
#
# The daemon URLs are passed in as env vars (the images bake in none), defaulting
# to the docker-network service names; override by exporting them before running.
set -euo pipefail

DEFAULT_SERVER_URL="http://caos-server"

NET="${CAOS_DOCKER_NETWORK:-caos-net}"
SERVER_URL="${CAOS_SERVER_URL:-$DEFAULT_SERVER_URL}"
IMAGE="${CAOS_WORKER_BASH_IMAGE:-caos-worker-bash:latest}"

# Turn any --name=value / --name:@=path args into an args tree (via `caos build-args` in a
# throwaway container on the docker network, so it reaches the server by name and
# uploads the args over HTTP `/object`), and pass its hash as --args so `caos
# entrypoint` materializes them at /cas/args. The current directory is mounted
# read-only at /work so path-valued args resolve.
args=()
if [ "$#" -gt 0 ]; then
  hash=$(docker run --rm --network "$NET" \
    -e "CAOS_SERVER_URL=$SERVER_URL" \
    -v "$PWD:/work:ro" -w /work \
    --entrypoint /bin/caos "$IMAGE" build-args "$@")
  args=("--args=$hash")
fi

exec docker run --rm -it \
  --network "$NET" \
  -e "CAOS_SERVER_URL=$SERVER_URL" \
  "$IMAGE" "${args[@]}"
