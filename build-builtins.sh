#!/usr/bin/env bash
# Populate the caos `std` library — the workers clients reach as
# `/cas/std/<name>` — and publish it to the server as `refs/caos/std`.
#
# Every entry but one is the checked-in `std/<name>` SOURCE DIR, published
# verbatim: a `.caos-expr` says how it is built and resolving the entry evaluates
# that expression (design/caos-expr.md, Phase 3). `runner` is the exception — a
# leaf image with no source, published as the host-built git-docker DELTA
# {base, config.json, layer<NN>} whose clean image this script streams to the
# registry once, keyed on its store path, for the server to stack on first use.
#
# The published tree is `{ .caos-expr, <name> -> <source> }`: entries are
# UN-DEEPENED (each keeps its `DEPS`), and the std-root `std/.caos-expr`
# (`run deep-deps -- --in:@=.`) deepens them at RESOLVE time. So std resolves by
# descent, and nothing about the deepened shape is baked into the ref. This
# script still hand-deepens — but only to form SEED KEYS (see the seed block).
#
# Three of the entries (flake-builder, cargo, and the rustc/deep-deps sentinels)
# cannot be built by the machinery they ARE, so their images are hand-built here
# and published as SEED RECORDS under `refs/caos/seed` instead; the
# core-seeder-runner answers the exact arg-tree key their expressions form.
#
# The tree is pushed to the server under `refs/caos/std` (uploading every
# referenced object, negotiated). Clients then `git fetch caos refs/caos/std`
# and resolve it locally to reach the library.
#
# Usage: ./build-builtins.sh [name ...]   (default: all)
# Requires the dev server running and git + skopeo + curl on PATH. No docker:
# this script composes no images — it pushes clean ones and describes deltas.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

names=("$@")
[ ${#names[@]} -eq 0 ] && names=(runner cargo bash flake-builder merge rgrep bash-tool llm-client llm-call llm-step deep-deps rustc llm-stub)

# std entries that are SOURCE DIRS published whole (design/caos-expr.md,
# Phase 3): the checked-in std/<name> directory IS the published tree, copied
# verbatim — nothing generated (std/refresh.sh maintains the checked-in
# redundancies, tests/std-lint verifies them). Each carries a `.caos-expr`, so
# resolving `/cas/std/<name>` evaluates that expression to build the image:
#   bash, merge  — flake dirs; the expr runs /std/flake-builder on the dir.
#   rgrep        — a Rust project; the expr builds it with rustc (its
#                  regex dep rides the seeded /std/cargo bake).
#   bash-tool    — a Rust project too (worker-common only, no crates.io deps),
#                  built with rustc exactly like rgrep. No binary is staged here
#                  — rustc compiles it on resolution.
#   llm-call,    — Rust projects built with rustc; their crates.io deps ride the
#   llm-step       bake (anchored by crates/bake-anchor) and they link the
#                  llm-client crate via a numbered --dep0 mount.
#   llm-stub     — a Rust project too, but built by CARGO DIRECTLY (its expr runs
#                  DEEP-DEPS/cargo --cmd=build): the tests run it as a plain
#                  sidecar HTTP server, so what they need is the produced FILE
#                  at bin/llm-stub, not a runner-pool image.
#   llm-client   — a LIBRARY entry (no .caos-expr): source mounted into the llm
#                  tools as a code dep, never resolved as an image itself.
#   deep-deps,   — SEEDED core: a minimal {.caos-expr} sentinel entry; bootstrap
#   rustc          hand-builds the curry result and seeds it (below). No binary
#                  is staged into the entry.
is_source_entry() {
  case "$1" in
    bash | bash-tool | deep-deps | llm-call | llm-client | llm-step | merge | rgrep | rustc | llm-stub) return 0 ;;
    *) return 1 ;;
  esac
}
# Everything else is a DELTA entry: the flake-builder (the bootstrap image),
# the runner (the pooled interpreter) — both self-contained nix closures — and
# cargo, whose image the root flake builds from the same `src` and toolchain as
# the binaries (so its deps are cargoArtifacts, not a second compile of them).
# The partition is exactly two-way, so "not a source entry" IS the predicate.
# (flake-builder is a special hybrid: it BUILDS as an image delta here, but its
# std ENTRY is later swapped to a source tree + a seed record — see the seed
# block near the end, design/caos-expr.md Phase 3.)
image_names=()
for name in "${names[@]}"; do
  is_source_entry "$name" || image_names+=("$name")
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
bin_names=(deep-deps rustc)
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
# The world-writable /tmp. Its 1777 mode is a sidecar because git records no
# modes beyond the exec bit — but the DIRECTORY is now carried by a keep-file
# rather than as an empty tree, and that is load-bearing.
#
# It used to be spliced in below as git's empty tree. That is a legal tree entry
# and git transfers it fine, but an empty directory does not survive a
# materialize -> read -> re-put round trip, so ANY worker that rebuilds a tree
# from the filesystem silently drops it. deep-deps is such a worker: deepening
# the runner delta returned a tree with `layer00/tmp` GONE, which made the
# worker-deepened entry disagree with this script's hand-deepen and broke every
# seeded key that reached the runner (measured: 7 tests, all rustc-built tools).
#
# A keep-file makes the directory ordinary, so nothing anywhere has to special
# case it — the same move as the sidecar beside it, one step further.
mkdir -p "$ADD/tmp"
printf 'This file exists so that /tmp is a NON-EMPTY directory.\n\nAn empty directory is not representable in a git worktree and does not survive\na materialize -> read -> re-put round trip, so a worker that rebuilds a tree\nfrom the filesystem (deep-deps) drops it. /tmp must exist in the image, so it\ncarries this file. Its 1777 mode rides in the tmp.caosmeta sidecar.\n' \
  > "$ADD/tmp/.caos-keep"
