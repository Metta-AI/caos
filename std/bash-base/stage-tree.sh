#!/usr/bin/env bash
# Assemble std/bash-base's publishable tree: the flake + a lock derived from
# the main flake.lock (std/lib/derive-lock.sh — the pin cannot drift).
# Usage: stage-tree.sh <project root> <output dir>
set -euo pipefail
src=$1
out=$2

mkdir -p "$out"
cp "$src/std/bash-base/flake.nix" "$out/flake.nix"
"$src/std/lib/derive-lock.sh" "$src/flake.lock" "$out/flake.lock" nixpkgs
