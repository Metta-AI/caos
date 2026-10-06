#!/bin/bash
# tests/stack-daemon — a WORKER test of the stack daemon (dev/stack-daemon) for a
# tree that has no stack.
#
#   status, logs, harvest, stop   each answer `not running` and start nothing: the
#                                 daemon's container is woken to say so and exits,
#                                 since there is nothing for it to keep resident
#   start with no relay           is refused before anything is brought up, and
#                                 the refusal says what is missing
#
# The "tree under test" is a one-file directory: the daemon only compares its hash
# with `affinity` until an op needs a stack, and none of these does. A message is
# a request like any other, so each stage is the `then` of the one before; every
# message carries a `request-id` made here, because a repeated one would answer
# from the cache and never reach a daemon.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --daemon:@=/cas/args/daemon "$@"; }
result_text() { caos get /cas/args/result >/dev/null; cat /cas/args/result; }

# One message to the daemon for the tree named by --tree (a hash; the tree itself
# is /cas/tree, rebuilt by every stage because /cas is per job).
message() { # <op> [--name=value ...]
  local op=$1; shift
  mkdir -p /tmp/tree && echo x > /tmp/tree/file
  caos put /tmp/tree /cas/tree >/dev/null
  caos prepare-request --base:@=/cas/args/daemon "--affinity=$(caos hash /cas/tree)" \
    "--op=$op" --in:@=/cas/tree "--request-id=$(date +%s%N)-$$-$RANDOM" "$@"
}

case "$stage" in

start)
  echo "== status of a tree with no stack ==" >&2
  caos run-request-then "$(message status)" --then:hash="$(next after-status)"
  ;;

after-status)
  r=$(result_text)
  grep -q "^not running" <<<"$r" || fail "status should say not running; got: $r"
  echo "  ok: $r" >&2
  echo "== logs ==" >&2
  caos run-request-then "$(message logs)" --then:hash="$(next after-logs)"
  ;;

after-logs)
  r=$(result_text)
  grep -q "^not running" <<<"$r" || fail "logs should say not running; got: $r"
  echo "  ok: $r" >&2
  echo "== harvest ==" >&2
  caos run-request-then "$(message harvest)" --then:hash="$(next after-harvest)"
  ;;

after-harvest)
  r=$(result_text)
  grep -q "^not running" <<<"$r" || fail "harvest should say not running; got: $r"
  echo "  ok: $r" >&2
  echo "== stop ==" >&2
  caos run-request-then "$(message stop)" --then:hash="$(next after-stop)"
  ;;

after-stop)
  r=$(result_text)
  grep -q "^not running" <<<"$r" || fail "stop should say not running; got: $r"
  echo "  ok: $r" >&2
  echo "== start with no relay is refused ==" >&2
  caos run-request-then "$(message start)" --then:hash="$(next after-start)" --catch
  ;;

after-start)
  [ -e /cas/args/error ] || fail "start with no relay should have failed; got: $(result_text)"
  caos get /cas/args/error >/dev/null
  grep -q "needs a relay" /cas/args/error || fail "wrong refusal: $(cat /cas/args/error)"
  echo "  ok: refused: $(grep -m1 'needs a relay' /cas/args/error)" >&2

  printf 'stack-daemon: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
