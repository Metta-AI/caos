#!/usr/bin/env bash
# Populate the caos `std` library — the workers clients reach as
# `/cas/std/<name>` — and publish it to the server as `refs/caos/std`.
#
# Entries come in three forms (see the is_*_entry predicates below):
#   delta     (flake-builder, runner,    the host-nix-built CLEAN image goes
#              cargo)                    to the registry once, keyed on its
#                                        store path; the entry is a git-docker
#                                        tree {base, config.json, layer<NN>}
#                                        the server stacks and pushes on first
#                                        use — the same shape std/flake-builder
#                                        emits
#   flake     (bash)                     literal checked-in std/<name> dirs —
#                                        complete worker images, /worker
#                                        included; the flake-builder images
#                                        each tree on first use
#   curry     (bin_names)                curry(runner, worker1=<binary>) — the
#                                        compiled workers ride the shared
#                                        runner pool
# The final `{name: entry}` tree is pushed to the server under `refs/caos/std`
# (uploading every referenced object, negotiated). Clients then `git fetch
# caos refs/caos/std` and resolve it locally to reach the library.
#
# Usage: ./build-builtins.sh [name ...]   (default: all)
# Requires the dev server running and git + skopeo + curl on PATH. No docker:
# this script composes no images — it pushes clean ones and describes deltas.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

names=("$@")
[ ${#names[@]} -eq 0 ] && names=(runner cargo bash flake-builder)

# std entries that are FLAKE TREES (design/flake-images.md, part 2): the
# checked-in std/<name> directory IS the published tree, copied whole —
# nothing generated (std/refresh.sh maintains the checked-in redundancies,
# tests/std-lint verifies them). The server's flake-builder images each
# tree on first use.
is_flake_entry() { case "$1" in bash) return 0 ;; *) return 1 ;; esac; }
# Everything else is a DELTA entry: the flake-builder (the bootstrap image),
# the runner (the pooled interpreter) — both self-contained nix closures — and
# cargo, whose image the root flake builds from the same `src` and toolchain as
# the binaries (so its deps are cargoArtifacts, not a second compile of them).
# The partition is exactly two-way, so "not a flake entry" IS the predicate.
image_names=()
for name in "${names[@]}"; do
  is_flake_entry "$name" || image_names+=("$name")
done

# caos-cli: a prebuilt binary if the caller injected one (CAOS_CLI — how caosd
# runs us from a store copy with no `nix` at runtime), else built from the flake.
if [ -n "${CAOS_CLI:-}" ]; then
  caos=$CAOS_CLI
else
  nix build .#caos-cli -o result-caos
  caos=$PROJECT/result-caos/bin/caos-cli
fi
SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}
export CAOS_SERVER_URL=$SERVER_URL

# A local client working repo with the server as its `caos` remote — the same
# shape a user has. `caos-cli` builds objects here (in-process via gix); `git
# push` ships them to the server. Reused across runs (git init is idempotent).
# CAOS_CLIENT_REPO relocates it off PROJECT (which is read-only when caosd runs
# us from the store); caosd points it at $CAOS_DATA so it persists per-project.
CLIENT=${CAOS_CLIENT_REPO:-$PROJECT/.caos-dev/client-repo}
git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "$SERVER_URL" 2>/dev/null \
  || git -C "$CLIENT" remote set-url caos "$SERVER_URL"

image_attr() { echo "caos-worker-$1-docker"; } # std name -> nix docker image attr

# The image tarball store paths. If the caller prebuilt them (CAOS_BUILTIN_IMAGES,
# a whitespace-separated list — how caosd hands us the flake's images with no
# `nix` at runtime), use those; else build every image in ONE nix invocation (the
# builds run in parallel under a single, low-memory evaluation). Either way, map
# each path back to its builtin via the image name baked into it
# (<hash>-caos-worker-<name>.tar.gz).
if [ -n "${CAOS_BUILTIN_IMAGES:-}" ]; then
  built_paths=$CAOS_BUILTIN_IMAGES
else
  attrs=()
  for name in "${image_names[@]}"; do attrs+=(".#$(image_attr "$name")"); done
  echo "building ${#image_names[@]} images in parallel..." >&2
  if ! built_paths=$(nix build "${attrs[@]}" --no-link --print-out-paths); then
    echo "build-builtins: nix build failed" >&2; exit 1
  fi
fi
declare -A img_path
# Unquoted: word-split on whitespace, covering both nix-build's newline-per-path
# output and a space-separated CAOS_BUILTIN_IMAGES. Store paths never contain
# whitespace or glob chars, so this is safe.
# shellcheck disable=SC2086
for p in $built_paths; do
  for name in "${image_names[@]}"; do
    case "$p" in *-caos-worker-"$name".tar.gz) img_path[$name]=$p ;; esac
  done
