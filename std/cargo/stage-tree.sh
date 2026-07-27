#!/usr/bin/env bash
# Assemble std/cargo's publishable BAKE TREE (design/flake-images.md): the
# CHECKED-IN files — the flake, its lock, and the vendored workspace inputs
# (manifests, Cargo.lock, rust-toolchain.toml, empty target stubs), all kept
# matching their sources of truth by std/refresh.sh and verified by
# tests/std-lint — plus the one thing not checked in: the nix-built
# worker-cargo binary as ./worker. No source rides along, so a source edit
# never re-keys the tree (the flake-builder's registry memo is
# flake-<caos hash of this tree>).
# Usage: stage-tree.sh <project root> <output dir> <bin store paths...>
set -euo pipefail
src=$1
out=$2
shift 2

mkdir -p "$out"
for f in flake.nix bake.nix flake.lock Cargo.toml Cargo.lock rust-toolchain.toml; do
  cp "$src/std/cargo/$f" "$out/$f"
done
cp -R "$src/std/cargo/crates" "$out/crates"
for p in "$@"; do
  [ -x "$p/bin/worker-cargo" ] && install -m 755 "$p/bin/worker-cargo" "$out/worker"
done
[ -e "$out/worker" ] || { echo "stage-tree: no worker-cargo among: $*" >&2; exit 1; }
# $src may be a read-only store copy (caosd); writable so the next publish's
# rm -rf works.
chmod -R u+w "$out"
