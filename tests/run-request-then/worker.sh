#!/bin/bash
# tests/run-request-then — a WORKER test: no client, no repo.
#
# `run-request-then <R>` tail-calls the EXACT ArgTree R — no `--in` invented, no
# request reassembled around an image — so R's hash is the request identity the
# promise interpreter executes. Covered: that identity survives the tail call,
# `then` receives only R's result, invalid forms are refused before a promise is
# recorded, an exact-request failure propagates by default, and `--catch`
# delivers it as `--error` instead.
#
# SEVEN STAGES: no run can be waited on, so each assertion is the `then` of the
# run it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --bash:@=/cas/args/bash --identify:@=/cas/args/identify \
  --driver:@=/cas/args/driver --callback:@=/cas/args/callback \
  --checks:@=/cas/args/checks --boom:@=/cas/args/boom "$@"; }

# A bash worker running one of this test's fixture scripts.
fixture() { local w=$1; shift; caos curry --base:@=/cas/args/bash --worker1:@="/cas/args/$w" "$@"; }
# Nothing to pass: run-request-then takes no subject, but run-then needs one, so
# these dispatches ride an empty tree.
nothing() { rm -rf /tmp/none && mkdir -p /tmp/none && caos put /tmp/none /cas/none >/dev/null && echo /cas/none; }
result_text() { caos get /cas/args/result >/dev/null; cat /cas/args/result; }

case "$stage" in

start)
  echo "== build one complete request R and learn its identity ==" >&2
  # identify.sh returns its OWN complete ArgTree hash, which is what lets the
  # next stage prove the tail call executes precisely R rather than a near
  # equivalent rebuilt around its image.
  caos run-then "$(nothing)" --run:hash="$(fixture identify --tag=exact-request)" \
    --then:hash="$(next identified)"
  ;;

identified)
  request=$(result_text)
  [ "${#request}" -eq 40 ] || fail "identify returned a malformed request hash: $request"
  echo "  ok: R=$request" >&2

  echo "== an exact tail call runs R unchanged ==" >&2
  caos run-then "$(nothing)" \
    --run:hash="$(fixture driver --request="$request")" \
    --then:hash="$(next exact --request="$request")"
  ;;

exact)
  caos get /cas/args/request
  request=$(cat /cas/args/request)
  [ "$(result_text)" = "$request" ] || fail "expected R to report $request, got: $(result_text)"
  echo "  ok: R retained its exact ArgTree identity" >&2

  echo "== then receives only R's result ==" >&2
  driver=$(fixture driver --request="$request")
  caos run-then "$(nothing)" \
    --run:hash="$(caos curry --base:hash="$driver" --then-img="$(fixture callback)")" \
    --then:hash="$(next threaded --request="$request")"
  ;;

threaded)
  caos get /cas/args/request
  [ "$(result_text)" = "result=$(cat /cas/args/request)" ] \
    || fail "callback result mismatch: $(result_text)"
  echo "  ok: callback received --result without a synthetic --in" >&2

  echo "== invalid forms are rejected before recording a promise ==" >&2
  caos run-then "$(nothing)" \
    --run:hash="$(fixture checks --request="$(cat /cas/args/request)")" \
    --then:hash="$(next validated)"
  ;;

validated)
  [ "$(result_text)" = "ok" ] || fail "validation checks returned: $(result_text)"
  echo "  ok: malformed request, catch-without-then, and --run were refused" >&2

  echo "== exact-request failures propagate by default ==" >&2
  # boom.sh reports its own ArgTree on stderr and exits 23. `--catch` here is
  # about THIS dispatch: the failure is the value we want, and the error text
  # carries boom's stderr.
  caos run-then "$(nothing)" --run:hash="$(fixture boom)" \
    --then:hash="$(next boomed)" --catch
  ;;

boomed)
  [ -e /cas/args/error ] || fail "expected boom to fail"
  caos get /cas/args/error
  failed=$(while IFS= read -r line; do
             case "$line" in EXACT_FAIL_REQUEST=*) printf '%s' "${line#EXACT_FAIL_REQUEST=}" ;; esac
           done < /cas/args/error)
  [ "${#failed}" -eq 40 ] \
    || fail "could not recover the failed request identity from: $(cat /cas/args/error)"

  # THE UNCAUGHT PATH, one level down: this driver does a run-request-then with
  # no --catch, so the failure must propagate through it and kill it. We catch
  # THAT, which makes the propagation a value we can assert on.
  caos run-then "$(nothing)" --run:hash="$(fixture driver --request="$failed")" \
    --then:hash="$(next uncaught --failed="$failed")" --catch
  ;;

uncaught)
  [ -e /cas/args/error ] || fail "exact request failure was swallowed without --catch"
  caos get /cas/args/error
  grep -q "exit status: 23" /cas/args/error \
    || fail "uncaught failure lost its cause: $(cat /cas/args/error)"
  echo "  ok: failure propagated" >&2

  echo "== catch delivers the exact request's failure as --error ==" >&2
  caos get /cas/args/failed
  failed_driver=$(fixture driver --request="$(cat /cas/args/failed)")
  caos run-then "$(nothing)" \
    --run:hash="$(caos curry --base:hash="$failed_driver" \
      --then-img="$(fixture callback)" --catch=1)" \
    --then:hash="$(next caught)"
  ;;

caught)
  [ "$(result_text)" = "caught=yes" ] || fail "caught callback returned: $(result_text)"
  echo "  ok: callback received --error without --in" >&2

  printf 'run-request-then: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