done
for name in "${image_names[@]}"; do
  [ -n "${img_path[$name]:-}" ] || { echo "build-builtins: no image built for $name" >&2; exit 1; }
done

# The worker binaries published as curries over runner (bin_names below).
# Prebuilt store paths arrive via CAOS_BUILTIN_BINS (how caosd avoids
# runtime nix), else they're nix-built here. Nothing is staged into flake
# trees anymore: runner's /worker bakes into its streamed image, cargo's
# compiles in-flake from the vendored source.
bin_names=(bash-tool llm-step rgrep rustc)
if [ -n "${CAOS_BUILTIN_BINS:-}" ]; then
  bin_paths=$CAOS_BUILTIN_BINS
else
  attrs=()
  for b in "${bin_names[@]}"; do attrs+=(".#worker-$b"); done
  echo "building worker binaries..." >&2
  if ! bin_paths=$(nix build "${attrs[@]}" --no-link --print-out-paths); then
    echo "build-builtins: nix build failed" >&2; exit 1
  fi
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

declare -A hash_of

REGISTRY=localhost:5000 # the compose stack's registry, host-published (caosd)
# The same registry as THIS SCRIPT reaches it over HTTP. On the host the two
# names coincide. Inside the test stack (design/test-stack-image.md) they do
# not: we run in a container on caos-net and must call it caos-registry:5000.
REGISTRY_HTTP=${CAOS_REGISTRY_HTTP:-$REGISTRY}
# How the SERVER reaches the same registry: it pulls a delta's `base` with
# skopeo from inside its own container, where the registry is a service on
# the docker network. Mirrors the server's own CAOS_REGISTRY_PUSH_URL default.
REGISTRY_BASE_HOST=${CAOS_REGISTRY_BASE_HOST:-caos-registry:5000}
# <repo:tag> -> the registry's manifest digest, or empty when the tag is absent.
# ABSENT AND BROKEN ARE DIFFERENT: a 404 is the ordinary first-push case and
# returns empty, but an unreachable registry or any other status is fatal —
# swallowing those would silently re-push (or worse, mint a `base` ref for an
# image nothing can pull).
manifest_digest() { # <repo:tag>
  local url headers status
  url="http://$REGISTRY_HTTP/v2/caos/manifests/$1"
  headers=$(curl -sSI \
    -H 'Accept: application/vnd.docker.distribution.manifest.v2+json' \
    -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
    "$url") || { echo "build-builtins: registry unreachable: $url" >&2; exit 1; }
  status=$(printf '%s\n' "$headers" | awk 'NR==1 {print $2}')
  case "$status" in
    200) printf '%s\n' "$headers" \
           | tr -d '\r' \
           | awk 'tolower($1)=="docker-content-digest:" {print $2}' ;;
    404) ;;
    *) echo "build-builtins: registry returned $status for $url" >&2; exit 1 ;;
  esac
}
tool_bin() { # <name> -> the path of bin/<name> among the built paths
  local p
  # shellcheck disable=SC2086
  for p in $bin_paths; do
    if [ -x "$p/bin/$1" ]; then
      echo "$p/bin/$1"
      return 0
    fi
  done
  echo "build-builtins: no binary $1 in the built paths" >&2
  exit 1
}
caos_bin=$(tool_bin caos)

# Which workspace binary is a streamed image's `/worker`. These images are
# HOST-BUILT and the host is their author, so their interpreter is composed on
# here rather than baked into the nix image — the clean image then depends on
# nothing in the workspace and is pushed once, keyed on its store path alone,
# while the binary rides as a layer keyed on its BYTES. flake-builder is
# absent because its /worker comes from its own flake, not from this
# workspace.
worker_bin_of() { # <std name> -> the workspace binary that is its /worker, or empty
  case "$1" in
    runner) echo worker-runner ;;
    cargo) echo worker-cargo ;;
    *) ;;
  esac
}

# The caos additions, staged as a git-docker LAYER TREE — the same shape
# std/flake-builder's stack stage emits, so both image paths in this tree
# compose deltas exactly one way. The server rebuilds the layer tar from
# this at convert time and applies the `<name>.caosmeta` sidecars for the
# modes git cannot record (setuid, sticky).
#
# Staged inside CLIENT because only git-tracked paths can be hashed here.
ADD=$CLIENT/layer-additions
rm -rf "${ADD:?}"
mkdir -p "$ADD/bin" "$ADD/usr/bin" "$ADD/etc"
install -m 755 "$caos_bin" "$ADD/bin/caos"
# setuid needs no privilege here: the sidecar is data, and the SERVER (root)
# applies the mode when it rebuilds the layer.
printf '{"mode":"4755","uid":0,"gid":0}' > "$ADD/bin/caos.caosmeta"
# /usr/bin/env for env-shebang scripts; every streamed userland has /bin/env.
ln -s /bin/env "$ADD/usr/bin/env"
printf 'root:x:0:0:root:/root:/sbin/nologin\nworker:x:1000:1000:caos worker:/tmp:/sbin/nologin\n' \
  > "$ADD/etc/passwd"