printf '{"mode":"1777","uid":0,"gid":0}' > "$ADD/tmp.caosmeta"
git -C "$CLIENT" add layer-additions
# No splice any more: `tmp` is an ordinary directory in the staged layout, so
# `write-tree` carries it like everything else.
additions_tree=$(git -C "$CLIENT" write-tree --prefix=layer-additions/)

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

# refs/caos/bins is GONE (2026-07-30). It carried the HOST's nix-built binaries
# into caos so in-caos tools would not have to compile the workspace themselves
# — `run-tool` resolved the ref and passed its hash as `--bins`. The suite has
# compiled from source for some time now (caos-tools/test.sh: "There is no
# --bins: the tree under test is compiled from source"), so the ref had
# no reader left; only this publisher and the auto-arg in cli_run_tool.

# ---- std entries: published UN-deepened; hand-deepened only for seed keys ----
# Every std entry is a checked-in SOURCE tree whose `.caos-expr` builds it on
# resolution (design/caos-expr.md, Phase 3), EXCEPT `runner` — a leaf image with
# no source. What we PUBLISH is that source verbatim, DEPS and all, under a std
# root carrying `std/.caos-expr` (`run deep-deps -- --in:@=.`): resolving
# `/cas/std/<name>` evaluates that root expression first, so the REAL deep-deps
# worker computes each entry's `DEEP-DEPS/<dep>` mounts at resolve time. std is
# resolved BY DESCENT; nothing here bakes the deepened shape into the ref.
#
# `deepen_entry` below survives for ONE purpose: forming SEED KEYS. A seeded core
# item's arg-tree `in` is its DEEPENED entry — what the root expression produces
# — and bootstrap must know that hash before any stack exists to compute it. So
# it hand-deepens, and the hand-deepen must match the worker BYTE FOR BYTE or the
# seeder registers a key no caller ever forms. Today only `cargo` has DEPS among
# the seeded core (flake-builder, rustc and deep-deps are DEPS-free, so their
# deepened form is identity), which is where that identity is load-bearing.

# runner/cargo/flake-builder were hand-built as git-docker deltas into hash_of[]
# by the image loop above. Capture those deltas before the deepen pass overwrites
# the std entries with source trees: runner STAYS a delta (a leaf image), while
# cargo's and flake-builder's deltas become SEED RESULTS.
runner_delta=${hash_of[runner]:-}
cargo_delta=${hash_of[cargo]:-}
fb_delta=${hash_of[flake-builder]:-}

