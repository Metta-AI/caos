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

# A GENUINELY cold run needs a salt. `ws` is a snapshot of $CAOS_PROJECT HEAD,
# which does not change between suite runs, so without one the "cold" run is
# served from the PREVIOUS suite's cache and both numbers below are fixed
# overhead. That is why this test has been flaky: it was often comparing two
# cache hits. One fresh salt, shared by both runs, so the first genuinely
# computes and the second genuinely reuses it. That the salt TOOK is checked
# comparatively (cold_hits << edit_hits) once both runs are in, not by
# demanding the cold run have exactly zero hits (intra-run diamond dedup makes
# that count jitter — see below).
CAOS_SALT="cargo-self-$(date +%s%N)"
export CAOS_SALT

echo "== per-crate (mode=all): cold, then a one-crate edit ==" >&2
t2=$(ms)
"$CAOS_CLI" run --trace=cold.trace r2 --base:@=DEEP-DEPS/cargo \
  --tree:@=ws --cmd=check --mode=all "--target=$tgt"
t3=$(ms)
[ "$(cat r2/exit)" = "0" ] || fail "mode=all check failed: $(tail -c 2000 r2/stderr)"
cold=$((t3 - t2))
cold_hits=$(grep -c '"cache_hit":true' cold.trace || true)
echo "  ok: per-crate check clean (${cold}ms cold, ${cold_hits} cache hits)" >&2
# The cold mode=all run checks every workspace crate, so it is also the
# deps-reuse tripwire. A fingerprint regression recompiles ~170 dependencies
# and blows well past this generous bound; a separate whole-workspace check
# tested the same property and only added another cargo job.
[ "$cold" -lt 300000 ] \
  || fail "per-crate self-check took ${cold}ms — baked deps likely not reused"
# NB: cold_hits is NOT asserted to be 0. The workspace DAG has diamonds
# (worker-common is a dependency of many members), so its single `job` is
# computed once and re-requested by every dependent. Whether a re-request lands
# as a single-flight WAITER (records no cache_hit) or, if the first finished and
# cached first, a redis cache_hit:true is pure timing — so a genuinely cold run
# legitimately shows a small, jittery number of intra-run dedup hits. Asserting
# ==0 was the flake. The real question — "did the salt take?" — is answered
# comparatively against the edit run below, where it has meaning and no race.

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
# worker-runner is a LEAF — nothing depends on it — so every other member's job
# should hit. The threshold is deliberately loose: the point is "most of the
# workspace was reused", not an exact count that would need editing whenever a
# crate is added.
echo "// tripwire edit" >> ws/crates/worker-runner/src/main.rs
commit "edit one crate"
t4=$(ms)
"$CAOS_CLI" run --trace=edit.trace r3 --base:@=DEEP-DEPS/cargo \
  --tree:@=ws --cmd=check --mode=all "--target=$tgt"
t5=$(ms)
[ "$(cat r3/exit)" = "0" ] || fail "edited mode=all check failed: $(tail -c 2000 r3/stderr)"
edit_hits=$(grep -c '"cache_hit":true' edit.trace || true)
echo "  ok: one-crate edit had ${edit_hits} cache hits (cold had ${cold_hits})" >&2
# The salt-took check, done comparatively and race-free. If the salt had NOT
# taken, the "cold" run would have been served wholesale from the previous
# suite's cache, so cold_hits would be the full cached-job count — at least as
# large as the edit run's reuse count. When the salt DID take, cold_hits is only
# the handful of intra-run diamond-dedup hits, well below edit_hits. So a cold
# run with FEWER hits than the reuse run is the honest signal, and it cannot be
# broken by the single-flight-vs-cache timing that broke the old ==0 assertion.
[ "$cold_hits" -lt "$edit_hits" ] \
  || fail "the cold run had ${cold_hits} cache hits vs ${edit_hits} in the edit run — the salt did not take, so nothing here is measuring what it claims"
[ "$edit_hits" -ge 8 ] \
  || fail "one-crate edit reused only ${edit_hits} cached jobs — per-crate caching regressed.
  Editing the leaf crate worker-runner should leave every other member's compile
  a cache hit; the cold run before it had ${cold_hits} hits by construction."
edit=$((t5 - t4))
# Reported, NOT asserted on. The ratio is worth seeing in the log and worthless
# as a gate (see above); the absolute bound is the one that still means
# something — a fingerprint regression that recompiles ~170 deps blows past it
# on any machine.
echo "  ok: one-crate edit checked (${edit}ms vs ${cold}ms cold)" >&2
[ "$edit" -lt 300000 ] \
  || fail "one-crate edit took ${edit}ms — the baked deps are not being reused"
