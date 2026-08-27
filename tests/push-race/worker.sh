#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/cli-test stages the repo, then runs this).
#
# Exercises CONCURRENT `ensure_pushed` (crates/caos/src/lib.rs): two clients
# pushing the same object to one server must both succeed.
#
# The failure it guards is not a slow path, it is a dead one. A request's
# objects ship under a content-addressed `refs/caos/req/<hash>`, and two
# clients pushing the same object both read an advertisement without that ref,
# so both plan a CREATE. The one that locks second dies:
#
#   remote: error: cannot lock ref 'refs/caos/req/<h>': reference already exists
#
# Measured before the fix: of six concurrent pushes of one new object, ONE
# succeeded and five failed. `--force` does not help — the create precondition
# comes from the advertised state, not from the refspec — so the fix is a
# retry, which is a no-op success because the winner has already landed both
# the objects and the ref.
#
# Nothing hits this today, because every test has a server to itself. It is
# tested now because design/faster-tests.md makes it load-bearing: one test
# stack shared by every test means ~28 clients pushing overlapping objects to
# one server, where this is not a race but the normal case.
#
# NO CONTAINERS. `curry` over a `docker://` ref resolves nothing and runs
# nothing — it builds an arg tree and pushes it — so this drives the real
# client push path for the cost of a few processes. The image ref is never
# pulled and does not have to exist.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# The object must be NEW, or the ref already exists and there is no create to
# race over — the bug would sit right here and the test would pass.
marker="$(date +%s%N)-$$-$RANDOM"
printf 'push-race probe %s\n' "$marker" > probe
git add probe
git -c user.email=test@caos -c user.name=caos commit -qm "probe $marker"

N=6

echo "== $N concurrent curries of the SAME new object all succeed ==" >&2
pids=()
for i in $(seq 1 "$N"); do
  # Each writes its own hash and status; a shared file would interleave.
  #
  # `if`, not `cmd; echo $?`: the harness runs cli.sh under `set -e` with an
  # errtrace ERR trap, so a failing curry would kill the subshell BEFORE it
  # recorded its status — the rc file would be missing, the read below would
  # trip the trap, and the very failure this test exists to catch would be
  # reported as "aborted" (a harness bug) instead of FAIL (the thing under test
  # is wrong). Verified by reverting the fix: it aborted.
  (
    if "$CAOS_CLI" curry --base:docker=caos-push-race-not-pulled --probe:@=probe \
         > "/tmp/curry.$i.out" 2> "/tmp/curry.$i.err"
    then echo 0 > "/tmp/curry.$i.rc"
    else echo $? > "/tmp/curry.$i.rc"
    fi
  ) &
  pids+=("$!")
done
for p in "${pids[@]}"; do wait "$p" || true; done

losers=0
for i in $(seq 1 "$N"); do
  rc=$(cat "/tmp/curry.$i.rc")
  if [ "$rc" != 0 ]; then
    losers=$((losers + 1))
    echo "  client $i exited $rc: $(tail -n 2 "/tmp/curry.$i.err")" >&2
  fi
done
[ "$losers" -eq 0 ] || fail "$losers of $N concurrent pushes failed — ensure_pushed does not survive a create race"
echo "  ok: $N/$N" >&2

# All of them curried the same inputs, so all of them must name the same arg
# tree. This is what says the survivors did the WORK and not merely exited 0 —
# a push that quietly skipped its objects would still print a hash here, but a
# disagreeing one would mean they were not racing over one object at all.
echo "== every client agrees on the arg tree ==" >&2
first=$(cat /tmp/curry.1.out)
[ -n "$first" ] || fail "client 1 printed no hash"
for i in $(seq 2 "$N"); do
  got=$(cat "/tmp/curry.$i.out")
  [ "$got" = "$first" ] || fail "client $i printed $got, client 1 printed $first"
done
echo "  ok: all $N agree on $first" >&2

# And the objects really are on the server: fetch the arg tree back by hash.
# Without this the test would pass on a client that returned early — the whole
# point of the ref is that the objects arrived with it.
echo "== the pushed object is readable from the server ==" >&2
rm -rf /tmp/fetched
"$CAOS_CLI" get "$first" /tmp/fetched >/dev/null 2>&1 \
  || fail "the server cannot serve $first back — the push did not land"
# A curried arg tree is `{.caos-curry, args, base}`, so a bound arg is under
# `args/`, not at the root.
[ -e /tmp/fetched/args/probe ] \
  || fail "fetched tree has no args/probe: $(ls -A /tmp/fetched)"
grep -q "$marker" /tmp/fetched/args/probe \
  || fail "fetched probe is not this run's: $(cat /tmp/fetched/args/probe)"
echo "  ok: $first fetched back with this run's probe" >&2

echo "push-race: ALL PASS" >&2