# Map each worker binary to the store path that CONTAINS it (bin/worker-<b>) —
# every worker binary shares one workspace store path, so existence of
# bin/worker-<b> is the honest test.
declare -A bin_path
if [ -n "$runner_delta" ]; then
  # shellcheck disable=SC2086
  for p in $bin_paths; do
    for b in "${bin_names[@]}"; do
      [ -x "$p/bin/worker-$b" ] && bin_path[$b]=$p
    done
  done
fi

declare -A undeepened

# Stage a checked-in std/<name> source dir into CLIENT and record its tree.
# `cp -RL`: PROJECT may be an image layout of SYMLINKS into the nix store (the
# test stack); dereference so what is published is the content.
stage_source() { # <name>
  rm -rf "${CLIENT:?}/$1"
  cp -RL "$PROJECT/std/$1" "$CLIENT/$1"
  chmod -R u+w "$CLIENT/$1" # PROJECT may be a read-only store copy (caosd)
  git -C "$CLIENT" add "$1"
  undeepened[$1]=$(git -C "$CLIENT" write-tree --prefix="$1/")
}

# A compiled-worker BINARY is no longer staged into any std entry: the runner-
# pool leaves (bash-tool, llm-*) are source-built by rustc, and the seeded core
# (rustc, deep-deps) has its curry hand-built as a SEED RESULT below — the
# checked-in entry is just a `.caos-expr` sentinel. So there is no `stage_worker`.

for name in "${names[@]}"; do
  # EVERY entry is now a checked-in source dir — `runner` included. It used to be
  # published as the raw delta, which meant `std/runner` was a name with no
  # directory behind it: `std/rustc/DEPS` says `../runner`, and a tree cannot be
  # self-resolving when a declared dependency is not IN it. It is a
  # `{.caos-expr}` sentinel now, seeded like flake-builder (below).
  stage_source "$name"
done

# The hand-deepen — SEED KEYS ONLY, never published (see the block header).
# deepen_entry(name) replaces the entry's top-level DEPS with a DEEP-DEPS/
# subtree of its deepened dependencies (each a sibling std entry named
# `../<name>`), recursively and memoized — so a dep reached twice is ONE shared
# tree (git dedups the identical hash, matching the deep-deps worker's
# absolute-path sharing). An entry with no DEPS is identity (its subtrees are
# DEPS-free, which the worker reproduces unchanged — so this matches byte for
# byte). Divergence from the worker is not silent: the seeded key stops matching
# the key resolution forms, the job falls through to the generic runner and dies
# on a sentinel image, and every test that resolves the item goes red.
declare -A deepened
deepen_entry() { # <name> -> deepened tree hash (stdout)
  local name=$1
  if [ -n "${deepened[$name]:-}" ]; then echo "${deepened[$name]}"; return; fi
  local tree=${undeepened[$name]} deps_oid="" meta file
  while IFS=$'\t' read -r meta file; do
    if [ "$file" = DEPS ]; then deps_oid=${meta##* }; fi
  done < <(git -C "$CLIENT" ls-tree "$tree")

  local result
  if [ -z "$deps_oid" ]; then
    result=$tree
  else
    # DEEP-DEPS/<mount> = deepened(sibling entry). DEPS lines are `<path> <name>`
    # (path `../<sibling>`); comments/blank lines skipped. mktree sorts.
    local dd_lines="" dep_path mount
    while read -r dep_path mount; do
      if [ -n "$dep_path" ]; then
        case "$dep_path" in
          \#*) ;;
          *) dd_lines+="040000 tree $(deepen_entry "$(basename "$dep_path")")"$'\t'"$mount"$'\n' ;;
        esac
      fi
    done < <(git -C "$CLIENT" cat-file blob "$deps_oid")
    local dd_tree
    dd_tree=$(printf '%s' "$dd_lines" | git -C "$CLIENT" mktree)
    # The entry's own tree minus DEPS, plus the DEEP-DEPS subtree.
    result=$(
      {
        git -C "$CLIENT" ls-tree "$tree" | while IFS=$'\t' read -r meta file; do
          if [ "$file" != DEPS ]; then printf '%s\t%s\n' "$meta" "$file"; fi
        done
        printf '040000 tree %s\tDEEP-DEPS\n' "$dd_tree"
      } | git -C "$CLIENT" mktree
    )
  fi
  deepened[$name]=$result
  echo "$result"
}

