#!/bin/bash
# tests/resident — a WORKER test of resident workers (design/daemons.md).
#
# A resident worker calls `caos next` when it finishes a job, and the SERVER
# hands every later job for the same instance to that one worker, in order. The
# instance is the job's reserved `affinity` arg. Covered here, end to end:
#
#   start        three messages sent at once to one instance
#   batch        …were handled by ONE process (same host and token), one at a
#                time (n is 1, 2 and 3 once each), and /cas content a job fetched
#                outside /cas/args was still there for the later ones
#   after-stop   an explicit `stop` is answered by the same process, which then
#                leaves
#   after-again  the next message for the instance wakes a NEW container
#   after-crash  a worker killed mid-job fails THAT job, and is not cached
#   after-reborn the instance is usable again afterwards, in a new container
#   after-bash   `caos next` is refused on an image that is not resident
#   after-keyless `caos next` is refused for a job with no `affinity`
#
# Not covered end to end: eviction, the lifetime cap and a lapsed lease. Each
# needs a clock a test cannot wait out, and the server side of each is covered
# by the unit tests in crates/server/src/runner.rs.
#
# THE INSTANCE NAME IS MADE ONCE, at `start`, and carried: it is part of every
# message's cache key, so a name that repeated across runs would answer from the
# cache and never reach a daemon at all.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --bash:@=/cas/args/bash --resident:@=/cas/args/resident \
  --daemon:@=/cas/args/daemon --refuse:@=/cas/args/refuse "$@"; }

arg() { caos get "/cas/args/$1" >/dev/null; cat "/cas/args/$1"; }
result_text() { caos get /cas/args/result >/dev/null; cat /cas/args/result; }
# `key=value` out of a reply (no sed or awk in std/bash).
field() { local word; for word in $1; do case "$word" in "$2="*) printf '%s' "${word#"$2="}"; return ;; esac; done; }

# The resident image, running the daemon, for instance $1.
daemon() { caos curry --base:@=/cas/args/resident --worker1:@=/cas/args/daemon "--affinity=$1"; }
# A message: a blob at /cas/msg-<name>, returned as the path.
message() {
  printf '%s' "$1" > "/tmp/msg-$1"
  caos put "/tmp/msg-$1" "/cas/msg-$1" >/dev/null
  echo "/cas/msg-$1"
}

case "$stage" in

start)
  name="resident-$(date +%s%N)-$$-$RANDOM"
  mkdir -p /tmp/msgs
  for m in a b c; do printf '%s' "$m" > "/tmp/msgs/$m"; done
  caos put /tmp/msgs /cas/msgs
  echo "== three messages for instance $name, sent at once ==" >&2
  # `map-then` runs the image once per child of /cas/msgs, in parallel, binding
  # each as `in`. Every child carries the same `affinity`, so the server must
  # give the three to one owner.
  caos map-then /cas/msgs --map:hash="$(daemon "$name")" \
    --then:hash="$(next batch "--name=$name")"
  ;;

batch)
  name=$(arg name)
  caos get /cas/args/children >/dev/null
  hosts="" tokens="" ns="" kepts=""
  for m in a b c; do
    caos get "/cas/args/children/$m" >/dev/null
    r=$(cat "/cas/args/children/$m")
    echo "  $m -> $r" >&2
    [ "$(field "$r" in)" = "$m" ] || fail "message $m was answered with: $r"
    hosts+="$(field "$r" host)"$'\n'
    tokens+="$(field "$r" token)"$'\n'
    ns+="$(field "$r" n)"$'\n'
    kepts+="$(field "$r" kept)"$'\n'
  done
  [ "$(sort -u <<<"$hosts" | grep -c .)" -eq 1 ] || fail "more than one container answered: $hosts"
  [ "$(sort -u <<<"$tokens" | grep -c .)" -eq 1 ] || fail "more than one process answered: $tokens"
  [ "$(sort -n <<<"$ns" | grep . | tr '\n' ' ')" = "1 2 3 " ] \
    || fail "jobs did not run one at a time in one process; n was: $(tr '\n' ' ' <<<"$ns")"
  [ "$(grep -c '^no$' <<<"$kepts")" -eq 1 ] \
    || fail "exactly the first job should have found /cas/kept absent; got: $(tr '\n' ' ' <<<"$kepts")"
  echo "  ok: one process, three jobs, serialized, /cas content kept" >&2

  host=$(head -n1 <<<"$hosts")
  token=$(head -n1 <<<"$tokens")
  echo "== an explicit stop is answered by that same process ==" >&2
  caos run-then "$(message stop)" --run:hash="$(daemon "$name")" \
    --then:hash="$(next after-stop "--name=$name" "--host=$host" "--token=$token")"
  ;;

