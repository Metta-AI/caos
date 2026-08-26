#!/bin/bash
# tests/sub-run — a WORKER test: no client, no repo.
#
# `sub-run` dispatches a request and returns immediately. Two claims:
#
#   * A worker may sub-run its OWN in-flight ArgTree. A blocking implementation
#     would wait on itself and deadlock; the server admits the request under the
#     current stack instead, where the recursive edge fails independently.
#   * Work dispatched that way finishes AFTER ITS CALLER EXITS. The launcher
#     container is gone by the time the delayed job completes, so nothing is
#     waiting for it — and it must still store its result.
#
# HOW THE SECOND CLAIM IS OBSERVED, given there is deliberately no caller to ask.
# The delayed job's result content is unique to this run, so its git oid is
# unique too — and that oid is PREDICTED here, from the bytes, then looked up
# with `caos get-hash`. Predicted rather than obtained, because every way of
# asking caos for the oid (`put`, a run) would either store the object or join
# the request, and either would make the lookup answer yes for the wrong reason.
#
# THE OID IS COMPUTED WITH sha1sum, not `git hash-object`: std/bash has no git.
# A git blob's id is sha1 of `blob <byte-count>\0` followed by the content, and
# spelling that out is a fair thing for a test about object addressability to do.
#
# NO STAGE HERE WAITS ON ANOTHER JOB — which an earlier version of this file got
# wrong, polling `caos get-hash` in a loop until the result appeared. That is
# hold-and-wait: a container occupying a runner slot while the job it needs is
# queued for one, which is exactly what design/map-then.md's "this cannot
# deadlock" argument rules out. The one thing this test genuinely needs is for
# TIME to pass with nothing observing, so the delay is a job of its own
# (settle.sh): it depends on nothing, always finishes, always frees its slot,
# and its `then` looks exactly once.
#
# FIVE STAGES: no run can be waited on, so each assertion is the `then` of the
# dispatch it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --self:@=/cas/args/self --delayed:@=/cas/args/delayed \
  --launch:@=/cas/args/launch --settle:@=/cas/args/settle "$@"; }

# The image this test is running in, by oid — what a fixture runs in too.
BASH=$(caos hash /cas/args/base)
result_text() { caos get /cas/args/result >/dev/null; cat /cas/args/result; }

case "$stage" in

start)
  echo "== sub-run dispatches an in-flight request without waiting ==" >&2
  # self.sh hashes its own /cas/args and sub-runs THAT. `run-request-then` runs
  # the ArgTree unmodified, so the hash self.sh computes is the request it is
  # already running as — the self-reference the claim is about.
  req=$(caos prepare-request --base:hash="$BASH" --worker1:@=/cas/args/self) \
    || fail "forming the self-referential request"
  caos run-request-then "$req" --then:hash="$(next selfed)"
  ;;

selfed)
  reply=$(result_text)
  q=${reply#request }
  if [ "$q" = "$reply" ] || [ "${#q}" -ne 40 ]; then
    fail "expected 'request <40-character Q>', got: $reply"
  fi
  case "$q" in *[!0-9a-f]*) fail "Q is not a hex oid: $q" ;; esac
  echo "  ok: the worker continued after dispatching the request ($q)" >&2

  echo "== dispatched work finishes after its caller exits ==" >&2
  # Unique to THIS run, so the object it hashes to cannot already exist —
  # otherwise the probe below would pass on a previous run's leftovers.
  caos get /cas/args/test-salt
  payload="background-finished-$(date +%s%N)-$$-$RANDOM-$(cat /cas/args/test-salt)"
  content="completed: $payload"
  # A git blob's id: sha1 over `blob <n>\0<content>`, n counting the newline.
  oid=$({ printf 'blob %d\000' "$((${#content} + 1))"; printf '%s\n' "$content"; } \
    | sha1sum | cut -d' ' -f1)

  qd=$(caos prepare-request --base:hash="$BASH" --worker1:@=/cas/args/delayed \
    --payload="$payload") || fail "preparing the delayed request"
  launcher=$(caos prepare-request --base:hash="$BASH" \
    --worker1:@=/cas/args/launch --request="$qd") || fail "preparing the launcher"
  caos run-request-then "$launcher" \
    --then:hash="$(next dispatched --qd="$qd" --oid="$oid" --content="$content")"
  ;;

dispatched)
  caos get /cas/args/qd; caos get /cas/args/oid; caos get /cas/args/content
  qd=$(cat /cas/args/qd); oid=$(cat /cas/args/oid)
  [ "$(result_text)" = "request $qd" ] \
    || fail "launcher dispatched the wrong request: $(result_text)"
  echo "  launcher dispatched $qd and exited" >&2

  # LETTING TIME PASS WITH NOTHING OBSERVING, which is the claim. The delayed
  # job sleeps 2s and nobody is waiting for it: the launcher's container is
  # gone, and this job must not wait either — a worker that blocks on another
  # job holds a runner slot while the job it needs is queued for one, which is
  # the hold-and-wait design/map-then.md's deadlock argument rules out.
  #
  # So the delay is its OWN job. settle.sh depends on nothing, always finishes
  # and always frees its slot, so no number of them can deadlock a suite — and
  # its `then` looks exactly once. `--marker` keys it to this run; a cached
  # sleep would return instantly and prove nothing.
  caos run-request-then \
    "$(caos prepare-request --base:hash="$BASH" --worker1:@=/cas/args/settle \
       --seconds=8 --marker="$oid")" \
    --then:hash="$(next settled --qd="$qd" --oid="$oid" \
      --content="$(cat /cas/args/content)")"
  ;;

settled)
  caos get /cas/args/qd; caos get /cas/args/oid; caos get /cas/args/content
  qd=$(cat /cas/args/qd); oid=$(cat /cas/args/oid)
  # ONE LOOK, no loop: the sleeper has finished, so the dispatched job has had
  # its 2 seconds several times over. Only the disconnected request can make
  # this oid addressable, and nothing ever asked it for a result.
  caos get-hash "$oid" /cas/probe \
    || fail "dispatched request $qd never stored its result ($oid)"
  [ "$(cat /cas/probe)" = "$(cat /cas/args/content)" ] \
    || fail "background result contents were wrong: $(cat /cas/probe)"
  echo "  ok: result became addressable with no caller waiting for it" >&2

  printf 'sub-run: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