for name in "${names[@]}"; do
  hash_of[$name]=$(deepen_entry "$name")
  echo "$name: deepened entry (seed key) ${hash_of[$name]}" >&2
done

# Assemble the published tree — the UN-deepened entries plus the std-root
# `.caos-expr` that deepens them on resolution (a ref can name any object; std is
# a tree, so there's no commit to wrap it) — and push it to the server under
# refs/caos/std in one push, which uploads every builtin image the server doesn't
# already have. The root expression is the ONLY generated part of the std root:
# `std/refresh.sh` is a maintenance script and is deliberately not published.
# `--stdin` (not a path argument): PROJECT may be an image layout of symlinks
# into the nix store, and a redirect dereferences where `hash-object <path>`
# would hash the link. It also sidesteps hashing a file outside CLIENT.
root_expr=$(git -C "$CLIENT" hash-object -w --stdin < "$PROJECT/std/.caos-expr")
entries="100644 blob $root_expr"$'\t'".caos-expr"$'\n'
for name in "${names[@]}"; do
  entries+="040000 tree ${undeepened[$name]}"$'\t'"$name"$'\n'
done
tree=$(printf '%s' "$entries" | git -C "$CLIENT" mktree)
# --force: refs/caos/std points at a tree, and git refuses to update a non-commit
# ref (or move it) without it. Re-publishing always replaces it.
git -C "$CLIENT" push -q --force caos "$tree:refs/caos/std"
# Record it locally too, so this repo can also resolve refs/caos/std.
git -C "$CLIENT" update-ref refs/caos/std "$tree"
echo "refs/caos/std -> $tree (published to $SERVER_URL)" >&2

# ---- seed records (design/caos-expr.md, Phase 3) ----------------------------
# The irreducible core can't be built by the machinery it IS, so bootstrap
# hand-builds each core artifact and publishes a SEED RECORD per item under
# refs/caos/seed; the core-seeder-runner registers one poll per record and
# answers the arg-tree key a caller forms, spawning no container.
#
#   refs/caos/seed -> tree { <name> -> { required: <JSON name→oid>, result: <obj> } }
#
# bootstrap HAND-ASSEMBLES each `required` from the deltas it already built —
# NOT by running the evaluator — so it needs no live answerer and no seeder
# during publish (the deltas are known; the key a caller's eval forms is
# reproduced here). `required` OMITS `std`/`salt`: a core item's output depends
# only on its `image`+`in`, so the seeder answers under ANY std — which is what
# lets the test harness hand each job a std SUBSET. The server matches a required
# set as a SUBSET of the job's arg entries (runner::matches), still far more
# specific than runnerd's empty required.
seed_entries=""
add_seed_record() { # <name> <required-json> <result-tree>
  local reqblob record
  reqblob=$(printf '%s' "$2" | git -C "$CLIENT" hash-object -w --stdin)
  record=$(printf '100644 blob %s\trequired\n040000 tree %s\tresult\n' \
    "$reqblob" "$3" | git -C "$CLIENT" mktree)
  seed_entries+="040000 tree $record"$'\t'"$1"$'\n'
  echo "seed: $1 -> $3" >&2
}

if [ -n "$fb_delta" ] && [ -n "${hash_of[flake-builder]:-}" ]; then
  # flake-builder: `run docker://seeded -- --in:@=.`. The `image` a caller forms
  # for a `docker://` ref is a BLOB naming it (image_arg_entry stores the bytes),
  # so required `image` is that blob's oid; `in` is the (deepened, but DEPS-free
  # so identity) flake-builder source entry.
  seeded_blob=$(printf 'docker://seeded' | git -C "$CLIENT" hash-object -w --stdin)
  add_seed_record flake-builder \
    "$(printf '{"image":"%s","in":"%s"}' "$seeded_blob" "${hash_of[flake-builder]}")" "$fb_delta"
fi

if [ -n "$cargo_delta" ] && [ -n "${hash_of[cargo]:-}" ] && [ -n "$fb_delta" ]; then
  # cargo: `run DEEP-DEPS/flake-builder -- --in:@=.`. Resolving the mount yields
  # the flake-builder image (fb_delta), so the caller's arg-tree `image` is that
  # tree oid and `in` is the deepened cargo entry — both known here.
  add_seed_record cargo \
    "$(printf '{"image":"%s","in":"%s"}' "$fb_delta" "${hash_of[cargo]}")" "$cargo_delta"
