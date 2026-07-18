#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# The suite as PER-TEST caos jobs (design/cargo-workers.md, phase 3): for each
# curry-able test (file-count, dirs-only, deep-deps, rgrep), fire one testenv
# job that runs that test's REAL cli.sh inside a nested process-mode stack —
# keyed on (run-test.sh, that test's tree, the built binaries). A second pass
# is all cache hits, and editing one test's fixtures would re-run only its
# job. This is "each tests/<name> becomes one job, and a test whose inputs
# didn't change never re-runs" — on the tests that don't need an image-based
# worker (the bash script-worker and toolchain tests await the podman
# backend).
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

CASES=(file-count dirs-only deep-deps rgrep)

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

# Bring each case's real test dir into the client repo so caos-cli can ingest
# it (it hashes git-tracked paths only). They live beside us in the source
# tree, reached via CAOS_PROJECT.
mkdir -p cases
for c in "${CASES[@]}"; do
  cp -r "$CAOS_PROJECT/tests/$c" "cases/$c"
done
commit "binaries + cases"

run_case() { # <name> <result-dir> -> runs its cli.sh as a testenv job
  "$CAOS_CLI" run /cas/std/testenv "$2" \
    -- --script:@=test/run-test.sh --test:@="cases/$1" --bins:@=bins
}

echo "== each case as its own caos job ==" >&2
t0=$(ms)
for c in "${CASES[@]}"; do
  run_case "$c" "r-$c"
  grep -q "RUN-TEST: PASS" "r-$c" || fail "$c did not pass"
  echo "  ok: $c" >&2
done
t1=$(ms)
echo "  all cases passed ($((t1 - t0))ms cold)" >&2

echo "== second pass: every case is a cache hit ==" >&2
t2=$(ms)
for c in "${CASES[@]}"; do
  run_case "$c" "r2-$c"
  cmp -s "r-$c" "r2-$c" || fail "$c cached verdict differs"
done
t3=$(ms)
echo "  ok: cached ($((t3 - t2))ms vs $((t1 - t0))ms cold)" >&2
# The cached pass must be dramatically cheaper — proof the jobs memoized.
[ "$((t3 - t2))" -lt "$(( (t1 - t0) / 4 ))" ] \
  || fail "cached pass ($((t3 - t2))ms) not much cheaper than cold ($((t1 - t0))ms)"
