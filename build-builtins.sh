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
[ ${#names[@]} -eq 0 ] && names=(base bash fold file-count dirs-only hello deep-deps rustc runner)

nix build .#caos-cli -o result-caos
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

# Some builtins ship as a thin delta on a stock docker base instead of a
# self-contained image: the nix image holds only our bits, and `import-image
# --base docker://<ref>` records the stock base so the heavy toolchain rides as
# stock registry layers (pulled server-side at convert time) rather than in git.
# rustc bases on the stock rust image (cargo/rustc/gcc/glibc); the worker base on
# stock debian (glibc, for source-built workers); the rest are self-contained.
import_base() { # std name -> docker:// base ref, or empty for self-contained
  case "$1" in
    base | runner) echo "docker://debian:stable-slim" ;;
    rustc) echo "docker://rust:1-bookworm" ;;
    *) echo "" ;;
  esac
}

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

# Import cache: refs/caos/src/<sha1(image store path)> in CLIENT pins each image's
# imported tree. The store path is immutable + content-addressed, so the ref's
# presence means "already imported this exact image" — we reuse the hash and skip
# the multi-second re-unpack/re-hash (rustc especially). The ref also keeps the
# objects from gc. (Same scheme as the flake's set-stdlib, flake.nix.) Wipe CLIENT
# and the cache goes with it; change an image and its store path -> a new ref.
src_ref_of() { echo "refs/caos/src/$(printf '%s' "$1" | sha1sum | cut -c1-40)"; }

declare -A hash_of
to_import=()
for name in "${names[@]}"; do
  cached=$(git -C "$CLIENT" rev-parse --verify --quiet "$(src_ref_of "${img_path[$name]}")^{tree}" || true)
  if [ -n "$cached" ]; then
    echo "$name: reusing import $cached" >&2
    hash_of[$name]=$cached
  else
    to_import+=("$name")
  fi
done

# Import the cache-misses in PARALLEL. `import-image` only writes objects into its
# *local* repo (no network I/O), so each runs in its own throwaway repo with zero
# contention (the parallel win is the per-layer materialize/hash). We then union
# every repo's objects into CLIENT, pin each src_ref, and push ONCE below —
# concurrent pushes to one server repo race and corrupt it, so the push stays serial.
if [ "${#to_import[@]}" -gt 0 ]; then
  WORK=$(mktemp -d)
  trap 'rm -rf "$WORK"' EXIT
  pids=()
  for name in "${to_import[@]}"; do
    echo "$name: importing..." >&2
    (
      repo="$WORK/repo-$name"
      git init -q "$repo"
      git -C "$repo" remote add caos "$SERVER_URL"
      base=$(import_base "$name")
      if [ -n "$base" ]; then
        hash=$(cd "$repo" && "$caos" import-image --base "$base" "${img_path[$name]}")
      else
        hash=$(cd "$repo" && "$caos" import-image "${img_path[$name]}")
      fi
      printf '%s' "$hash" >"$WORK/$name.hash"
    ) &
    pids+=("$!")
  done
  rc=0
  for pid in "${pids[@]}"; do wait "$pid" || rc=1; done
  [ "$rc" -eq 0 ] || { echo "build-builtins: an import failed" >&2; exit 1; }
  for name in "${to_import[@]}"; do
    cp -rn "$WORK/repo-$name/.git/objects/." "$CLIENT/.git/objects/"
    hash_of[$name]=$(cat "$WORK/$name.hash")
    git -C "$CLIENT" update-ref "$(src_ref_of "${img_path[$name]}")" "${hash_of[$name]}"
  done
fi

# Assemble the {name: image} tree (a ref can name any object; std is a tree, so
# there's no commit to wrap it) and publish it to the server under refs/caos/std
# in one push, which uploads every builtin image the server doesn't already have.
entries=""
for name in "${names[@]}"; do
  entries+="040000 tree ${hash_of[$name]}"$'\t'"$name"$'\n'
done
tree=$(printf '%s' "$entries" | git -C "$CLIENT" mktree)
# --force: refs/caos/std points at a tree, and git refuses to update a non-commit
# ref (or move it) without it. Re-publishing always replaces it.
git -C "$CLIENT" push -q --force caos "$tree:refs/caos/std"
# Record it locally too, so this repo can also resolve refs/caos/std.
git -C "$CLIENT" update-ref refs/caos/std "$tree"
echo "refs/caos/std -> $tree (published to $SERVER_URL)" >&2
echo "$tree"
