#!/bin/bash
# Build stage 2b (the `then` of the base-image builds): --children holds the
# runner/bash/nixbuilder digest refs. Assemble the BAKE TREE — flake files,
# manifests, lockfiles, and the crates' target entry points as empty files
# (crane's dummy-source pass needs the paths; empty keeps the bake's key
# independent of source content) — and run-then the toolchain bake
# (lib/bake.sh, in the just-built nix-builder) into stage 2c.
set -euo pipefail

caos get /cas/args/children
caos get /cas/args/workspace
caos get /cas/args/workspace/caos-tools
caos get /cas/args/workspace/caos-tools/lib
caos get -r /cas/args/workspace/crates
LIB=/cas/args/workspace/caos-tools/lib

mkdir /tmp/bake-ws
for f in flake.nix flake.lock rust-toolchain.toml Cargo.toml Cargo.lock; do
  [ -e "/cas/args/workspace/$f" ] && ln -s "/cas/args/workspace/$f" "/tmp/bake-ws/$f"
done
mkdir /tmp/bake-ws/crates
for d in /cas/args/workspace/crates/*/; do
  c=$(basename "$d")
  mkdir -p "/tmp/bake-ws/crates/$c"
  ln -s "$d/Cargo.toml" "/tmp/bake-ws/crates/$c/Cargo.toml"
  for f in src/main.rs src/lib.rs build.rs; do
    if [ -e "$d$f" ]; then
      mkdir -p "/tmp/bake-ws/crates/$c/$(dirname "$f")"
      : > "/tmp/bake-ws/crates/$c/$f"
    fi
  done
  for b in "$d"src/bin/*.rs; do
    [ -e "$b" ] || continue
    mkdir -p "/tmp/bake-ws/crates/$c/src/bin"
    : > "/tmp/bake-ws/crates/$c/src/bin/$(basename "$b")"
  done
done
caos put /tmp/bake-ws /cas/bake-ws

caos get /cas/args/children/nixbuilder
bake=$(caos curry "docker://$(cat /cas/args/children/nixbuilder)" -- \
  "--script:@=$LIB/bake.sh")
stage2c=$(caos curry /cas/std/bash -- "--script:@=$LIB/build-stage2c.sh" \
  "--workspace:@=/cas/args/workspace" "--bin:@=/cas/args/bin" \
  "--runner_image:@=/cas/args/children/runner" \
  "--bash_image:@=/cas/args/children/bash")
caos run-then /cas/bake-ws -- --run="$bake" --then="$stage2c"
