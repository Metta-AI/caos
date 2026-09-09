#!/bin/bash
# tests/llm-call — a WORKER test: no client, no repo.
#
# Exercises the generic stateless LLM call worker against the scripted API
# stub: caller-owned prompt/messages/config in, plain text blob out, with no
# agent tools, thinking mode, commits, or conversation refs.
#
# ONE STAGE, WHICH IS THE POINT. The stub only exists while this container does,
# so everything that observes it has to happen here — and a worker may not block
# on a run. So the call is DISPATCHED (through relay.sh; read its header for
# why the indirection is load-bearing) and the result is waited for by POLLING
# FOR ITS OBJECT: llm-call writes the response text with no trailing newline, so
# its blob oid is a pure function of the text the stub is scripted to return,
# and `caos get-hash` on that oid is simultaneously "the run finished" and "it
# returned exactly the right text".
#
# THE OID IS COMPUTED WITH sha1sum, not `git hash-object`: std/bash has no git.
# A git blob's id is sha1 of `blob <byte-count>\0` followed by the content.
#
# WHICH IS ALSO WHY THE SCRIPTED REPLY IS GENERATED HERE RATHER THAN CHECKED IN.
# A fixed reply hashes to a fixed oid, and that object OUTLIVES the run that
# stored it — so the probe answered yes instantly, on an earlier run's blob, and
# the test passed with llm-call never having called the stub (observed: "ok:
# text block returned verbatim" immediately above "the stub recorded no
# request"). The title carries this run's salt, so the object it hashes to
# cannot already exist and the probe can only be answered by this call.
#
# THE KEY COMES FROM /secret. A store is built by a CLIENT, so caos-tools/test
# registers a MOCK anthropic-api-key naming `std/llm-call` as its reader when it
# brings the dev stack up; the server grants it here because this call's ArgTree
# is a superset of that reader's. Nothing sensitive: the value is a constant and
# the stub is the only thing that sees it.
#
# THIS JOB HOLDS A RUNNER SLOT WHILE IT SERVES, and needs the llm-call job to get
# one concurrently — one spare slot, no more.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

caos get -r /cas/args/stub || fail "reading the llm-stub entry"
caos get -r /cas/args/llm-call || fail "reading the llm-call image"
caos get /cas/args/test-salt || fail "reading --test-salt"

# The stub, from its std entry (std/llm-stub): a cargo `--cmd=build` result, so
# the executable is at bin/<name>. Copied out because materialized CAS content
# is read-only and owner-only — exec straight from /cas is "Permission denied".
stub_bin=/tmp/llm-stub
install -m 755 /cas/args/stub/bin/llm-stub "$stub_bin" || fail "staging llm-stub"
# The scripted reply. The title is unique to this run (see the header), and the
# thinking block is the half llm-call must DROP. `jq -Rs` so the title is
# escaped as JSON rather than pasted into it.
title="Improve sidebar titles $(date +%s%N)-$$-$RANDOM-$(cat /cas/args/test-salt)"
mkdir -p /tmp/stub
printf '%s' "$title" | jq -Rs \
  '{content:[{type:"thinking",thinking:"hidden"},{type:"text",text:.}],stop_reason:"end_turn"}' \
  > /tmp/stub/response-1.json || fail "scripting the stub response"

# THIS CONTAINER'S OWN ADDRESS ON THE NETWORK, not 127.0.0.1: the llm-call
# worker is a separate container, so loopback would be its own. Read from
# /etc/hosts in bash — there is no `hostname`, `getent`, `ip` or `ifconfig` in
# this image, and an absent binary is exit 127 at runtime, not a build error.
# (dev/cli-test does the same thing for the same reason.)
self=$(cat /etc/hostname) || fail "no /etc/hostname"
stub_host=""
while read -r ip names; do
  case " $names " in *" $self "*) stub_host=$ip; break ;; esac
done < /etc/hosts
[ -n "$stub_host" ] || fail "no /etc/hosts entry for $self"

stub_pid=""
for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" /tmp/stub 2>/tmp/stub.log &
  stub_pid=$!
  # Wait for the LISTENER, not for a fixed interval: the stub binds in a few ms.
  # Probing the port also tells the two failures apart — a dead process (retry
  # on another port) against one still coming up.
  ready=0
  for _ in {1..400}; do
    if ! kill -0 "$stub_pid" 2>/dev/null; then break; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then ready=1; break; fi
    sleep 0.005
  done
  if [ "$ready" = 1 ]; then break; fi
  kill "$stub_pid" 2>/dev/null || true
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat /tmp/stub.log)"
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT
echo "== llm-stub listening on $stub_host:$port ==" >&2

# llm-call writes the joined text blocks with NO trailing newline, so the result
# blob is exactly the title and its oid is a pure function of it.
oid=$({ printf 'blob %d\000' "${#title}"; printf '%s' "$title"; } \
  | sha1sum | cut -d' ' -f1)

messages='[{"role":"user","content":"Name the sidebar task"}]'
call=$(caos curry --base:hash="$(caos hash /cas/args/llm-call)" \
  --base-url="http://$stub_host:$port" \
  --system='Return only a concise title.' \
  --messages="$messages" \
  --model=test-model \
  --max-tokens=64) || fail "currying the llm-call request"

# DISPATCHED THROUGH relay.sh, which is where the whole shape of this file comes
# from — see its header. The short version: only `run-then` gets a secret folded
# into the callee's key, and `run-then` makes the caller exit; the stub cannot
# outlive this container, so the run-then goes one job down and this one stays up.
caos get /cas/args/relay || fail "reading relay.sh"
relay=$(caos prepare-request --base:@=/cas/args/base \
  --worker1:@=/cas/args/relay --call="$call") || fail "forming the relay request"
caos sub-run "$relay" >/dev/null || fail "dispatching the relay"

echo "== the call returns the reply's text blocks as a plain blob ==" >&2
found=0
for _ in $(seq 1 300); do
  if caos get-hash "$oid" /cas/response 2>/dev/null; then found=1; break; fi
  sleep 0.25
done
if [ "$found" -ne 1 ]; then
  echo "--- stub log"; cat /tmp/stub.log >&2 || true
  if [ -f /tmp/stub/request-1.json ]; then
    echo "--- the stub DID receive a request:" >&2; cat /tmp/stub/request-1.json >&2
  else
    echo "--- the stub received no request at all" >&2
  fi
  fail "llm-call (via relay $relay) never returned the expected text ($oid)"
fi
echo "  ok: thinking block dropped, text block returned verbatim" >&2

echo "== the request carries exactly the caller's configuration ==" >&2
# The result exists, so the call is over: request-2.json's absence is decided,
# not raced.
req=/tmp/stub/request-1.json
[ -f "$req" ] || fail "the stub recorded no request"
grep -qF '"model":"test-model"' "$req" || fail "model not sent"
grep -qF '"max_tokens":64' "$req" || fail "max_tokens not sent"
grep -qF '"system":"Return only a concise title."' "$req" \
  || fail "system prompt not sent"
grep -qF '"messages":[{"content":"Name the sidebar task","role":"user"}]' "$req" \
  || fail "messages not sent"
if grep -q '"tools"' "$req"; then fail "stateless call registered tools"; fi
if grep -q '"thinking"' "$req"; then fail "stateless call enabled thinking"; fi
[ ! -f /tmp/stub/request-2.json ] || fail "worker made more than one model call"
echo "  ok: no tools, no thinking, exactly one call" >&2

printf 'llm-call: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
