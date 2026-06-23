#!/usr/bin/env bash
# Populate the caos `std` library — the worker images clients reach as
# `/cas/std/<name>` — and publish it to the server as `refs/caos/std`.
#
# For each named worker it builds the image with Nix and imports it as a
# git-docker tree (real git objects, so versions dedup) into a local *client*
# repo, assembles a `{name: image}` tree, and pushes that tree to the server
# under `refs/caos/std`. The push uploads every referenced builtin image
# (negotiated) and pins them under the ref. Clients then `git fetch caos
# refs/caos/std` and resolve it locally to reach the library.
#
# Usage: ./build-builtins.sh [name ...]   (default: all)
# A name maps to the `caos-worker-<name>` image; `base` -> `caos-worker-base`.
# Requires the dev server running and git on PATH.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

names=("$@")
[ ${#names[@]} -eq 0 ] && names=(base fold file-count hello deep-deps rustc)

nix build .#caos -o result-caos
caos=$PROJECT/result-caos/bin/caos-cli
SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}
export CAOS_SERVER_URL=$SERVER_URL

# A local client working repo with the server as its `caos` remote — the same
# shape a user has. `caos-cli` builds objects here (in-process via gix); `git
# push` ships them to the server. Reused across runs (git init is idempotent).
CLIENT=$PROJECT/.caos-dev/client-repo
git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "$SERVER_URL" 2>/dev/null \
  || git -C "$CLIENT" remote set-url caos "$SERVER_URL"

CAS=$PROJECT/.caos-dev/builtins-cas
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
# `import-image` (run inside the client repo, so its git transport finds it)
# writes the git-docker tree locally and prints its hash.
entries=""
for name in "${names[@]}"; do
  attr=$(image_attr "$name")
  echo "building + importing $name ($attr)..." >&2
  nix build ".#$attr" -o "result-builtin-$name"
  hash=$(cd "$CLIENT" && "$caos" import-image "$PROJECT/result-builtin-$name" "$CAS/$name")
  entries+="040000 tree $hash"$'\t'"$name"$'\n'
done

# Assemble the {name: image} tree locally (a ref can name any object; std is a
# tree, so there's no commit to wrap it) and publish it to the server under
# refs/caos/std — which uploads every builtin image it references and pins them.
tree=$(printf '%s' "$entries" | git -C "$CLIENT" mktree)
# --force: refs/caos/std points at a tree, and git refuses to update a
# non-commit ref (or move it) without it. Re-publishing always replaces it.
git -C "$CLIENT" push -q --force caos "$tree:refs/caos/std"
# Record it locally too, so this repo can also resolve refs/caos/std.
git -C "$CLIENT" update-ref refs/caos/std "$tree"
echo "refs/caos/std -> $tree (published to $SERVER_URL)" >&2
echo "$tree"
