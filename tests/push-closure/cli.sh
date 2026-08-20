#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI set,
# INSIDE a test stack — the suite's per-test job (tests/lib/run-test.sh).
#
# A CLIENT MUST BE ABLE TO PUSH A REQUEST IT IS ABLE TO FORM.
#
# An ArgTree references its base image as a real tree entry
# (`base_arg_entry`), so `git push` must walk that image's whole closure to
# build a pack. But the client only ever holds the image's ROOT tree —
# `get_object` fetches one object at a time and nothing fetches an image's
# closure — so the interior lives only on the server, and git cannot use the
# remote's copy as a boundary: it drops any negative tip it does not hold.
#
# Whether that is fatal turns on one thing: is the image reachable from an
# advertised remote ref the client CAN traverse? Two cases, and only one bites:
#
#   - a flake-built image (std/bash) is pinned by the server as a run RESULT,
#     so a ref points straight at it; git marks it uninteresting and never
#     needs its children.
#   - a rustc-built worker resolves to curry(runner, worker1=<binary>), and a
#     curry node holds its `base` as a BLOB naming the hash. So unwrapping it
#     yields the runner-pool image, which NO advertised ref reaches. git has to
#     pack it, cannot read it, and dies on `bad tree object <oid>`.
#
# Hence rgrep here rather than bash. The harness normally hides all of this by
# handing each client an ALTERNATE object store (tests/lib/run-test.sh →
# /tmp/seed-git); dropping it leaves exactly the state a host repo is in.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

state() { # <oid> -> absent | partial | complete
  git cat-file -e "$1" 2>/dev/null || { echo absent; return; }
  if git rev-list --objects "$1" >/dev/null 2>&1; then echo complete; else echo partial; fi
}

alt=.git/objects/info/alternates
[ -e "$alt" ] \
  || fail "this client has no alternate; the fixture's assumption about tests/lib/run-test.sh is stale"

# OWN WHAT THIS REPO TRACKS, before taking the alternate away.
#
# The harness stages `DEEP-DEPS/**` with the alternate INSTALLED
# (tests/lib/run-test.sh), so git skips writing any blob the seed store already
# holds: the `testtree` commit references objects this repo does not own —
# measured, every run. Removing the alternate then leaves them unreadable, and
# the failure surfaces wherever something first NEEDS one, naming an object with
# no bearing on this test. All three of these were reproduced here: `git commit`
# dying in write-tree (`invalid object … Error building trees`), `git commit`
# dying on `bad tree object HEAD` (it reads HEAD's tree to diff, so NO commit
# survives), and the run's own ingest of `DEEP-DEPS/rgrep` 404ing on a tree the
# server never had. Which one you get depends on git's CACHE TREE — it reuses a
# cached oid for an unchanged directory and never reads its blobs, so the whole
# thing hides until anything under `DEEP-DEPS/` perturbs it.
#
# `repack -ad` internalizes exactly the objects reachable from THIS repo's refs,
# which is the precise line: it takes the worktree's own history and leaves the
# base's CLOSURE — a resolved image reachable from no local ref, fetched one
# object at a time from the server — exactly as unreadable as the test needs.
git repack -adq
rm -f "$alt"

# Asserted rather than assumed: the failure this guards against surfaces far
# from its cause, so let it fail HERE with a sentence about the fixture.
[ "$(state "$(git rev-parse HEAD^{tree})")" = complete ] \
  || fail "this client cannot read its own HEAD tree — the fixture is broken, not the feature"

mkdir -p t
printf 'needle\n' > t/a.txt
git add -A && git -c user.email=test@caos -c user.name=caos commit -qm push-closure
# And the other half of the same claim: the base's closure must still be OUT of
# reach, or the run below proves nothing. `repack -ad` packs what local refs
# reach, and this image is reached by none — but assert it rather than argue it,
# because a fixture that silently stops testing its subject is the failure mode
# this whole test exists to catch. (At this point it reads `absent` — the client
# has not even fetched the image root yet; `complete` is the only state that
# would make the run below meaningless.)
img=$("$CAOS_CLI" eval-path DEEP-DEPS/rgrep) || fail "resolving the base image"
img=${img##* }
[ "$(state "$img")" != complete ] \
  || fail "this client can read the base's whole closure ($img) — the test no longer proves anything"

echo "== a client that cannot read its base's closure can still push ==" >&2
out=$("$CAOS_CLI" run --base:@=DEEP-DEPS/rgrep --pattern=needle --in:@=t/a.txt) \
  || fail "pushing a request whose base the client cannot read: $out"
# rgrep on a file returns `<linenum>:<line>` matches (tests/rgrep).
[ "$out" = "1:needle" ] || fail "unexpected result: $out"
echo "  ok: the request was formed, delivered and run" >&2
