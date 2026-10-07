#!/usr/bin/env bash
# What `caos mcp serve` must do, in the terms Claude Code judges it by.
#
# The contract is SYNCHRONOUS: `initialize` answers at once (it never resolves
# the step), and `tools/list` resolves the step and answers with the result --
# real tools, or, when the step cannot resolve, one `caos_status` tool that
# explains why. There is no background retry, no timed hold, and no
# `tools/list_changed` notification revising the list afterwards; the answer to
# the one `tools/list` a re-list-averse client makes is the final answer.
# (`warm`, run in the session-start hook before the client starts, normally
# resolves the tools ahead of all this so `tools/list` is a cache read.)
#
# The regressions this guards: resolving inside `initialize` blew the client's
# startup budget and the session reported a server that closed; and an empty
# tool list on failure made a model conclude caos is absent, so an empty
# registry carries the one tool that explains itself.
set -euo pipefail

# The recorded-workspace store lives under $HOME/.cache, and the worker's HOME
# is not writable.
HOME=$(mktemp -d)
export HOME

fail() { echo "FAIL: $*" >&2; exit 1; }

# One JSON-RPC exchange over the server's stdio, as a coprocess: the protocol
# is request/response with UNSOLICITED notifications interleaved, so a reader
# has to skip lines it did not ask for rather than assume the next line is its
# answer.
open_server() { # <llm-step argument>
  coproc SERVER { "$CAOS_CLI" mcp serve "$1" 2>/tmp/mcp-serve.err; }
  SERVER_IN=${SERVER[1]}
  SERVER_OUT=${SERVER[0]}
}

# WAITED FOR, not merely killed: bash refuses to start a second coprocess while
# the first is still known to it, and the second half of this test opens one.
close_server() {
  # Close our end of the coproc's stdin so serve sees EOF -- but ONLY if the fd
  # is still open. `exec {fd}>&-` on an already-closed fd is a redirection error,
  # which is FATAL in a non-interactive shell and NOT caught by `|| true`; and
  # bash auto-closes a coproc's fds the moment the coproc process exits. So a
  # serve that has already gone would kill this script here. Guard on the fd's
  # presence, and let kill+wait cover a serve still running.
  if [ -n "${SERVER_IN:-}" ] && [ -e "/proc/$$/fd/$SERVER_IN" ]; then
    exec {SERVER_IN}>&-
  fi
  if [ -n "${SERVER_PID:-}" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}

send() { printf '%s\n' "$1" >&"$SERVER_IN"; }

# Read lines until one matches, or give up. The timeout is the assertion in
# every case that uses it: what is being measured is whether an answer arrives
# at all within a budget, not what it says.
await() { # <grep pattern> <seconds> ; prints the matching line
  local pattern=$1 limit=$2 line deadline
  deadline=$(( $(date +%s) + limit ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if IFS= read -r -t "$limit" line <&"$SERVER_OUT"; then
      case "$line" in *"$pattern"*) printf '%s\n' "$line"; return 0 ;; esac
    else
      return 1
    fi
  done
  return 1
}

echo "== a step that cannot resolve still leaves the session a way to ask ==" >&2
open_server "--llm-step:@=DEEP-DEPS/no-such-entry"
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
# The handshake resolves NOTHING, so it answers at once. Ten seconds is a ceiling
# that must not scale with the step, which takes minutes to build the first time.
await '"id":1' 10 >/dev/null || fail "the handshake did not answer"

# tools/list resolves the step and answers with the result. This step cannot
# resolve, and a missing path fails FAST -- no build to wait out -- so the reply
# is prompt, and it carries the one tool a toolless session still offers.
send '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
listing=$(await '"id":2' 15) || fail "tools/list did not answer"
case "$listing" in
  *caos_status*) ;;
  *) fail "a server with no tools offered nothing to ask: $listing" ;;
esac
case "$listing" in
  *'"name":"read"'*) fail "an unresolvable step somehow offered the real tools" ;;
esac

# The REASON is settled by the time tools/list answered (it did the resolving),
# so caos_status names it on the first ask -- but the loop tolerates a slower
# path. A status that says only "no tools" costs the same round trip and settles
# nothing.
status=""
for id in 3 4 5 6 7 8; do
  send "{\"jsonrpc\":\"2.0\",\"id\":$id,\"method\":\"tools/call\",\"params\":{\"name\":\"caos_status\",\"arguments\":{}}}"
  status=$(await "\"id\":$id" 20) || fail "caos_status did not answer"
  case "$status" in *no-such-entry*) break ;; esac
  sleep 2