fi

# rustc/deep-deps: SEEDED core. Their `.caos-expr` is a distinct sentinel
# (`run docker://seeded-{rustc,deep-deps} -- --in:@=.`), so the caller's key is
# `{ image: <blob of that sentinel>, in: <the {.caos-expr} entry> }`. The RESULT
# is the hand-built curry over the runner pool — the shape these used to be
# published as directly, now a seed result instead of a staged-binary entry.
if [ -n "$runner_delta" ] && [ -n "${bin_path[deep-deps]:-}" ] && [ -n "${hash_of[deep-deps]:-}" ]; then
  install -m 755 "${bin_path[deep-deps]}/bin/worker-deep-deps" "$CLIENT/seed-deep-deps"
  git -C "$CLIENT" add seed-deep-deps
  dd_curry=$(cd "$CLIENT" && "$caos" curry "$runner_delta" -- "--worker1:@=seed-deep-deps")
  dd_blob=$(printf 'docker://seeded-deep-deps' | git -C "$CLIENT" hash-object -w --stdin)
  add_seed_record deep-deps \
    "$(printf '{"image":"%s","in":"%s"}' "$dd_blob" "${hash_of[deep-deps]}")" "$dd_curry"
fi

if [ -n "$runner_delta" ] && [ -n "$cargo_delta" ] && [ -n "${bin_path[rustc]:-}" ] && [ -n "${hash_of[rustc]:-}" ]; then
  install -m 755 "${bin_path[rustc]}/bin/worker-rustc" "$CLIENT/seed-rustc"
  git -C "$CLIENT" add seed-rustc
  rm -rf "${CLIENT:?}/seed-rustc-wc"
  cp -RL "$PROJECT/crates/worker-common" "$CLIENT/seed-rustc-wc"
  chmod -R u+w "$CLIENT/seed-rustc-wc"
  git -C "$CLIENT" add seed-rustc-wc
  # curry(runner, worker1=<rustc bin>, cargo=<cargo image, a literal hash blob>,
  # runner=<the same, for what rustc CURRIES ONTO>, worker-common=<tree>) — the
  # worker factory's ready-to-run form.
  #
  # `--runner` is bootstrap REALIZING std/rustc/DEPS: rustc depends on the runner
  # pool and curries every built binary onto it, so no caller passes one. A hash
  # literal like `--cargo`, because that is what a seed result can bind — when
  # rustc stops being seeded its expression binds DEEP-DEPS/runner instead.
  rustc_curry=$(cd "$CLIENT" && "$caos" curry "$runner_delta" -- \
    "--worker1:@=seed-rustc" "--cargo=$cargo_delta" "--runner=$runner_delta" \
    "--worker-common:@=seed-rustc-wc")
  rustc_blob=$(printf 'docker://seeded-rustc' | git -C "$CLIENT" hash-object -w --stdin)
  add_seed_record rustc \
    "$(printf '{"image":"%s","in":"%s"}' "$rustc_blob" "${hash_of[rustc]}")" "$rustc_curry"
fi

# runner: the pooled interpreter, a self-contained nix closure with no source to
# build from — so it is seeded exactly like flake-builder. Its checked-in entry
# is the `docker://seeded-runner` sentinel and the host-built delta is the seed
# RESULT. This is what gives `std/rustc/DEPS`'s `../runner` a directory to point
# at, and so what lets the tree deepen itself.
if [ -n "$runner_delta" ] && [ -n "${hash_of[runner]:-}" ]; then
  runner_blob=$(printf 'docker://seeded-runner' | git -C "$CLIENT" hash-object -w --stdin)
  add_seed_record runner \
    "$(printf '{"image":"%s","in":"%s"}' "$runner_blob" "${hash_of[runner]}")" "$runner_delta"
fi

if [ -n "$seed_entries" ]; then
  seed_tree=$(printf '%s' "$seed_entries" | git -C "$CLIENT" mktree)
  git -C "$CLIENT" push -q --force caos "$seed_tree:refs/caos/seed"
  git -C "$CLIENT" update-ref refs/caos/seed "$seed_tree"
  echo "refs/caos/seed -> $seed_tree" >&2
fi

echo "$tree"
