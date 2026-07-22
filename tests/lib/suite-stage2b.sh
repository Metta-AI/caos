#!/bin/bash
# Suite stage 2b (the `then` of the base-image builds): --children holds the
# runner/bash/nixbuilder digest refs. Assemble the BAKE TREE — flake files,
# manifests, lockfiles, and the crates' target entry points as empty files —
# and run-then the toolchain bake (suite-bake.sh, in the just-built
# nix-builder image) into stage 2c. See suite-bake.sh for why the empties:
# the bake's key must not move on source edits.
set -euo pipefail

caos get /cas/args/children
caos get /cas/args/workspace
caos get /cas/args/workspace/tests
caos get /cas/args/workspace/tests/lib
caos get -r /cas/args/workspace/crates
LIB=/cas/args/workspace/tests/lib

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

fwd=(
  "--build:@=/cas/args/build"
  "--build_ws:@=/cas/args/build_ws"
  "--workspace:@=/cas/args/workspace"
  "--runner_image:@=/cas/args/children/runner"
  "--bash_image:@=/cas/args/children/bash"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

caos get /cas/args/children/nixbuilder
bake=$(caos curry "docker://$(cat /cas/args/children/nixbuilder)" -- \
  "--script:@=$LIB/suite-bake.sh")
stage2c=$(caos curry /cas/std/bash -- "--script:@=$LIB/suite-stage2c.sh" "${fwd[@]}")
caos run-then /cas/bake-ws -- --run="$bake" --then="$stage2c"
