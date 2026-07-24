#!/usr/bin/env bash
# Derive a std flake's flake.lock from the MAIN flake.lock: the named input
# nodes copied verbatim (locked revs + originals + follows edges), under a
# root naming just those inputs. Run at publish (each std/<name>/stage-tree.sh)
# so a std flake's pins are structurally incapable of drifting from the root
# flake's — same nixpkgs, same rust-overlay, same crane, always.
#
# Usage: derive-lock.sh <main flake.lock> <out flake.lock> <input name>...
set -euo pipefail
main=$1
out=$2
shift 2

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
' "$main" > "$out"