printf 'root:x:0:\nworker:x:1000:\n' > "$ADD/etc/group"
# The world-writable /tmp is an EMPTY directory, which git cannot record at
# all — so its 1777 sidecar is staged here and the directory itself is
# spliced into the tree below as the empty tree.
printf '{"mode":"1777","uid":0,"gid":0}' > "$ADD/tmp.caosmeta"
git -C "$CLIENT" add layer-additions
EMPTY_TREE=$(git -C "$CLIENT" mktree </dev/null)
additions_tree=$(
  {
    git -C "$CLIENT" ls-tree "$(git -C "$CLIENT" write-tree --prefix=layer-additions/)"
    printf '040000 tree %s\ttmp\n' "$EMPTY_TREE"
  } | git -C "$CLIENT" mktree
)

# Each streamed image's /worker, its own layer. worker-runner is the pool
# protocol's in-image half; worker-cargo is what the cargo image is FOR — and
# neither belongs in the nix closure it runs on. The cargo image is 3.4 GB of
# toolchain and baked deps, so baking a 5 MB binary into it made every Rust
# edit re-tar and re-gzip all of it under `nix build` and then gunzip and
# re-push it under `caosd up` (design/flake-images.md listed exactly this as
# the open boundary-caching item).
declare -A worker_tree
for name in "${image_names[@]}"; do
  wb=$(worker_bin_of "$name")
  if [ -n "$wb" ]; then
    WD=$CLIENT/layer-worker-$name
    rm -rf "${WD:?}"
    mkdir -p "$WD"
    install -m 755 "$(tool_bin "$wb")" "$WD/worker"
    git -C "$CLIENT" add "layer-worker-$name"
    worker_tree[$name]=$(git -C "$CLIENT" write-tree --prefix="layer-worker-$name/")
  fi
done

# The streamed std entries (design/one-stack-image.md): the nix tarball is the
# CLEAN image — no caos, no user db, no /tmp — and it goes to the registry
# ONCE, keyed on its store path ALONE. The clean images depend on nothing in
# the workspace, so a Rust edit never re-pushes one.
#
# The std entry itself is a git-docker delta {base, config.json, layer<NN>}:
# the server stacks our layers on that base and pushes the result the first
# time something runs it, memoized in redis by the tree's hash. So no layer
# bytes of the CLEAN image ever enter git, and this script does no image
# composition at all — no docker, no build context, no manifest surgery.
for name in "${image_names[@]}"; do
  tarball=${img_path[$name]}
  ctag="clean-$name-$(printf '%s' "$tarball" | sha1sum | cut -c1-12)"
  digest=$(manifest_digest "$ctag")
  if [ -n "$digest" ]; then
    echo "$name: registry hit for $ctag" >&2
  else
    echo "$name: streaming clean image $ctag..." >&2
    # skopeo's docker-archive transport reads an uncompressed tar; nix hands
    # us .tar.gz.
    gunzip -c "$tarball" > "$WORK/$name.tar"
    skopeo --insecure-policy copy --dest-tls-verify=false \
      "docker-archive:$WORK/$name.tar" "docker://$REGISTRY_HTTP/caos:$ctag" >&2
    rm -f "$WORK/$name.tar"
    digest=$(manifest_digest "$ctag")
    [ -n "$digest" ] || { echo "build-builtins: no digest for pushed $ctag" >&2; exit 1; }
  fi

  img=$CLIENT/img-$name
  rm -rf "${img:?}"
  mkdir -p "$img"
  # `base` is read by the SERVER, which pulls it with skopeo from inside its
  # own container (crates/server/src/compute.rs, fetch_base) — so it carries
  # the on-network registry name, not the host-published one the docker
  # daemon uses.
  printf 'docker://%s/caos@%s' "$REGISTRY_BASE_HOST" "$digest" > "$img/base"
  # The clean image's own OCI config, verbatim: it already names the runner
  # entrypoint and its PATH, because its flake said so.
  skopeo --insecure-policy inspect --tls-verify=false --config \
    "docker://$REGISTRY_HTTP/caos:$ctag" > "$img/config.json"
  git -C "$CLIENT" add "img-$name"
  hash_of[$name]=$(
    {
      git -C "$CLIENT" ls-tree "$(git -C "$CLIENT" write-tree --prefix="img-$name/")"
      printf '040000 tree %s\tlayer00\n' "$additions_tree"
      # Not `[ ... ] && printf`: this is the block's last command, so under
      # `set -e -o pipefail` a false test would fail the whole substitution
      # for every entry that carries no /worker of its own.
      if [ -n "${worker_tree[$name]:-}" ]; then
        printf '040000 tree %s\tlayer01\n' "${worker_tree[$name]}"
      fi
    } | git -C "$CLIENT" mktree
  )
  echo "$name: git-docker delta ${hash_of[$name]} over $ctag" >&2
