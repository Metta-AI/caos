#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI)
# ----------------------------------------------------------
# The subject here is first-class commits as CROSS-STACK VALUES: a commit that
# the client hands to the inner stack as a `:commit=` arg, that a worker reads,
# walks and re-mints server-side, and that comes back as a real git object the
# client can fetch by hash. None of that exists outside a live run — there is
# no object to inspect, no minted child, no round-trip — until the tested
# client drives a real computation against the inner server. So the CLI is not
# incidental scaffolding for verifying something inspectable on disk; it is the
# ONE and ONLY thing that can put a commit onto the inner stack and pull the
# result back. Every step below is a property OF the tested client:
#
#   - `curry`/`run` build and launch a source-built worker on the inner stack —
#     the inner stack can only be driven through the tested client (run-test.sh:
#     "must be the one every test command goes through").
#   - `--head:commit=HEAD` pins that the client marshals a commit UNPEELED
#     (the worker must see the commit, not its tree) — a wire-format property of
#     the client's arg encoding, observable only by running it.
#   - the stdout `commit <hash>` and the `git fetch caos <hash>` round-trip pin
#     that the minted child is a genuine server-held commit the client can name
#     by hash — the client's result decoding and fetch negotiation, end to end.
#
# Replace the CLI with plain git or a direct object read and there is nothing
# left to test: the commit was never marshalled, never minted, never fetched.
#
# Mechanics: the client passes HEAD as a `:commit=` arg (unpeeled — the worker
# sees the commit, not its tree); a source-built worker (commit-worker.rs,
# compiled by the rustc builder, linking worker-common's commit helpers) reads
# it, walks its tree and parent by hash, runs one tool call through run-then,
# and mints a child commit — message from the tool's output, tree unchanged,
# parent = HEAD — returned as `commit <hash>`. The client gets the raw commit
# bytes on stdout and can fetch the real object from the server by hash.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

echo "== build the commit worker from source ==" >&2
# No --runner: rustc DEPENDS on the runner pool (std/rustc/DEPS) and curries
# the built binary onto it itself, so a caller says only what it is building.
builder=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/rustc)
"$CAOS_CLI" run img --base:hash="$builder" --src:@=test/commit-worker.rs
commit "built worker image"
worker=$(git rev-parse HEAD:img)

# The conversation head this run is over: the current HEAD (which now includes
# the built image, so it has real history and content behind it).
head=$(git rev-parse HEAD)
head_tree=$(git rev-parse 'HEAD^{tree}')

echo "== HEAD as a :commit= arg -> worker -> child commit on stdout ==" >&2
# --bash: the worker curries its tool onto the bash image, and a worker cannot
# evaluate a `.caos-expr` — so we resolve it here and bind the hash.
bash_img=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash)
"$CAOS_CLI" run --base:hash="$worker" --head:commit=HEAD --tool-script:@=test/tool.sh \
  --bash="$bash_img" \
  > child.commit
grep -q "^tree $head_tree\$" child.commit \
  || fail "child commit does not snapshot HEAD's tree: $(cat child.commit)"
grep -q "^parent $head\$" child.commit \
  || fail "child commit's parent is not HEAD: $(cat child.commit)"
grep -q "tool said 42" child.commit \
  || fail "message doesn't carry the tool output: $(cat child.commit)"
echo "  ok: child commit has HEAD as parent, HEAD's tree, and the tool output" >&2

echo "== the minted commit is fetchable from the server as a real commit ==" >&2
hash=$(git hash-object -t commit --stdin < child.commit)
# noop negotiation, as the CLI itself uses: single-round fetch by bare hash.
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$hash"
[ "$(git cat-file -t "$hash")" = "commit" ] || fail "$hash is not a commit"
[ "$(git rev-parse "$hash^{tree}")" = "$head_tree" ] || fail "fetched tree differs"
[ "$(git rev-parse "$hash^")" = "$head" ] || fail "fetched parent differs"
git cat-file commit "$hash" | grep -q "tool said 42" || fail "fetched message differs"
echo "  ok: fetched $hash and verified tree/parent/message with plain git" >&2

echo "commit: ALL PASS" >&2
