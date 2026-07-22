#!/bin/bash
# THE test suite, as a caos worker (design/cargo-workers.md, phases C/D) —
# what the agent's `test` tool (caos-tools/test.sh) tail-calls. Runs in a
# bash worker on the outer stack. Its interface is a TOOL's interface: the
# workspace tree, and optionally an API key and a test filter — every script
# it runs downstream comes from the workspace itself (caos-tools/*,
# tests/lib/*), so the suite tests exactly the harness the tree carries.
# Keyed on all of it: a full-suite cache hit means literally nothing
# changed; salt to force.
#
# The chain, no worker slot held between stages (continuations all the way):
#   1. (this script) prune the tree to what cargo reads and run-then THE
#      BUILD WORKER (caos-tools/build.sh — the same script, over the same
#      pruned tree, as an agent's `build` call: every cargo job shared);
#   2. stage 2: fan out the base-image builds (runner, bash, nix-builder);
#   3. stage 2b: bake the cargo toolchain base (nix, in the builder);
#   4. stage 2c: stack the cargo worker image onto it;
#   5. stage 3: fan out one job per tests/<name>/cli.sh;
#   6. summarize: the report + every test's complete record.
set -euo pipefail

caos get /cas/args/workspace
caos get /cas/args/workspace/tests
caos get /cas/args/workspace/tests/lib
caos get /cas/args/workspace/caos-tools
LIB=/cas/args/workspace/tests/lib

# The pruned tree — just what cargo reads — feeds the wrapper tests
# (cargo-self, unit), whose jobs must not re-key on non-Rust edits. The
# build worker prunes identically inside itself, so its cargo jobs land on
# the same keys whether fired here, by a host runner, or by an agent.
mkdir /tmp/build-ws
for e in Cargo.toml Cargo.lock rust-toolchain.toml crates; do
  [ -e "/cas/args/workspace/$e" ] && ln -s "/cas/args/workspace/$e" "/tmp/build-ws/$e"
done
caos put /tmp/build-ws /cas/build-ws

build=$(caos curry /cas/std/bash -- \
  "--script:@=/cas/args/workspace/caos-tools/build.sh")

fwd=(
  "--build_ws:@=/cas/build-ws"
  "--workspace:@=/cas/args/workspace"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

stage2=$(caos curry /cas/std/bash -- "--script:@=$LIB/suite-stage2.sh" "${fwd[@]}")
caos run-then /cas/args/workspace -- --run="$build" --then="$stage2"