done
case "$status" in
  *no-such-entry*) ;;
  *) fail "caos_status never named what failed: $status" ;;
esac
close_server

echo "== the real step: the handshake is immediate, tools/list resolves then answers ==" >&2
open_server "--llm-step:@=DEEP-DEPS/llm-step"
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
started=$(date +%s)
await '"id":1' 10 >/dev/null || fail "the handshake waited for the step to resolve"
elapsed=$(( $(date +%s) - started ))
[ "$elapsed" -lt 10 ] || fail "the handshake took ${elapsed}s"

# GENEROUS, because this is the wait that used to sit on the handshake: the first
# resolution in a tree BUILDS the step. It now sits on tools/list, which holds
# its own reply until the tools are ready -- no notification, no re-ask.
send '{"jsonrpc":"2.0","id":9,"method":"tools/list"}'
listing=$(await '"id":9' 600) \
  || fail "the tools never resolved (see the server's stderr below)"
for tool in read ls write edit grep log show diff run_tool tool_help merge; do
  case "$listing" in
    *"\"name\":\"$tool\""*) ;;
    *) fail "the resolved listing is missing $tool" ;;
  esac
done
# AND THE std TOOLS ARE NOT THERE. Each is an entry with its own HELP, so a
# conversation whose tree mounts caos reaches it as `caos-std/<name>` --
# `tool_help` describes it and `run_tool` runs it, over the tree named by `in`
# (std/README.md is the index). Registering one as well would be a second way to
# call it, and the two can disagree about which version runs.
#
# `merge` is the exception above: it writes into a source tree and so needs that
# tree's COMMIT, which only the step's own routing supplies.
for tool in bash caos-build caos-test caos-test-result; do
  case "$listing" in
    *"\"name\":\"$tool\""*)
      fail "$tool is registered; std tools are reached by path through run_tool" ;;
  esac
done
# caos_status rides ALONGSIDE the real tools, always: its provisioning
# diagnostics are wanted most on a working session you are trying to confirm.
case "$listing" in
  *caos_status*) ;;
  *) fail "caos_status vanished once the real tools resolved" ;;
esac

echo "== MCP reaches a tool at a PINNED GENERATED path ==" >&2
# Two things at once, and the pin is what makes the second one real:
#   - the tool is at a path that exists only in the EVALUATION result
#     (`generated/args/tools/hello`), never in any stored tree;
#   - reaching it needs a `:@@=` locator resolved, and NOTHING HERE RESOLVES
#     ONE. `caos mcp` records the call and the step asks the server to evaluate
#     the path, so the fetch is the server's.
#
# This used to assert the opposite. The foreign repo was a client-container
# temp dir, unreachable from the server on purpose, and the test's subject was
# that both help and invocation consumed a CLIENT HANDOFF — `caos mcp serve`
# pre-resolving the path against the session's own checkout. That handoff is
# gone: it was a second resolver, and two resolvers can disagree about which
# version of a tool runs. So the repo is served where the server can fetch it,
# over this container's own address — the same route `chat-offline` points a
# stub at, reachable because a job's containers sit in the stack's netns.
foreign_base=$(mktemp -d)
foreign=$foreign_base/tools
mkdir -p "$foreign/hello" generated
bash_image=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash)
cat > "$foreign/hello/.caos-expr" <<EXPR
HELP=<<END
Pinned generated tool.
@param word The word.
@in
END
curry --base:hash=$bash_image --worker1:@=worker.sh --help=\$HELP
EXPR
cat > "$foreign/hello/worker.sh" <<'WORKER'
#!/usr/bin/env bash
set -euo pipefail
caos get /cas/args/in
caos get /cas/args/in/mcp-marker
caos get /cas/args/word
printf 'pinned %s %s' "$(cat /cas/args/word)" "$(cat /cas/args/in/mcp-marker)" > /tmp/out
caos put /tmp/out /cas/out
WORKER
git -C "$foreign" init -q
git -C "$foreign" config uploadpack.allowReachableSHA1InWant true
git -C "$foreign" add -A
git -C "$foreign" -c user.name=test -c user.email=test@caos commit -qm pinned-tools
revision=$(git -C "$foreign" rev-parse HEAD)

