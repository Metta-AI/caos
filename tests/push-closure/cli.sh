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
rm -f "$alt"

mkdir -p t
printf 'needle\n' > t/a.txt
git add -A && git -c user.email=test@caos -c user.name=caos commit -qm push-closure

echo "== a client that cannot read its base's closure can still push ==" >&2
out=$("$CAOS_CLI" run --base:@=DEEP-DEPS/rgrep --pattern=needle --in:@=t/a.txt) \
  || fail "pushing a request whose base the client cannot read: $out"
# rgrep on a file returns `<linenum>:<line>` matches (tests/rgrep).
[ "$out" = "1:needle" ] || fail "unexpected result: $out"
echo "  ok: the request was formed, delivered and run" >&2
