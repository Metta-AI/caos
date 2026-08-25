#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI set,
# INSIDE a test stack — the suite's per-test job (dev/run-test/run-test.sh).
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
# Hence rgrep here rather than bash. The old harness hid all of this by handing
# each client an ALTERNATE object store; the dev-stack harness hands out none,
# so a client is already in exactly the state a host repo is in.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

state() { # <oid> -> absent | partial | complete
  git cat-file -e "$1" 2>/dev/null || { echo absent; return; }
  if git rev-list --objects "$1" >/dev/null 2>&1; then echo complete; else echo partial; fi
}

# NO ALTERNATE TO REMOVE ANY MORE, and that is the point rather than a gap.
#
# This fixture used to have to CREATE the condition it tests: the harness handed
# every client an alternate object store, so a client could read objects it had
# never fetched, and this test installed nothing but took that away again. The
# dev-stack harness hands out no alternate, so every client is already in the
# state a host repo is in — which is what this test was reaching for.
#
# `repack -ad` stays, and still earns its place: it internalizes exactly the
# objects reachable from THIS repo's refs, so the worktree owns its own history
# while the base's CLOSURE — a resolved image reachable from no local ref —
# stays as unreadable as the test needs. The assertions below check both halves
# rather than assuming them, because a fixture that silently stops testing its
# subject is precisely the failure this test exists to catch.
git repack -adq

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
