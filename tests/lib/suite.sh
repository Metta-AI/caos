#!/bin/bash
# THE test suite, as a caos worker (design/cargo-workers.md) — what the
# agent's `test` tool (caos-tools/test.sh) tail-calls. Runs in a bash worker
# on the outer stack. Its interface is a TOOL's interface: the workspace
# tree, and optionally an API key and a test filter — every script it runs
# comes from the workspace itself, so the suite tests exactly the harness
# the tree carries. Keyed on all of it: a full-suite cache hit means
# literally nothing changed; salt to force.
#
# Test = build + run tests, literally: run-then THE BUILD TOOL
# (caos-tools/build.sh — the same job an agent's `build` call fires, every
# stage shared in the cache), whose result is the artifact tree
# {report, bin/, images/}; then stage 3 fans out one job per
# tests/<name>/cli.sh over those artifacts, and summarize reports.
set -euo pipefail

caos get /cas/args/workspace
caos get /cas/args/workspace/tests
caos get /cas/args/workspace/tests/lib
caos get /cas/args/workspace/caos-tools
LIB=/cas/args/workspace/tests/lib

# The pruned tree — just what cargo reads — feeds the wrapper tests
# (cargo-self, unit), whose jobs must not re-key on non-Rust edits. The
# build tool prunes identically inside itself.
mkdir /tmp/build-ws
for e in Cargo.lock rust-toolchain.toml crates; do
  [ -e "/cas/args/workspace/$e" ] && ln -s "/cas/args/workspace/$e" "/tmp/build-ws/$e"
done
# Exclude caos-tui (a host TUI, not a caos worker — see caos-tools/build.sh)
# from the workspace the build + cargo-self/unit compile. build.sh prunes
# identically, so its cargo jobs land on the same keys.
caos get /cas/args/workspace/Cargo.toml
grep -v '"crates/caos-tui"' /cas/args/workspace/Cargo.toml > /tmp/build-ws/Cargo.toml
caos put /tmp/build-ws /cas/build-ws

build=$(caos curry /cas/std/bash -- \
  "--script:@=/cas/args/workspace/caos-tools/build.sh")

fwd=(
  "--build_ws:@=/cas/build-ws"
  "--workspace:@=/cas/args/workspace"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

stage3=$(caos curry /cas/std/bash -- "--script:@=$LIB/suite-stage3.sh" "${fwd[@]}")
caos run-then /cas/args/workspace -- --run="$build" --then="$stage3"
