#!/usr/bin/env bash
# THE generator for every checked-in redundancy under std/ (design/
# flake-images.md, part 2: literal trees). The build generates nothing:
# publish copies the checked-in trees, and this script is how the copies are
# (re)made from their sources of truth. `--check` runs the SAME generation
# and byte-compares instead of writing — one code path, so the check cannot
# drift from the generator. tests/std-lint runs the check in the suite.
#
# What it maintains:
#   std/{bash,cargo,flake-builder,git-runner,merge}/flake.lock
#       derived from the ROOT flake.lock: the named input nodes copied
#       verbatim (locked revs + originals + follows edges) under a root
#       naming just those inputs — a std flake's pins structurally cannot
#       drift from the root's (std/cargo keeps the exact rustc that builds
#       caos; the caos-in-caos suite depends on that). Rerun on pin bumps.
#
# std/cargo used to vendor the workspace's manifests, lockfile, toolchain
# file and per-crate target stubs here too — because a PUBLISHED flake tree
# is resolved from its own tree alone, so it had to be self-contained. Its
# image is now host-built and streamed from the root flake (which passes its
# own `src` and toolchain, so the bake IS cargoArtifacts rather than a second
# compile of the same crates), and a streamed image needs no such tree. Only
# the derived lock remains.
#
# Usage: std/refresh.sh          rewrite the checked-in copies
#        std/refresh.sh --check  verify them (a diff per mismatch, exit 1)
set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in
"") check=0 ;;
--check) check=1 ;;
*)
  echo "usage: std/refresh.sh [--check]" >&2
  exit 2
  ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

derive_lock() { # <out> <input name>...
  local out=$1
  shift
  local names
  names=$(printf '%s\n' "$@" | jq -R . | jq -sc .)
  jq --argjson names "$names" '
    .nodes as $n
    | ($names | map(select($n[.] == null))) as $missing
    | if ($missing | length) > 0 then
        error("input(s) \($missing | join(", ")) not in the main flake.lock")
      else
        {
          nodes: (
            ($names | map({ key: ., value: $n[.] }) | from_entries)
            + { root: { inputs: ($names | map({ key: ., value: . }) | from_entries) } }
          ),
          root: "root",
          version: 7
        }
      end
  ' flake.lock > "$out"
}

# Generate the whole redundant set under $tmp, mirroring the repo layout.
mkdir -p "$tmp/std/bash" "$tmp/std/cargo" "$tmp/std/flake-builder" "$tmp/std/git-runner" "$tmp/std/merge"
derive_lock "$tmp/std/bash/flake.lock" nixpkgs
derive_lock "$tmp/std/cargo/flake.lock" nixpkgs rust-overlay crane
derive_lock "$tmp/std/flake-builder/flake.lock" nixpkgs
derive_lock "$tmp/std/git-runner/flake.lock" nixpkgs
derive_lock "$tmp/std/merge/flake.lock" nixpkgs

# The maintained set.
files="std/bash/flake.lock
std/cargo/flake.lock
std/flake-builder/flake.lock
std/git-runner/flake.lock
std/merge/flake.lock"

if [ "$check" = 1 ]; then
  fail=0
  while IFS= read -r f; do
    diff -u "$f" "$tmp/$f" || fail=1
  done <<< "$files"
  if [ "$fail" != 0 ]; then
    echo "std/refresh.sh --check: checked-in std copies are stale (diffs above); run std/refresh.sh" >&2
    exit 1
  fi
  echo "std/refresh.sh --check: every checked-in std copy matches its source" >&2
else
  while IFS= read -r f; do
    cp "$tmp/$f" "$f"
  done <<< "$files"
  echo "std/refresh.sh: checked-in std copies rewritten" >&2
fi
