#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# `cargo clippy` over the workspace, per-crate (mode=all), so a lint re-runs
# for the touched crates and their dependents rather than the world. The
# workspace arrives in this test's wrapper (the pruned build tree — what
# cargo reads), staged as $CAOS_PROJECT.
#
# Clippy runs HERE rather than as a nix check because the suite is the only
# runner anyone invokes: `nix flake check` has no CI behind it, and clippy had
# no coverage at all until this. `-D warnings` is applied worker-side
# (worker-cargo), so a lint is a job failure, not a warning.
#
# One of four unit-* tests — see tests/unit-test/cli.sh for why they are four.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

mkdir ws
git -C "$CAOS_PROJECT" archive HEAD | tar -x -C ws
commit "workspace snapshot"

# Target musl: the one target the deps bake carries, so this reuses it
# instead of recompiling the dep graph.
tgt="$(uname -m)-unknown-linux-musl"

echo "== cargo clippy of the workspace, per-crate, in caos workers ==" >&2
ok=1
"$CAOS_CLI" run DEEP-DEPS/cargo r2 -- --tree:@=ws --cmd=clippy --mode=all \
  "--target=$tgt" >/tmp/r2.log 2>&1 || ok=0
cat /tmp/r2.log >&2
if [ "$ok" = 0 ] || [ ! -e r2/exit ] || [ "$(cat r2/exit)" != "0" ]; then
  echo "== cargo clippy FAILED (exit $(cat r2/exit 2>/dev/null)) — full output ==" >&2
  # STDOUT FIRST, STDERR LAST: clippy's diagnostics are on stderr, and the
  # suite report inlines the LAST lines of a failing test. The opposite of
  # unit-test, which puts stdout last for the same reason.
  echo "---- stdout ----" >&2; cat r2/stdout >&2 || true
  echo "---- stderr ----" >&2; cat r2/stderr >&2 || true
  if [ "$ok" = 0 ] || [ ! -e r2/exit ]; then infra "cargo worker did not run"; fi
  fail "clippy failed"
fi
echo "unit-clippy: ALL PASS" >&2
