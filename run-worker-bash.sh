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

DEFAULT_OBJECT_URL="http://caos-object-server"

NET="${CAOS_DOCKER_NETWORK:-caos-net}"
OBJECT_URL="${CAOS_OBJECT_SERVER_URL:-$DEFAULT_OBJECT_URL}"
COMPUTE_URL="${CAOS_COMPUTE_SERVER_URL:-http://caos-compute-server}"
IMAGE="${CAOS_WORKER_BASH_IMAGE:-caos-worker-bash:latest}"

# Build the args tree on the host with git plumbing and print its hash. Objects
# go straight into the repo (the one the default object server is backed by — see
# the Tiltfile), so:
#   * a clean, tracked path costs a single `git ls-tree` — no re-read, no upload,
#     so a large unchanged directory is effectively free;
#   * a dirty/untracked path is hashed into the object store now (a directory via
#     a throwaway index, so .gitignore'd files are skipped); and
#   * a non-path value is stored as a literal blob.
# Entries are assembled with `git mktree`. Nothing here touches HEAD, the real
# index, or commits. Only used when the object server is this repo's (the default
# Tilt one); otherwise we fall back to `caos build-args`, which uploads via
# whatever object server the container is wired to.
build_args_via_git() {
  local kv name value rest mode type hash idx full entries=""
  for kv in "$@"; do
    case "$kv" in
      --*=*) ;;
      *) echo "argument must look like --name=value, got: $kv" >&2; return 1 ;;
    esac
    name=${kv#--}; name=${name%%=*}
    value=${kv#--*=}
    if [ -z "$name" ] || [ "${name%/*}" != "$name" ]; then
      echo "argument name must be a single path component, got: $name" >&2
      return 1
    fi

    if [ -e "$value" ]; then
      if [ -z "$(git status --porcelain -- "./$value" 2>/dev/null)" ] \
         && rest=$(git ls-tree HEAD -- "./$value" 2>/dev/null) && [ -n "$rest" ]; then
        # Clean and tracked: reuse git's recorded entry as-is (instant).
        mode=${rest%% *}; rest=${rest#* }
        type=${rest%% *}; rest=${rest#* }
        hash=${rest%%$'\t'*}
      elif [ -d "$value" ]; then
        # Dirty/untracked directory: hash its current contents via a throwaway
        # index (skips .gitignore'd files), then pull out the subtree's hash. The
        # index must not pre-exist (git rejects an empty file), so use a fresh dir.
        idx=$(mktemp -d)
        GIT_INDEX_FILE="$idx/index" git add -A -- "./$value"
        full=$(GIT_INDEX_FILE="$idx/index" git write-tree)
        rm -rf "$idx"
        hash=$(git rev-parse "$full:./$value")
        mode=040000; type=tree
      else
        # Dirty/untracked file: hash it now.
        hash=$(git hash-object -w -- "$value")
        mode=100644; [ -x "$value" ] && mode=100755; type=blob
      fi
    else
      # Not a path: store the literal value as a blob.
      hash=$(printf '%s' "$value" | git hash-object -w --stdin)
      mode=100644; type=blob
    fi

    entries+="$mode $type $hash"$'\t'"$name"$'\n'
  done
  printf '%s' "$entries" | git mktree
}

# Turn any --name=value args into an args tree and pass its hash as --args, so
# `caos entrypoint` materializes them at /cas/args.
args=()
if [ "$#" -gt 0 ]; then
  if [ "$OBJECT_URL" = "$DEFAULT_OBJECT_URL" ] \
     && command -v git >/dev/null 2>&1 \
     && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    # Fast path: build the tree with git, writing straight into this repo.
    hash=$(build_args_via_git "$@")
  else
    # Fallback: build it with `caos build-args` in a throwaway container that
    # shares the docker network (to reach the object server by name) and mounts
    # the current directory read-only at /work (so path values resolve).
    hash=$(docker run --rm --network "$NET" \
      -e "CAOS_OBJECT_SERVER_URL=$OBJECT_URL" \
      -v "$PWD:/work:ro" -w /work \
      --entrypoint /bin/caos "$IMAGE" build-args "$@")
  fi
  args=("--args=$hash")
fi

exec docker run --rm -it \
  --network "$NET" \
  -e "CAOS_OBJECT_SERVER_URL=$OBJECT_URL" \
  -e "CAOS_COMPUTE_SERVER_URL=$COMPUTE_URL" \
  "$IMAGE" "${args[@]}"