after-stop)
  name=$(arg name)
  r=$(result_text)
  [ "$(field "$r" host)" = "$(arg host)" ] && [ "$(field "$r" token)" = "$(arg token)" ] \
    || fail "stop was answered by a different process: $r"
  [ "$(field "$r" n)" = 4 ] || fail "stop should have been the fourth job; got: $r"
  echo "  ok: $r" >&2

  echo "== the next message wakes a new container ==" >&2
  caos run-then "$(message again)" --run:hash="$(daemon "$name")" \
    --then:hash="$(next after-again "--name=$name" "--host=$(arg host)")"
  ;;

after-again)
  name=$(arg name)
  r=$(result_text)
  [ "$(field "$r" host)" != "$(arg host)" ] || fail "no new container after the stop: $r"
  [ "$(field "$r" n)" = 1 ] && [ "$(field "$r" kept)" = no ] \
    || fail "the new daemon should start from nothing; got: $r"
  echo "  ok: $r" >&2

  echo "== a worker killed mid-job fails that job ==" >&2
  caos run-then "$(message crash)" --run:hash="$(daemon "$name")" \
    --then:hash="$(next after-crash "--name=$name" "--host=$(field "$r" host)")" --catch
  ;;

after-crash)
  name=$(arg name)
  [ -e /cas/args/error ] || fail "expected the killed worker's job to fail, got: $(result_text)"
  caos get /cas/args/error >/dev/null
  grep -q "exit status: 137" /cas/args/error || fail "no kill reported; got: $(cat /cas/args/error)"
  echo "  ok: the job failed ($(head -c 120 /cas/args/error | head -n1))" >&2

  echo "== and the instance is usable again, in a new container ==" >&2
  caos run-then "$(message reborn)" --run:hash="$(daemon "$name")" \
    --then:hash="$(next after-reborn "--name=$name" "--host=$(arg host)")"
  ;;

after-reborn)
  r=$(result_text)
  [ "$(field "$r" host)" != "$(arg host)" ] || fail "the killed daemon's container answered: $r"
  [ "$(field "$r" n)" = 1 ] || fail "expected a new daemon, got: $r"
  echo "  ok: $r" >&2

  echo "== caos next is refused on an image that is not resident ==" >&2
  caos run-then "$(message x)" \
    --run:hash="$(caos curry --base:@=/cas/args/bash --worker1:@=/cas/args/refuse)" \
    --then:hash="$(next after-bash)"
  ;;

after-bash)
  r=$(result_text)
  grep -q "CAOS_RESIDENT=1" <<<"$r" || fail "wrong refusal: $r"
  echo "  ok: $r" >&2

  echo "== and for a job that names no instance, even on a resident image ==" >&2
  caos run-then "$(message y)" \
    --run:hash="$(caos curry --base:@=/cas/args/resident --worker1:@=/cas/args/refuse)" \
    --then:hash="$(next after-keyless)"
  ;;

after-keyless)
  r=$(result_text)
  grep -q "affinity" <<<"$r" || fail "wrong refusal: $r"
  echo "  ok: $r" >&2

  printf 'resident: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
