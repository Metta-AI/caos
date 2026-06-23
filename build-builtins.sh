#!/usr/bin/env bash
# Populate the `caos/std` built-ins ref — the "standard library" of worker images
# clients reach as `/cas/std/<name>`.
#
# For each named worker it builds the image with Nix, imports it as a git-docker
# tree (real git objects, so versions dedup), then assembles a `{name: image}`
# tree, and points `refs/caos/std` straight at it. Run from the
# caos source tree: the ref lands in this repo's `.git`, which is the server's
# repo for now (later this becomes a push to wherever the server pulls from).
#
# Usage: ./build-builtins.sh [name ...]   (default: all)
# A name maps to the `caos-worker-<name>` image; `base` -> `caos-worker-base`.
# Requires the dev server running (object DB = this repo's .git) and git on PATH.
set -euo pipefail
cd "$(dirname "$0")"

names=("$@")
[ ${#names[@]} -eq 0 ] && names=(base fold file-count hello deep-deps rustc)

nix build .#client -o result-client
caos=$PWD/result-client/bin/client
export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}
CAS=$PWD/.caos-dev/builtins-cas
rm -rf "$CAS"; mkdir -p "$CAS"
export CAOS_CAS_DIR=$CAS
trap 'rm -rf "$CAS"' EXIT

image_attr() { # std name -> nix docker image attribute
  case "$1" in
    base) echo caos-worker-base-docker ;;
    *) echo "caos-worker-$1-docker" ;;
  esac
}

# Build + import each builtin, collecting `git mktree` lines (name -> tree hash).
# `import-image` prints the git-docker tree's hash.
entries=""
for name in "${names[@]}"; do
  attr=$(image_attr "$name")
  echo "building + importing $name ($attr)..." >&2
  nix build ".#$attr" -o "result-builtin-$name"
  hash=$("$caos" import-image "result-builtin-$name" "$CAS/$name")
  entries+="040000 tree $hash"$'\t'"$name"$'\n'
done

# Assemble the {name: image} tree and point refs/caos/std straight at it (a ref
# can name any object; std is a tree, so there's no commit to wrap it).
tree=$(printf '%s' "$entries" | git mktree)
git update-ref refs/caos/std "$tree"
echo "refs/caos/std -> $tree" >&2
echo "$tree"
