#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# The dogfood: `cargo check` of the caos workspace ITSELF, in a caos worker
# (design/cargo-workers.md). This is what an agent's `build` tool runs on
# every edit, so it's the one that matters: the baked deps must be reused
# (a check that recompiles 170 deps blows the whole point — and the time
# budget below is the tripwire for that regression), and the workspace's own
# crates must compile against them.
#
# The workspace source is ingested from $CAOS_PROJECT via a git snapshot —
# exactly the tree an agent's conversation would carry.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; } # epoch milliseconds
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# Target musl: that is the one target the deps bake carries, so a check
# reuses it instead of recompiling the dep graph. A host build has no bake
# to reuse and no consumer — every test runs in the Linux stack, where musl
# statics run fine.
tgt="$(uname -m)-unknown-linux-musl"

# The caos workspace source, as git records it (tracked files only — target/,
# .caos-dev etc. are untracked or ignored and never land here).
mkdir ws
git -C "$CAOS_PROJECT" archive HEAD | tar -x -C ws
commit "caos workspace snapshot"

echo "== cargo check of the caos workspace, in a caos worker ==" >&2
t0=$(ms)
"$CAOS_CLI" run /cas/std/cargo r1 -- --tree:@=ws --cmd=check "--target=$tgt"
t1=$(ms)
[ "$(cat r1/exit)" = "0" ] || fail "self-check failed: $(tail -c 2000 r1/stderr)"
took=$((t1 - t0))
echo "  ok: workspace checks clean (${took}ms)" >&2

# The deps-reuse tripwire: with the baked artifacts valid, a check compiles
# only the ~15 workspace crates (tens of seconds); a fingerprint regression
# recompiles ~170 deps and blows well past this. Generous for slow machines.
[ "$took" -lt 300000 ] || fail "self-check took ${took}ms — baked deps likely not reused"

# A GENUINELY cold run needs a salt. `ws` is a snapshot of $CAOS_PROJECT HEAD,
# which does not change between suite runs, so without one the "cold" run is
# served from the PREVIOUS suite's cache and both numbers below are fixed
# overhead. That is why this test has been flaky: it was often comparing two
# cache hits. One fresh salt, shared by both runs, so the first genuinely
# computes and the second genuinely reuses it.
CAOS_SALT="cargo-self-$(date +%s%N)"
export CAOS_SALT

echo "== per-crate (mode=all): cold, then a one-crate edit ==" >&2
t2=$(ms)
"$CAOS_CLI" run --trace=cold.trace /cas/std/cargo r2 -- \
  --tree:@=ws --cmd=check --mode=all "--target=$tgt"
t3=$(ms)
[ "$(cat r2/exit)" = "0" ] || fail "mode=all check failed: $(tail -c 2000 r2/stderr)"
cold=$((t3 - t2))
cold_hits=$(grep -c '"cache_hit":true' cold.trace || true)
echo "  ok: per-crate check clean (${cold}ms cold, ${cold_hits} cache hits)" >&2
[ "$cold_hits" -eq 0 ] || fail "the cold run had ${cold_hits} cache hits — the salt did not take, so nothing below is measuring what it claims"

# Edit one leaf crate: its jobs (and the whole-tree-keyed orchestration)
# re-run; every other member's compile is a CACHE HIT, and that is what this
# asserts — structurally, out of the trace, not by stopwatch.
#
# It used to compare wall clocks: edit < 3/4 * cold. That measures what
# FRACTION of the run is compiling, not whether caching works, so every
# improvement to compile speed pushed the ratio toward 1 and failed the test.
# It broke twice in one day that way — once when dependencies got optimized,
# once when the bake started warming per-member feature resolutions (which cut
# the cold run from ~25s to ~3s and left the ratio at 0.93 with caching
# working perfectly). A count of cache hits cannot be broken by making things
# faster.
#
# worker-rgrep is a LEAF — nothing depends on it — so every other member's job
# should hit. The threshold is deliberately loose: the point is "most of the
# workspace was reused", not an exact count that would need editing whenever a
# crate is added.
echo "// tripwire edit" >> ws/crates/worker-rgrep/src/main.rs
commit "edit one crate"
t4=$(ms)
"$CAOS_CLI" run --trace=edit.trace /cas/std/cargo r3 -- \
  --tree:@=ws --cmd=check --mode=all "--target=$tgt"
t5=$(ms)
[ "$(cat r3/exit)" = "0" ] || fail "edited mode=all check failed: $(tail -c 2000 r3/stderr)"
edit_hits=$(grep -c '"cache_hit":true' edit.trace || true)
echo "  ok: one-crate edit had ${edit_hits} cache hits (cold had ${cold_hits})" >&2
[ "$edit_hits" -ge 8 ] \
  || fail "one-crate edit reused only ${edit_hits} cached jobs — per-crate caching regressed.
  Editing the leaf crate worker-rgrep should leave every other member's compile
  a cache hit; the cold run before it had ${cold_hits} hits by construction."
edit=$((t5 - t4))
# Reported, NOT asserted on. The ratio is worth seeing in the log and worthless
# as a gate (see above); the absolute bound is the one that still means
# something — a fingerprint regression that recompiles ~170 deps blows past it
# on any machine.
echo "  ok: one-crate edit checked (${edit}ms vs ${cold}ms cold)" >&2
[ "$edit" -lt 300000 ] \
  || fail "one-crate edit took ${edit}ms — the baked deps are not being reused"
