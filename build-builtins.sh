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
[ ${#names[@]} -eq 0 ] && names=(base bash fold file-count hello deep-deps rustc)

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

image_attr() { echo "caos-worker-$1-docker"; } # std name -> nix docker image attr

# Build every image in ONE nix invocation: the builds run in parallel under a
# single (low-memory) evaluation. Map each resulting store path back to its
# builtin via the image name baked into it (<hash>-caos-worker-<name>.tar.gz).
attrs=()
for name in "${names[@]}"; do attrs+=(".#$(image_attr "$name")"); done
echo "building ${#names[@]} images in parallel..." >&2
if ! built_paths=$(nix build "${attrs[@]}" --no-link --print-out-paths); then
  echo "build-builtins: nix build failed" >&2; exit 1
fi
declare -A img_path
while IFS= read -r p; do
  for name in "${names[@]}"; do
    case "$p" in *-caos-worker-"$name".tar.gz) img_path[$name]=$p ;; esac
  done
done <<<"$built_paths"
for name in "${names[@]}"; do
  [ -n "${img_path[$name]:-}" ] || { echo "build-builtins: no image built for $name" >&2; exit 1; }
done

# Import the images into git in PARALLEL. `import-image` only writes objects into
# its *local* repo — it does no network I/O — so each runs in its own throwaway
# repo with zero contention (the parallel win is the per-layer materialize/hash).
# We then union every repo's objects into the one CLIENT repo and push ONCE below:
# concurrent pushes to the same server repo race and corrupt it, so the network
# step stays serial.
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pids=()
for name in "${names[@]}"; do
  echo "importing $name..." >&2
  (
    repo="$WORK/repo-$name"
    git init -q "$repo"
    git -C "$repo" remote add caos "$SERVER_URL"
    hash=$(cd "$repo" && "$caos" import-image "${img_path[$name]}")
    printf '%s' "$hash" >"$WORK/$name.hash"
  ) &
  pids+=("$!")
done
rc=0
for pid in "${pids[@]}"; do wait "$pid" || rc=1; done
[ "$rc" -eq 0 ] || { echo "build-builtins: an import failed" >&2; exit 1; }

# Union each import's objects into CLIENT (objects are content-addressed, so this
# is a safe merge) so the single publish push below carries everything.
for name in "${names[@]}"; do
  cp -rn "$WORK/repo-$name/.git/objects/." "$CLIENT/.git/objects/"
done

# Assemble the {name: image} tree (a ref can name any object; std is a tree, so
# there's no commit to wrap it) and publish it to the server under refs/caos/std
# in one push, which uploads every builtin image it references.
entries=""
for name in "${names[@]}"; do
  entries+="040000 tree $(cat "$WORK/$name.hash")"$'\t'"$name"$'\n'
done
tree=$(printf '%s' "$entries" | git -C "$CLIENT" mktree)
# --force: refs/caos/std points at a tree, and git refuses to update a non-commit
# ref (or move it) without it. Re-publishing always replaces it.
git -C "$CLIENT" push -q --force caos "$tree:refs/caos/std"
# Record it locally too, so this repo can also resolve refs/caos/std.
git -C "$CLIENT" update-ref refs/caos/std "$tree"
echo "refs/caos/std -> $tree (published to $SERVER_URL)" >&2
echo "$tree"
