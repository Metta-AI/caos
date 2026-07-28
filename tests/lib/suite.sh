#!/bin/bash
# THE test suite, as a caos worker (design/test-stack-image.md) — what the
# agent's `test` tool (caos-tools/test.sh) tail-calls. Runs in a bash worker
# on the outer stack. Its interface is a TOOL's interface: the workspace
# tree, and optionally an API key and a test filter — every script it runs
# comes from the workspace itself, so the suite tests exactly the harness the
# tree carries. Keyed on all of it: a full-suite cache hit means literally
# nothing changed; salt to force.
#
# Test = build + run tests, literally: run-then THE BUILD TOOL
# (caos-tools/build.sh — the same job an agent's `build` call fires, sharing
# its cache), whose result is the TEST STACK IMAGE; then stage 3 fans out one
# job per tests/<name>/cli.sh, each running that image with the per-test
# runner as worker1, and summarize reports.
#
# There is no --bins anymore: the tree under test is compiled from source
# inside the build job, so nothing crosses from the host but the tree.
set -euo pipefail

caos get /cas/args/workspace
caos get /cas/args/workspace/tests
caos get /cas/args/workspace/tests/lib
caos get /cas/args/workspace/caos-tools
LIB=/cas/args/workspace/tests/lib

# The pruned tree — just what cargo reads — feeds the wrapper tests
# (cargo-self, unit), whose jobs must not re-key on non-Rust edits.
mkdir /tmp/build-ws
for e in Cargo.toml Cargo.lock rust-toolchain.toml crates; do
  [ -e "/cas/args/workspace/$e" ] && ln -s "/cas/args/workspace/$e" "/tmp/build-ws/$e"
done
caos put /tmp/build-ws /cas/build-ws

build=$(caos curry /cas/std/bash -- \
  "--worker1:@=/cas/args/workspace/caos-tools/build.sh")

fwd=(
  "--build_ws:@=/cas/build-ws"
  "--workspace:@=/cas/args/workspace"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

stage3=$(caos curry /cas/std/bash -- "--worker1:@=$LIB/suite-stage3.sh" "${fwd[@]}")
caos run-then /cas/args/workspace -- --run="$build" --then="$stage3"
