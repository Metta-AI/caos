#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job (tests/lib/run-test.sh).
#
# max_tokens continuation (design/agent-harness.md): when a round ends with
# stop_reason "max_tokens" the harness does NOT fail the turn — it appends the
# partial assistant content as a prefill and asks the model to resume,
# accumulating every partial into the one logical round. This drives a scripted
# stub through TWO truncations before an end_turn and asserts (a) the turn
# advanced, (b) its message is the concatenation of all three partials, and
# (c) each continuation request replayed the running prefill verbatim.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent] -> a commit minted with plain git
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}

echo "== stage the stub and fixtures ==" >&2
# The stub, from its std entry (std/llm-stub): a cargo `--cmd=build` result, so
# the executable is at bin/<name>. Copied out because materialized CAS content
# is read-only and owner-only — exec straight from /cas is "Permission denied".
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry || fail "resolving std/llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

mkdir -p ws
echo "hello" > ws/greeting.txt
echo "You are a coding agent operating on a git workspace." > system.txt
commit "workspace + fixtures"

base=$(mkcommit "HEAD:ws" "base")

echo "== script the stub: two truncations, then end_turn ==" >&2
# response_text joins text blocks with a blank line, so the turn message is the
# three partials joined by "\n\n".
P1='[{"text":"Part one.","type":"text"}]'
P2='[{"text":"Part two.","type":"text"}]'
P3='[{"text":"Part three.","type":"text"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"max_tokens"}' "$P1" > stub/response-1.json
printf '{"content":%s,"stop_reason":"max_tokens"}' "$P2" > stub/response-2.json
printf '{"content":%s,"stop_reason":"end_turn"}'   "$P3" > stub/response-3.json

stub_pid=""
for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  # Wait for the LISTENER, not for a fixed interval: a flat `sleep 0.5` here
  # was half a second of a test whose whole body is a few seconds, and the stub
  # binds in a few ms. Probing the port also tells the two failures apart — a
  # dead process (retry on another port) against one still coming up.
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
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT

echo "== curry llm-step and run the turn ==" >&2
conv="max-tokens-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
conversation_ref="refs/caos/v2/conversations/$conv/head"
# Workers reach the stub as host.containers.internal from the outer engine's
# container network; nested siblings share this job's netns (CAOS_STUB_HOST).
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
# NO TOOL IMAGES: llm-step's `.caos-expr` binds its own (std/llm-step/DEPS), so
# a caller says what the turn is, never which shell the agent greps with.
llm=$("$CAOS_CLI" curry DEEP-DEPS/llm-step -- \
  --api-key=test-key --system:@=system.txt \
  --model=test-model --base-url="http://$stub_host:$port" \
  --conversation="$conv")

human1=$(mkcommit "HEAD:ws" \
  '{"author":"user","content":"write me a long answer","v":2}' \
  "$base")
request=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$human1")
[ "${#request}" -eq 40 ] && [[ "$request" =~ ^[0-9a-f]+$ ]] \
  || fail "prepared request is not exact Q: $request"
admitted=$(mkcommit "HEAD:ws" \
  "{\"v\":2,\"request\":\"$request\",\"request_head\":\"$human1\",\"status\":\"queued\"}" \
  "$human1")
git push --quiet caos "$admitted:$conversation_ref" || fail "publishing request admission"
"$CAOS_CLI" run "$request" -- > turn.commit
turn=$(git hash-object -t commit --stdin < turn.commit)
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$turn"

echo "== the turn advanced and concatenated the three partials ==" >&2
git merge-base --is-ancestor "$human1" "$turn" \
  || fail "terminal event does not descend from the queued event"
[ "$(git show -s --format=%an "$turn")" = "caos-agent" ] || fail "turn author"
want=$(printf 'Part one.\n\nPart two.\n\nPart three.')
[ "$(git show -s --format=%B "$turn" | jq -r .content)" = "$want" ] \
  || fail "turn message is not the concatenation of the three partials"
[ "$(git rev-parse "$turn^{tree}")" = "$(git rev-parse "$human1^{tree}")" ] \
  || fail "toolless turn changed the tree"
echo "  ok: single-parent turn, message = the three partials joined" >&2

echo "== each continuation replayed the running prefill verbatim ==" >&2
grep -qF '"max_tokens":64000' stub/request-1.json || fail "max_tokens not sent"
grep -qF '{"content":"write me a long answer","role":"user"}]' stub/request-1.json \
  || fail "round 1 user message wrong"
# Request 2 (first continuation) ends with round 1's partial prefilled.
grep -qF "\"content\":$P1,\"role\":\"assistant\"}]" stub/request-2.json \
  || fail "round-1 partial not prefilled as the trailing assistant message in request 2"
# Request 3 (second continuation) carries BOTH prior partials as prefill.
grep -qF "\"content\":$P1,\"role\":\"assistant\"}" stub/request-3.json \
  || fail "round-1 partial missing from request 3"
grep -qF "\"content\":$P2,\"role\":\"assistant\"}]" stub/request-3.json \
  || fail "round-2 partial not the trailing assistant message in request 3"
[ ! -f stub/request-4.json ] || fail "unexpected fourth request (turn should have ended)"
echo "  ok: two continuations, each prefilling the accumulated response" >&2

echo "== the progress ref advanced to the turn's step-free tip ==" >&2
# A toolless turn mints no tool steps; the turn commit itself is the run's
# result and the canonical event ref advances to it.
[ -s turn.commit ] || fail "no turn commit emitted"
echo "  ok: turn commit emitted as the run result" >&2

echo "max-tokens: ALL PASS" >&2
