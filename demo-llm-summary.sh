#!/usr/bin/env bash
# Demo: incremental "LLM" summarization of a document tree, driven through the
# built-ins (`/cas/std`). It folds a per-file summary up into directory
# summaries — and shows that editing one file recomputes only that file's
# summary and the directory summaries on the path to the root; every sibling is
# a cache hit.
#
# Summaries are REAL Anthropic calls when ANTHROPIC_API_KEY is set in *this
# script's* environment — it's passed to the worker as the --key argument (see
# "Real summaries" below). With no key the worker uses a deterministic local
# stand-in, so this demo runs anywhere — no key, no egress. The incrementality is
# identical either way — and caching even makes a nondeterministic model
# byte-reproducible here: an unchanged node is a cache hit, never re-sampled.
#
# The result mirrors the input tree: every node is `{ summary, <children…> }`,
# so `<out>/summary` summarizes the whole tree and `<out>/guide/intro.md/summary`
# is one file's.
#
# What it shows:
#   A. correctness — a tree of summaries, root summarizing the whole thing.
#   B. memoization — an identical re-run spawns no worker (0 cache misses). This
#      is also the "a teammate inherits your cache" property.
#   C. Merkle incrementality — edit ONE file and only its branch recomputes: the
#      untouched sibling directory is byte-identical, and even the unedited file
#      in the *same* directory is reused; misses ≪ the cold run.
#
# Requires the dev daemons running (`tilt up`, or `nix run .#caosd`): the caos
# server :9090, redis, registry — and a docker the server can reach.
#
# Real summaries: export your key before running this script; it's passed to the
# worker as `--key` (which then rides in the content-addressed request — stored
# in the CAS and folded into the cache key):
#   export ANTHROPIC_API_KEY=sk-ant-...
#   ./demo-llm-summary.sh
# The worker container also needs outbound HTTPS to api.anthropic.com.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}

# A per-run salt threads into every request (hence every cache key), so this
# run's cache entries can't collide with any other run's — runs are fully
# independent without ever clearing Redis. Constant within this run, so the
# cache hit/miss assertions below still hold.
export CAOS_SALT="${CAOS_SALT:-$(date +%s%N)-$$}"

# Publish the built-ins this demo needs (fold + llm-summary) to the server, then
# build the user-facing client.
echo "building caos client + publishing caos/std (fold, llm-summary)..." >&2
nix build .#caos-cli -o result-caos
./build-builtins.sh fold llm-summary >/dev/null
caosbin=$PROJECT/result-caos/bin/caos-cli

# A client working repo with the server as its `caos` remote — the shape a user
# has. `caos-cli` (below) runs from inside it, so its git transport finds it.
CLIENT=$PROJECT/.caos-dev/demo-summary-client
rm -rf "$CLIENT"; git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "$CAOS_SERVER_URL"
caos-cli() { ( cd "$CLIENT" && "$caosbin" "$@" ); }

# CAS must live on an xattr-capable fs (caos records each path's hash in
# user.caos.hash); the repo's fs qualifies, /tmp may not.
CAS=$PROJECT/.caos-dev/demo-summary-cas
DOCS=$(mktemp -d)
SNAP=$(mktemp -d)
rm -rf "$CAS"; mkdir -p "$CAS"
export CAOS_CAS_DIR=$CAS
trap 'rm -rf "$CAS" "$DOCS" "$SNAP" "$CLIENT"' EXIT

# Fetch the published std library, then materialize it so we can run the
# llm-summary built-in by path. (`caos run` independently resolves caos/std and
# threads it in as the run's `std`.)
git -C "$CLIENT" fetch -q caos '+refs/caos/std:refs/caos/std'
caos-cli get-hash "$(caos-cli resolve refs/caos/std)" "$CAS/std" >/dev/null
IMG="$CAS/std/llm-summary"

fail() { echo "FAIL: $*" >&2; exit 1; }
misses_since() { docker logs --since "$1" caos-server 2>&1 | grep -c "cache miss:" || true; }

# Fixture: a small document tree.
#   DOCS/README.md
#   DOCS/guide/{intro.md,setup.md}
#   DOCS/api/{reference.md,errors.md}
mkdir -p "$DOCS/guide" "$DOCS/api"
printf 'caos\n\nContent-addressed storage and compute over git objects.\n' > "$DOCS/README.md"
printf '# Intro\n\nWelcome to the guide. Start here to learn the basics.\n'  > "$DOCS/guide/intro.md"
printf '# Setup\n\nInstall Nix and Docker, then run tilt up.\n'              > "$DOCS/guide/setup.md"
printf '# Reference\n\nThe object, run, and git endpoints.\n'                > "$DOCS/api/reference.md"
printf '# Errors\n\n400 malformed, 404 absent, 500 internal.\n'             > "$DOCS/api/errors.md"

