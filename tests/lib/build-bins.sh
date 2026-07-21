#!/bin/bash
# Runs INSIDE a bash worker on the OUTER stack: build the caos workspace's
# binaries with std/cargo — the old, known-good caos building the edited tree
# (design/cargo-workers.md, bootstrap) — and return JUST the bin tree.
#
# The two-step run-then exists for cache honesty: the cargo result carries
# volatile stderr (cargo prints timings), so anything keyed on the whole
# result would re-key on every rebuild. The strip step (strip-bins.sh)
# reduces it to the bin tree, which is content-stable when the binaries are —
# so downstream jobs keyed on `--bins:tree=<this result>` hit whenever the
# code they exercise is unchanged.
set -euo pipefail

caos get /cas/args/target
target=$(cat /cas/args/target)
cargo=$(caos curry /cas/std/cargo -- --cmd=build "--target=$target" --profile=release)
strip=$(caos curry /cas/std/bash -- --script:@=/cas/args/strip)
caos run-then /cas/args/workspace -- --run="$cargo" --then="$strip"
