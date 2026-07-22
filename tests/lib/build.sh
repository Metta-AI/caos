#!/bin/bash
# THE build worker: pruned workspace tree in (--in, run-then), bin tree out.
# Runs std/cargo (--cmd=build, musl release — the old, known-good caos
# building the edited tree) and strips the result to the content-stable bin
# tree (strip-bins.sh: cargo's stderr carries volatile timings; nothing
# downstream may key on it). A build FAILURE fails this job loudly, with
# the compiler output on stderr — callers get a working bin tree or an
# error, never a half-result.
#
# The suite's stage 1 runs it; the agent's `build` tool is the same worker
# over the conversation's tree.
set -euo pipefail

cargo=$(caos curry /cas/std/cargo -- --cmd=build \
  "--target:@=/cas/args/target" --profile=release)
strip=$(caos curry /cas/std/bash -- "--script:@=/cas/args/strip")
caos run-then /cas/args/in -- --run="$cargo" --then="$strip"