# Pass the API key to the worker as --key when one is set, else run keyless (the
# worker's local stand-in). A key makes every summary a real Anthropic call.
KEY_ARG=()
[ -n "${ANTHROPIC_API_KEY:-}" ] && KEY_ARG=(--key="$ANTHROPIC_API_KEY")

# Summarize the whole tree and materialize the result. ELAPSED is whole seconds
# (portable: bash's SECONDS, so no GNU `date +%N` dependency on macOS).
run() {
  rm -rf "$CAS/out"
  SECONDS=0
  caos-cli run "$IMG" "$CAS/out" -- --in:@="$DOCS" "${KEY_ARG[@]}" >/dev/null
  ELAPSED=$SECONDS
  caos-cli get -r "$CAS/out" >/dev/null
}

echo "== Phase A: cold run — summarize the whole tree ==" >&2
since=$(date +%s)
run
[ -f "$CAS/out/summary" ]                  || fail "no root summary produced"
[ -f "$CAS/out/guide/intro.md/summary" ]   || fail "no per-file summary produced"
[ -f "$CAS/out/api/reference.md/summary" ] || fail "no per-file summary produced"
cold=$(misses_since "$since")
echo "  ok: summarized in ${ELAPSED}s, ${cold} cache misses (one per node, cold)" >&2
echo "  --- root summary (caos get out/summary) ---" >&2
sed 's/^/      /' "$CAS/out/summary" >&2
echo "  --- one file's summary (out/guide/intro.md/summary) ---" >&2
sed 's/^/      /' "$CAS/out/guide/intro.md/summary" >&2

# Snapshot the full summary tree to compare against after the edit.
cp -a "$CAS/out/." "$SNAP/"

echo "== Phase B: identical re-run is a full cache hit (memoization) ==" >&2
sleep 1; since=$(date +%s)   # gap so Phase A's logs fall before `since`
run
sleep 1
m=$(misses_since "$since")
[ "$m" -eq 0 ] || fail "identical re-run should be all hits, saw $m misses"
diff -r "$SNAP" "$CAS/out" >/dev/null || fail "re-run produced a different tree"
echo "  ok: re-ran in ${ELAPSED}s with 0 cache misses — no worker spawned." >&2
echo "       (this is also how a teammate inherits your cache: same hashes, all hits.)" >&2

echo "== Phase C: edit ONE file — only its branch recomputes ==" >&2
printf '# Intro\n\nWelcome! This guide now opens with a friendlier hello.\n' > "$DOCS/guide/intro.md"
sleep 1; since=$(date +%s)
run
sleep 1
edit=$(misses_since "$since")
# The untouched sibling directory is byte-identical — every summary under it was
# a cache hit.
diff -r "$SNAP/api" "$CAS/out/api" >/dev/null \
  || fail "api/ changed after editing only guide/intro.md"
# Even the unedited file in the *same* directory is reused.
diff "$SNAP/guide/setup.md/summary" "$CAS/out/guide/setup.md/summary" >/dev/null \
  || fail "guide/setup.md was recomputed though it didn't change"
# The edited file's summary and the directory summaries on the path to root did change.
! diff "$SNAP/guide/intro.md/summary" "$CAS/out/guide/intro.md/summary" >/dev/null \
  || fail "edited file's summary did not change"
! diff "$SNAP/guide/summary" "$CAS/out/guide/summary" >/dev/null \
  || fail "the edited file's directory summary did not change"
! diff "$SNAP/summary" "$CAS/out/summary" >/dev/null \
  || fail "the root summary did not change"
[ "$edit" -gt 0 ] || fail "edit should have recomputed something"
[ "$edit" -lt "$cold" ] || fail "edit ($edit) should recompute far less than cold ($cold)"
echo "  ok: edited guide/intro.md → ${edit} cache misses vs ${cold} cold (in ${ELAPSED}s)." >&2
echo "       api/ untouched; guide/setup.md reused; only intro.md + guide/ + root recomputed." >&2

echo "ALL PASS — incremental, memoized summarization over a document tree." >&2
