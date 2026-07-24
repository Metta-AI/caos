#!/usr/bin/env bash
# Assemble std/cargo's publishable BAKE TREE (design/flake-images.md): the
# flake next to this script plus exactly what its build reads — manifests,
# lockfile, toolchain file, the worker-cargo binary as ./worker — and NO
# source, so a source edit never re-keys the tree (the flake-builder's
# registry memo is flake-<caos hash of this tree>).
#
# The flake.lock is DERIVED from the main flake.lock here, at publish: the
# same nixpkgs/rust-overlay/crane revisions, so the flake's toolchain
# expression resolves the exact rustc that builds caos (the caos-in-caos
# suite needs the versions to match) and the pin structurally cannot drift.
#
# Usage: stage-tree.sh <project root> <output dir> <bin store paths...>
set -euo pipefail
src=$1
out=$2
shift 2

mkdir -p "$out"
cp "$src/std/cargo/flake.nix" "$out/flake.nix"
cp "$src/std/cargo/bake.nix" "$out/bake.nix"
for p in "$@"; do
  [ -x "$p/bin/worker-cargo" ] && install -m 755 "$p/bin/worker-cargo" "$out/worker"
done
[ -e "$out/worker" ] || { echo "stage-tree: no worker-cargo among: $*" >&2; exit 1; }

# The derived lock: the main lock's three nodes verbatim (locked + original +
# rust-overlay's nixpkgs follows), under a root naming just those inputs.
"$src/std/lib/derive-lock.sh" "$src/flake.lock" "$out/flake.lock" \
  nixpkgs rust-overlay crane

cp "$src/rust-toolchain.toml" "$out/"
cp "$src/Cargo.toml" "$src/Cargo.lock" "$out/"
# Every crate is a workspace member (Cargo.toml), so the glob is the member
# list. Manifests plus empty stubs at each crate's real target paths: cargo
# only sees an autodiscovered target if its file EXISTS, and crane's
# mkDummySrc detects targets the same way, then copies its own dummy content
# over them — so presence is all that matters, never the source itself.
for m in "$src"/crates/*/Cargo.toml; do
  c=$(dirname "$m")
  d="$out/crates/$(basename "$c")"
  mkdir -p "$d"
  cp "$m" "$d/"
  for f in "$c"/src/main.rs "$c"/src/lib.rs "$c"/src/bin/*.rs; do
    [ -e "$f" ] || continue
    rel=${f#"$c"/}
    mkdir -p "$d/$(dirname "$rel")"
    : > "$d/$rel"
  done
done
