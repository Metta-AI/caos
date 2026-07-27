#!/usr/bin/env bash
# THE generator for every checked-in redundancy under std/ (design/
# flake-images.md, part 2: literal trees). The build generates nothing:
# publish copies the checked-in trees, and this script is how the copies are
# (re)made from their sources of truth. `--check` runs the SAME generation
# and byte-compares instead of writing — one code path, so the check cannot
# drift from the generator. tests/std-lint runs the check in the suite.
#
# What it maintains:
#   std/{runner,bash,testenv,cargo}/flake.lock
#       derived from the ROOT flake.lock: the named input nodes copied
#       verbatim (locked revs + originals + follows edges) under a root
#       naming just those inputs — a std flake's pins structurally cannot
#       drift from the root's (std/cargo keeps the exact rustc that builds
#       caos; the caos-in-caos suite depends on that). Rerun on pin bumps.
#   std/testenv/worker
#       a copy of std/bash/worker (the source of truth): a flake reads only
#       its own tree, so each flake carries the script.
#   std/cargo/{Cargo.toml,Cargo.lock,rust-toolchain.toml,crates/**}
#       the workspace's manifests, lockfile, toolchain file, and EMPTY stubs
#       at each crate's real target paths (cargo and crane's mkDummySrc
#       detect autodiscovered targets by file presence) — no source, so a
#       source edit never re-keys std/cargo's toolchain bake.
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
mkdir -p "$tmp/std/bash" "$tmp/std/testenv" "$tmp/std/cargo"
derive_lock "$tmp/std/bash/flake.lock" nixpkgs
derive_lock "$tmp/std/testenv/flake.lock" nixpkgs
derive_lock "$tmp/std/cargo/flake.lock" nixpkgs rust-overlay crane

cp std/bash/worker "$tmp/std/testenv/worker"

cp Cargo.toml Cargo.lock rust-toolchain.toml "$tmp/std/cargo/"
# Every crate is a workspace member (Cargo.toml), so the glob is the member
# list. Two crates ride with REAL source — worker-cargo (std/cargo's
# /worker, compiled in-flake) and worker-common (its path dep) — so their
# edits re-key the tree and pay the one cold rebake. Everything else is a
# manifest plus empty stubs at each crate's real target paths: cargo only
# sees an autodiscovered target if its file EXISTS, and crane's mkDummySrc
# detects targets the same way, then copies its own dummy content over them
# — so presence is all that matters, never the source itself.
for m in crates/*/Cargo.toml; do
  c=$(dirname "$m")
  d="$tmp/std/cargo/crates/$(basename "$c")"
  mkdir -p "$d"
  cp "$m" "$d/"
  case "$(basename "$c")" in
  worker-cargo | worker-common)
    cp -R "$c/src" "$d/src"
    ;;
  *)
    for f in "$c"/src/main.rs "$c"/src/lib.rs "$c"/src/bin/*.rs; do
      [ -e "$f" ] || continue
      rel=${f#"$c"/}
      mkdir -p "$d/$(dirname "$rel")"
      : > "$d/$rel"
    done
    ;;
  esac
done

# The maintained set: the plain files, plus std/cargo/crates compared/replaced
# as a whole DIRECTORY so a crate deleted from the workspace fails the check
# as a stale vendored copy instead of lingering.
files="std/bash/flake.lock
std/testenv/flake.lock
std/cargo/flake.lock
std/testenv/worker
std/cargo/Cargo.toml
std/cargo/Cargo.lock
std/cargo/rust-toolchain.toml"

if [ "$check" = 1 ]; then
  fail=0
  while IFS= read -r f; do
    diff -u "$f" "$tmp/$f" || fail=1
  done <<< "$files"
  diff -ru std/cargo/crates "$tmp/std/cargo/crates" || fail=1
  if [ "$fail" != 0 ]; then
    echo "std/refresh.sh --check: checked-in std copies are stale (diffs above); run std/refresh.sh" >&2
    exit 1
  fi
  echo "std/refresh.sh --check: every checked-in std copy matches its source" >&2
else
  while IFS= read -r f; do
    cp "$tmp/$f" "$f"
  done <<< "$files"
  rm -rf std/cargo/crates
  cp -R "$tmp/std/cargo/crates" std/cargo/crates
  echo "std/refresh.sh: checked-in std copies rewritten" >&2
fi
