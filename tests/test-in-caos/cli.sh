#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# Caos-in-caos (design/cargo-workers.md, phase 3): a whole inner caos stack —
# the edited server + a process-mode runnerd with chroot slots — runs INSIDE
# a caos worker (the testenv image, whose CAOS_WORKER_UID=0 grant is the
# per-image containment decision that makes it possible), driving a recursive
# rgrep fold through the inner stack. The job is keyed on (script, built
# binaries), so the second run is a cache hit: a test whose inputs didn't
# change never re-runs — the tests-as-jobs thesis, demonstrated on itself.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

echo "== building the inner-stack binaries ==" >&2
nix build "$CAOS_PROJECT#server" -o srv
nix build "$CAOS_PROJECT#runnerd" -o rnd
nix build "$CAOS_PROJECT#caos" -o cs
nix build "$CAOS_PROJECT#worker-runner" -o wr
nix build "$CAOS_PROJECT#worker-rgrep" -o rg

mkdir -p bins
cp -L srv/bin/server rnd/bin/runnerd cs/bin/caos cs/bin/caos-cli \
  wr/bin/worker-runner rg/bin/worker-rgrep bins/
commit "inner-stack binaries"

echo "== the inner stack as a caos worker job ==" >&2
t0=$(ms)
"$CAOS_CLI" run /cas/std/testenv r1 -- --script:@=test/inner-caos.sh --bins:@=bins
t1=$(ms)
grep -q "INNER-STACK: ALL PASS" r1 || fail "inner stack did not pass: $(cat r1)"
echo "  ok: caos-in-caos ($((t1 - t0))ms)" >&2

echo "== identical inputs: the test never re-runs ==" >&2
t2=$(ms)
"$CAOS_CLI" run /cas/std/testenv r2 -- --script:@=test/inner-caos.sh --bins:@=bins
t3=$(ms)
cmp -s r1 r2 || fail "cached verdict differs"
echo "  ok: cache hit ($((t3 - t2))ms vs $((t1 - t0))ms)" >&2