done

# The flake-tree std entries (design/flake-images.md): bash is LITERAL —
# the checked-in std/<name> directory is the published tree, copied whole,
# nothing generated. The server's flake-builder images the tree on first
# use, memoized in the registry on the tree's own hash, so re-publishing an
# unchanged tree costs nothing.
# Staged inside CLIENT (like worker-common below) because only git-tracked
# paths can be hashed here.
for name in "${names[@]}"; do
  is_flake_entry "$name" || continue
  rm -rf "${CLIENT:?}/$name"
  # -L: PROJECT may be an image layout whose files are SYMLINKS into the nix
  # store (the test stack, design/test-stack-image.md). Copying those verbatim
  # would publish a tree of links to store paths no other container has — the
  # flake-builder then finds no flake.nix. Dereference, so what is published
  # is always the content. On the host every entry is already a real file.
  cp -RL "$PROJECT/std/$name" "$CLIENT/$name"
  # PROJECT may be a read-only store copy (caosd); writable so the next
  # publish's rm -rf works.
  chmod -R u+w "$CLIENT/$name"
  git -C "$CLIENT" add "$name"
  hash_of[$name]=$(git -C "$CLIENT" write-tree --prefix="$name/")
  echo "$name: flake tree ${hash_of[$name]}" >&2
done

# The compiled workers: each is published as a ready-to-run curry over the
# shared runner image — std/<name> = curry(runner, worker1=<static binary>) —
# NOT as a worker image of its own, so its runs ride the warm runner pool
# (design/runner-protocol.md) and a rebuild ships one small blob, not an
# image. `caos-cli curry` ingests the binary and pushes the curry; the std
# ref push below pins both. Skipped when `runner` isn't among the names (a
# partial, name-scoped run has no image to curry onto).
if [ -n "${hash_of[runner]:-}" ]; then
  declare -A bin_path
  # Map each worker binary to the store path that CONTAINS it (bin/worker-<b>),
  # not by matching the path's name: the workspace builds as one derivation
  # (caos-workspace), so every worker binary shares a single store path — a
  # name match on "-worker-<b>" would never hit. Existence of bin/worker-<b>
  # is the honest test and works whether bins are one path or many.
  # shellcheck disable=SC2086
  for p in $bin_paths; do
    for b in "${bin_names[@]}"; do
      [ -x "$p/bin/worker-$b" ] && bin_path[$b]=$p
    done
  done
  for b in "${bin_names[@]}"; do
    [ -n "${bin_path[$b]:-}" ] || { echo "build-builtins: no binary built for worker-$b" >&2; exit 1; }
    # rustc is orchestration over the cargo worker: curry in the published
    # cargo entry (as a literal image ref) and the worker-common source tree
    # its generated projects link against.
    extra=()
    if [ "$b" = rustc ]; then
      [ -n "${hash_of[cargo]:-}" ] || continue
      rm -rf "$CLIENT/worker-common"
      cp -RL "$PROJECT/crates/worker-common" "$CLIENT/worker-common"
      # PROJECT may be a read-only store copy (caosd); writable so the next
      # publish's rm -rf works.
      chmod -R u+w "$CLIENT/worker-common"
      git -C "$CLIENT" add worker-common
      extra=("--cargo=${hash_of[cargo]}" "--worker_common:@=worker-common")
    fi
    # Ingestion only accepts git-tracked worktree paths, so stage a copy of
    # the binary in the client repo (overwritten on every publish).
    install -m 755 "${bin_path[$b]}/bin/worker-$b" "$CLIENT/worker-$b"
    git -C "$CLIENT" add "worker-$b"
    hash_of[$b]=$(cd "$CLIENT" && "$caos" curry "${hash_of[runner]}" -- "--worker1:@=worker-$b" "${extra[@]}")
    echo "$b: curry ${hash_of[$b]}" >&2
    names+=("$b")
  done
fi

# refs/caos/bins is GONE (2026-07-30). It carried the HOST's nix-built binaries
# into caos so in-caos tools would not have to compile the workspace themselves
# — `run-tool` resolved the ref and passed its hash as `--bins`. The suite has
# compiled from source for some time now (tests/lib/suite.sh: "There is no
# --bins anymore: the tree under test is compiled from source"), so the ref had
# no reader left; only this publisher and the auto-arg in cli_run_tool.
#
# Deleting it removes a staging pass that copied ~61 MB of binaries into the
# client worktree just to hash them — the largest single piece of the publish's
# cost, and pure duplication, since git dedups those blobs against the curries
# that already carry the same bytes.

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
