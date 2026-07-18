#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# The suite-as-a-cached-job (design/cargo-workers.md, phase 3): a multi-worker
# smoke suite — file-count, deep-deps, dirs-only, rgrep — runs inside a nested
# process-mode caos stack, as ONE caos worker job keyed on (script, built
# binaries). The inner harness publishes its own std from those binaries
# (curry(dummy, bin), no images), so the core engine (map-then/run-then
# promises, self-recursion, sparse trees) is exercised end to end with no
# docker, no registry, no nix inside. An identical re-run is a cache hit — the
# whole inner suite becomes free when nothing changed, which is the point of
# running tests as caos jobs.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

echo "== building the inner-stack binaries ==" >&2
nix build "$CAOS_PROJECT#server" -o srv
nix build "$CAOS_PROJECT#runnerd" -o rnd
nix build "$CAOS_PROJECT#caos" -o cs
nix build "$CAOS_PROJECT#worker-runner" -o wr
nix build "$CAOS_PROJECT#worker-file-count" -o wfc
nix build "$CAOS_PROJECT#worker-dirs-only" -o wdo
nix build "$CAOS_PROJECT#worker-deep-deps" -o wdd
nix build "$CAOS_PROJECT#worker-rgrep" -o wrg

mkdir -p bins
cp -L srv/bin/server rnd/bin/runnerd cs/bin/caos cs/bin/caos-cli wr/bin/worker-runner \
  wfc/bin/worker-file-count wdo/bin/worker-dirs-only wdd/bin/worker-deep-deps \
  wrg/bin/worker-rgrep bins/
commit "inner-stack binaries"

echo "== the inner suite as a caos worker job ==" >&2
t0=$(ms)
"$CAOS_CLI" run /cas/std/testenv r1 -- --script:@=test/inner-suite.sh --bins:@=bins
t1=$(ms)
grep -q "SUITE-IN-CAOS: ALL PASS" r1 || fail "inner suite did not pass: $(cat r1)"
echo "  ok: suite-in-caos ($((t1 - t0))ms)" >&2

echo "== identical inputs: the suite never re-runs ==" >&2
t2=$(ms)
"$CAOS_CLI" run /cas/std/testenv r2 -- --script:@=test/inner-suite.sh --bins:@=bins
t3=$(ms)
cmp -s r1 r2 || fail "cached verdict differs"
echo "  ok: cache hit ($((t3 - t2))ms vs $((t1 - t0))ms)" >&2
