#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job (tests/lib/run-test.sh).
#
# Drives the std/merge worker directly (SPEC "Merging and conflict
# resolution"), the way tests/rgrep drives the fold before any chat wiring:
# build two commits with plain git, merge them through the worker, and assert
# the two-parent result commit — a conflicted merge (inline markers +
# .caos/conflicts) and a clean one.
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI), not just verify
# a merge some other way:
#
# The subject IS the std/merge worker, and a worker is not a library call or a
# checked-in artifact — it is a flake computation (std/merge is a `.caos-expr`
# plus a `worker` binary) that only ever exists as a JOB RUN against a server.
# There is no in-process entry point to call and no static output to lint; the
# merged commit M does not exist until the worker runs and produces it. The
# only way to make that happen is `caos-cli run --base:@=DEEP-DEPS/merge ...`,
# so the CLI is the essential and only path to the thing under test, not an
# incidental convenience — dropping it would leave nothing to assert against.
#
# Going through the client also pins the client-side half of a real merge
# invocation, on THIS build (the tested client, distinct in general from the
# host's — see run-test.sh): `caos-cli run` INGESTS the DEEP-DEPS/merge base
# and evaluates its expr, WALKS the arg tree's closure and PUSHES it to the
# inner server over git transport — here the closures of both input commits,
# `ours` and `theirs` — LAUNCHES the git-bearing worker, and STREAMS the result
# commit back as raw bytes (which is why run_merge hashes stdin and fetches M's
# closure). Every one of those steps is client behaviour a plain-git merge
# would never exercise. The worker in turn fetches both commit closures from
# the server's own transport, so this covers that path too, and the two
# identical runs assert the run is a content-addressed cache hit — a property
# that is only meaningful when a real client submits a real run.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
gc() { git -c user.email=test@caos -c user.name=caos "$@"; }
mkcommit() { # <tree> <message> [parent...] -> a commit minted with plain git
  local tree=$1 msg=$2; shift 2
  local ps=(); for p in "$@"; do ps+=(-p "$p"); done
  gc commit-tree "$tree" "${ps[@]}" -m "$msg"
}
# The merged commit M's hash from a std/merge run: run streams a commit result
# as raw bytes, so hash them and fetch M's closure into the local repo.
run_merge() { # <ours> <theirs> -> prints M, fetches it
  "$CAOS_CLI" run --base:@=DEEP-DEPS/merge --ours:commit="$1" --theirs:commit="$2" > m.commit \
    || fail "merge run failed"
  local m; m=$(git hash-object -t commit --stdin < m.commit)
  git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$m" || fail "fetch M ($m)"
  echo "$m"
}

echo "== build a base and two diverging sides ==" >&2
# base: f.txt (three lines), keep.txt (untouched by either side).
mkdir -p b
printf 'line1\nline2\nline3\n' > b/f.txt
printf 'keep\n' > b/keep.txt
gc add b; gc commit -qm scratch
base=$(mkcommit "$(git rev-parse HEAD:b)" base)

# ours: change line2 one way, add ours.txt (a clean, non-conflicting add).
mkdir -p o
printf 'line1\nOURS\nline3\n' > o/f.txt
printf 'keep\n' > o/keep.txt
printf 'o\n' > o/ours.txt
gc add o; gc commit -qm scratch2
ours=$(mkcommit "$(git rev-parse HEAD:o)" ours "$base")

# theirs: change the SAME line differently (→ conflict), add theirs.txt (clean).
mkdir -p t
printf 'line1\nTHEIRS\nline3\n' > t/f.txt
printf 'keep\n' > t/keep.txt
printf 't\n' > t/theirs.txt
gc add t; gc commit -qm scratch3
theirs=$(mkcommit "$(git rev-parse HEAD:t)" theirs "$base")

echo "== a conflicting merge: two parents, markers, .caos/conflicts ==" >&2
m=$(run_merge "$ours" "$theirs")
[ "$(git rev-parse "$m^")" = "$ours" ] || fail "M's first parent is not ours"
[ "$(git rev-parse "$m^2")" = "$theirs" ] || fail "M's second parent is not theirs"
[ "$(git show -s --format=%ct "$m")" = "$(git show -s --format=%ct "$ours")" ] \
  || fail "M did not inherit ours' timestamp"
[ "$(git show -s --format=%ct "$m")" -gt 0 ] || fail "M still has an epoch-zero date"

# The conflicted file carries git's inline markers and BOTH sides' text.
f=$(git show "$m:f.txt")
printf '%s' "$f" | grep -q '<<<<<<<' || fail "no conflict marker in f.txt:
$f"
printf '%s' "$f" | grep -q 'OURS' || fail "ours side missing from f.txt"
printf '%s' "$f" | grep -q 'THEIRS' || fail "theirs side missing from f.txt"

# The clean adds from each side both survive; the untouched file too.
[ "$(git show "$m:ours.txt")" = "o" ] || fail "ours.txt not merged in"
[ "$(git show "$m:theirs.txt")" = "t" ] || fail "theirs.txt not merged in"
[ "$(git show "$m:keep.txt")" = "keep" ] || fail "untouched keep.txt changed"

# .caos/conflicts is the authoritative set — git's unmerged notation, naming
# the conflicted path (and only it).
conflicts=$(git show "$m:.caos/conflicts") || fail "M has no .caos/conflicts"
printf '%s' "$conflicts" | grep -q 'f.txt' || fail ".caos/conflicts does not name f.txt:
$conflicts"
printf '%s' "$conflicts" | grep -q 'ours.txt' && fail ".caos/conflicts names the clean ours.txt"
echo "  ok: [ours, theirs] parents; markers in f.txt; .caos/conflicts lists f.txt" >&2

echo "== an identical merge is a cache hit ==" >&2
m2=$(run_merge "$ours" "$theirs")
[ "$m2" = "$m" ] || fail "identical merge produced a different commit ($m2 vs $m)"
echo "  ok: same two commits -> same M" >&2

echo "== a clean merge: two parents, no .caos/conflicts ==" >&2
# theirs-clean: touches only theirs.txt, never f.txt, so merging with ours is
# conflict-free.
mkdir -p tc
printf 'line1\nline2\nline3\n' > tc/f.txt
printf 'keep\n' > tc/keep.txt
printf 'tc\n' > tc/theirs.txt
gc add tc; gc commit -qm scratch4
theirs_clean=$(mkcommit "$(git rev-parse HEAD:tc)" theirs-clean "$base")

mc=$(run_merge "$ours" "$theirs_clean")
[ "$(git rev-parse "$mc^")" = "$ours" ] || fail "clean M's first parent is not ours"
[ "$(git rev-parse "$mc^2")" = "$theirs_clean" ] || fail "clean M's second parent wrong"
git rev-parse -q --verify "$mc:.caos" >/dev/null && fail "clean merge left a .caos entry"
[ "$(git show "$mc:f.txt")" = "$(printf 'line1\nOURS\nline3')" ] \
  || fail "clean merge did not keep ours' f.txt"
[ "$(git show "$mc:ours.txt")" = "o" ] || fail "clean merge dropped ours.txt"
[ "$(git show "$mc:theirs.txt")" = "tc" ] || fail "clean merge dropped theirs.txt"
echo "  ok: clean merge is a pure two-parent commit, no .caos/conflicts" >&2

echo "merge: ALL PASS" >&2