# Serve it where the SERVER can fetch it. Its own address, not 127.0.0.1: the
# fetch happens in the stack, so loopback here would be the wrong container.
[ -n "${CAOS_STUB_HOST:-}" ] || fail "dev/cli-test did not supply CAOS_STUB_HOST"
daemon_pid=""
stop_daemon() {
  if [ -n "$daemon_pid" ]; then kill "$daemon_pid" 2>/dev/null || true; fi
  daemon_pid=""
}
trap stop_daemon EXIT
for _ in 1 2 3 4 5; do
  git_port=$((20000 + RANDOM % 20000))
  git daemon --reuseaddr --export-all --listen=0.0.0.0 --port="$git_port" \
    --base-path="$foreign_base" "$foreign_base" >/tmp/tools-daemon.log 2>&1 &
  daemon_pid=$!
  for _ in $(seq 1 50); do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then break; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$git_port") 2>/dev/null; then exec 3>&-; break 2; fi
    sleep 0.1
  done
  stop_daemon
done
[ -n "$daemon_pid" ] || fail "could not serve the pinned tools: $(cat /tmp/tools-daemon.log)"

printf 'curry --base:docker=unused --tools:@@=git+git://%s:%s/tools?rev=%s\n' \
  "$CAOS_STUB_HOST" "$git_port" "$revision" > generated/.caos-expr
printf 'original-input' > mcp-marker
git add generated mcp-marker
git -c user.name=test -c user.email=test@caos commit -qm mcp-generated-tools
session="mcp-path-$(date +%s)-$$"
printf '{"hook_event_name":"UserPromptSubmit","session_id":"%s","prompt":"test generated tools"}\n' "$session" \
  | "$CAOS_CLI" mcp hook --llm-step:@=DEEP-DEPS/llm-step
key=$(printf 'cc/%s' "$session" | od -An -tx1 | tr -d ' \n')
head=$(git rev-parse "refs/caos/v3/conversations/$key/head")
# Discover the repository mount, so this test does not prescribe startup's
# source-tree prefix (or require a prefix if startup mounts content directly).
prefix=$(git ls-tree -r "$head" | awk '$1 == "160000" && !found {print $4; found=1}')
if [ -n "$prefix" ]; then prefix="$prefix/"; fi
tool_path="${prefix}generated/args/tools"
call_tool() { # <rpc id> <name> <leaf> [arguments JSON]
  local id=$1 name=$2 leaf=$3 arguments=${4:-'{}'}
  send "$(jq -nc --argjson id "$id" --arg name "$name" --arg session "$session" \
    --arg path "$tool_path/$leaf" --argjson arguments "$arguments" \
    '{jsonrpc:"2.0",id:$id,method:"tools/call",params:{name:$name,arguments:{
      caos_session:$session,caos_tool_use_id:("pinned-"+($id|tostring)),
      path:$path,arguments:$arguments}}}')"
  await "\"id\":$id" 300 || fail "pinned tool call $id did not answer"
}
help=$(call_tool 20 tool_help hello)
case "$help" in
  *'"isError":false'*'Pinned generated tool.'*|*'Pinned generated tool.'*'"isError":false'*) ;;
  *) fail "pinned help missed the generated path: $help" ;;
esac
invalid=$(call_tool 21 run_tool hello)
case "$invalid" in
  *needs*word*) ;;
  *) fail "run_tool did not reject missing arguments: $invalid" ;;
esac
ran=$(call_tool 22 run_tool hello '{"word":"supplied"}')
case "$ran" in
  *'pinned supplied original-input'*) ;;
  *) fail "pinned invocation lost its path, arguments, or original input: $ran" ;;
esac
missing=$(call_tool 23 tool_help missing)
case "$missing" in
  *'"isError":true'*missing*|*missing*'"isError":true'*) ;;
  *) fail "a missing generated path was not a recoverable tool error: $missing" ;;
esac

