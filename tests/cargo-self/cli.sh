#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
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

# The caos workspace source, as git records it (tracked files only — target/,
# .caos-dev etc. are untracked or ignored and never land here).
mkdir ws
git -C "$CAOS_PROJECT" archive HEAD | tar -x -C ws
commit "caos workspace snapshot"

echo "== cargo check of the caos workspace, in a caos worker ==" >&2
t0=$(ms)
"$CAOS_CLI" run /cas/std/cargo r1 -- --tree:@=ws --cmd=check
t1=$(ms)
[ "$(cat r1/exit)" = "0" ] || fail "self-check failed: $(tail -c 2000 r1/stderr)"
took=$((t1 - t0))
echo "  ok: workspace checks clean (${took}ms)" >&2

# The deps-reuse tripwire: with the baked artifacts valid, a check compiles
# only the ~15 workspace crates (tens of seconds); a fingerprint regression
# recompiles ~170 deps and blows well past this. Generous for slow machines.
[ "$took" -lt 300000 ] || fail "self-check took ${took}ms — baked deps likely not reused"

echo "== per-crate (mode=all): cold, then a one-crate edit ==" >&2
t2=$(ms)
"$CAOS_CLI" run /cas/std/cargo r2 -- --tree:@=ws --cmd=check --mode=all
t3=$(ms)
[ "$(cat r2/exit)" = "0" ] || fail "mode=all check failed: $(tail -c 2000 r2/stderr)"
cold=$((t3 - t2))
echo "  ok: per-crate check clean (${cold}ms cold)" >&2

# Edit one leaf crate: its jobs (and orchestration) re-run; every other
# member's compile is a cache hit. The tripwire is wall-clock: an edit run
# far cheaper than the cold one. (The remaining cost is the orchestration
# jobs, which are whole-tree-keyed by design.)
echo "// tripwire edit" >> ws/crates/worker-rgrep/src/main.rs
commit "edit one crate"
t4=$(ms)
"$CAOS_CLI" run /cas/std/cargo r3 -- --tree:@=ws --cmd=check --mode=all
t5=$(ms)
[ "$(cat r3/exit)" = "0" ] || fail "edited mode=all check failed: $(tail -c 2000 r3/stderr)"
edit=$((t5 - t4))
echo "  ok: one-crate edit checked (${edit}ms vs ${cold}ms cold)" >&2
[ "$edit" -lt $((cold / 2)) ] \
  || fail "one-crate edit (${edit}ms) not much cheaper than cold (${cold}ms) — per-crate caching regressed"