echo "== a call still running at Stop keeps its request, and its result ==" >&2
# Claude Code moves any MCP call past 120s to the background and ends the turn,
# so `Stop` fires while the call runs and the result arrives in a later turn --
# which Claude Code opens with a `UserPromptSubmit`. A Stop that closed the
# request left the call nowhere to record its completion, and the result reached
# the model as an EMPTY error ("Task failed: no detail"), while a re-run was an
# instant memo hit. The sleep stands in for the 120s.
mkdir -p slow
cat > slow/.caos-expr <<EXPR
HELP=<<END
Sleeps, then answers.
@param seconds How long to sleep.
@param nonce Makes each call a new job.
END
curry --base:hash=$bash_image --worker1:@=worker.sh --help=\$HELP
EXPR
cat > slow/worker.sh <<'WORKER'
#!/usr/bin/env bash
set -euo pipefail
caos get /cas/args/seconds
caos get /cas/args/nonce
sleep "$(cat /cas/args/seconds)"
printf 'slept %s' "$(cat /cas/args/seconds)" > /tmp/out
caos put /tmp/out /cas/out
WORKER
git add slow
git -c user.name=test -c user.email=test@caos commit -qm slow-tool
held="mcp-held-$(date +%s)-$$"
hook() { # <event JSON>
  printf '%s\n' "$1" | "$CAOS_CLI" mcp hook --llm-step:@=DEEP-DEPS/llm-step
}
hook "{\"hook_event_name\":\"UserPromptSubmit\",\"session_id\":\"$held\",\"prompt\":\"run slow\"}"
held_ref="refs/caos/v3/conversations/$(printf 'cc/%s' "$held" | od -An -tx1 | tr -d ' \n')/head"
send "$(jq -nc --arg session "$held" --arg path "${prefix}slow" --arg nonce "$held" \
  '{jsonrpc:"2.0",id:30,method:"tools/call",params:{name:"run_tool",arguments:{
    caos_session:$session,caos_tool_use_id:"held-30",path:$path,
    arguments:{seconds:"25",nonce:$nonce}}}}')"
# The Stop must land AFTER the call is declared, as a backgrounded call's does.
declared=""
for _ in $(seq 60); do
  if [ "$(git log -1 --format=%s "$held_ref")" = model.complete ]; then
    declared=yes
    break
  fi
  sleep 1
done
if [ -z "$declared" ]; then fail "the call was never declared"; fi
hook "{\"hook_event_name\":\"Stop\",\"session_id\":\"$held\",\"last_assistant_message\":\"moved to the background\"}"
if [ "$(git log -1 --format=%s "$held_ref")" = request.terminal ]; then
  fail "Stop closed a request whose call was still running"
fi
answer=$(await '"id":30' 300) || fail "the held call never answered"
case "$answer" in
  *'"isError":false'*'slept 25'*|*'slept 25'*'"isError":false'*) ;;
  *) fail "a call that outlived its turn lost its result: $answer" ;;
esac
# The turn that delivers the result closes the held request and opens its own,
# and that one's Stop closes it as usual.
hook "{\"hook_event_name\":\"UserPromptSubmit\",\"session_id\":\"$held\",\"prompt\":\"the result arrived\"}"
hook "{\"hook_event_name\":\"Stop\",\"session_id\":\"$held\",\"last_assistant_message\":\"done\"}"
subjects=$(git log --format=%s "$held_ref" | tr '\n' ' ')
case "$subjects" in
  "request.terminal model.complete request.claim request.admit message.append request.terminal "*) ;;
  *) fail "the held request was not closed before the next one opened: $subjects" ;;
esac

close_server

echo "== an unreachable caos server is named, not waited on ==" >&2
# LAST, because it breaks the remote for everything after it. A caos server
# reached through a dead tunnel ACCEPTS and never answers, so the resolution
# hangs rather than failing and the status can never improve -- which is what a
# real cloud container showed, twenty-seven seconds into "nothing has failed
# yet". A refused port is the testable half of that: it proves the probe runs
# and that its message names the server. The swallowing half is the same code
# path with a timeout instead of a refusal.
git remote set-url caos http://127.0.0.1:9
open_server "--llm-step:@=DEEP-DEPS/llm-step"
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
await '"id":1' 10 >/dev/null || fail "the handshake did not answer without a server"
status=""
for id in 2 3 4 5; do
  send "{\"jsonrpc\":\"2.0\",\"id\":$id,\"method\":\"tools/call\",\"params\":{\"name\":\"caos_status\",\"arguments\":{}}}"
  status=$(await "\"id\":$id" 20) || fail "caos_status did not answer"
  case "$status" in *"cannot reach the CAOS server"*) break ;; esac
  sleep 3
done
case "$status" in
  *"cannot reach the CAOS server"*) ;;
  *) fail "an unreachable server was not named: $status" ;;
esac
close_server

echo "mcp-serve: ALL PASS" >&2
