#!/usr/bin/env bash
# What `caos mcp serve` must do, in the terms Claude Code judges it by.
#
# The tools are FIVE AND FIXED -- run, help, start, result, eval-path -- so the
# contract is plain and synchronous. `initialize` answers at once and carries the
# `instructions` that tell the model how the five fit together. `tools/list` is a
# CONSTANT: it needs no step, no cache, no warm-up hook and no server, so there is
# nothing for a re-list-averse client to miss. A call is one request to the server,
# and what a tool runs is named by a PATH in the conversation.
#
# The regressions this guards: a listing that waited on a worker run (a first
# session in a tree built the step before the client's startup budget ran out, and
# the session reported a server that closed); and a model handed zero tools that
# concluded caos was absent. `help` with no path is how a session finds out the
# truth about caos, so it must answer with no workspace and with no server.
set -euo pipefail

# `help` writes its diag journal under $HOME/.cache, and the worker's HOME
# is not writable.
HOME=$(mktemp -d)
export HOME

fail() { echo "FAIL: $*" >&2; exit 1; }

# One JSON-RPC exchange over the server's stdio, as a coprocess: the protocol
# is request/response with UNSOLICITED notifications interleaved, so a reader
# has to skip lines it did not ask for rather than assume the next line is its
# answer.
open_server() { # <arguments: the server and the step>
  coproc SERVER { "$CAOS_CLI" mcp serve $1 2>/tmp/mcp-serve.err; }
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

echo "== the handshake and the list need nothing: no step, no server, no cache ==" >&2
# A step that does not exist: nothing in a handshake or a listing may touch it.
open_server "--server=http://127.0.0.1:9 --llm-step:hash=0000000000000000000000000000000000000000"
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
init=$(await '"id":1' 10) || fail "the handshake did not answer"
# The instructions are how the model learns what to do with five tools and no
# listing: where the standard ones are, how to find the index, what each primitive is.
for needle in 'instructions' 'help(path)' 'run(path' 'eval-path' 'caos-std'; do
  case "$init" in
    *"$needle"*) ;;
    *) fail "the handshake's instructions lack $needle: $init" ;;
  esac
done

send '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
listing=$(await '"id":2' 10) || fail "tools/list did not answer without a step"
names=$(printf '%s' "$listing" | jq -r '.result.tools[].name' | tr '\n' ' ')
[ "$names" = "run help start result eval-path " ] \
  || fail "the list is not the five fixed tools, in order: $names"
# EVERY TOOL accepts what the PreToolUse hook injects, or the hook's `updatedInput`
# would fail schema validation on a call the model made correctly.
printf '%s' "$listing" | jq -e '[.result.tools[].inputSchema.properties | has("caos_session")] | all' >/dev/null \
  || fail "a tool does not declare the injected caos_session: $listing"

# `help` with no path is the one call that needs no conversation: it reports on caos
# itself, which is the only diagnostic a locked-down session has.
send '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"help","arguments":{}}}'
overview=$(await '"id":3' 20) || fail "help with no path did not answer"
case "$overview" in
  *'"isError":false'*'--- caos diagnostics ---'*) ;;
  *'--- caos diagnostics ---'*'"isError":false'*) ;;
  *) fail "the overview is not an answer with diagnostics: $overview" ;;
esac
close_server

echo "== the real step: the same five tools, and none of the old registry ==" >&2
open_server "$CAOS_MCP_ARGS"
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
await '"id":1' 10 >/dev/null || fail "the handshake did not answer"
send '{"jsonrpc":"2.0","id":9,"method":"tools/list"}'
listing=$(await '"id":9' 10) || fail "tools/list did not answer"
# NOTHING OF THE STEP'S REGISTRY IS LISTED. The file tools, the history tools, the
# std tools and the step's own `run_tool`/`tool_help` are all reached by path through
# `run` and `help`; a listing that named them would be a second way to call one thing,
# and the two can disagree about which version runs.
for gone in caos_status read ls write edit grep log show diff merge run_tool tool_help bash caos-build caos-test; do
  case "$listing" in
    *"\"name\":\"$gone\""*) fail "$gone is listed: the tools are the five fixed ones, and the rest are paths" ;;
  esac
done

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
  | "$CAOS_CLI" mcp hook $CAOS_MCP_ARGS "$(mcp_base_arg)"
key=$(printf 'cc/%s' "$session" | od -An -tx1 | tr -d ' \n')
# The prompt hook recorded it on the server; fetch it.
git fetch -q caos "+refs/caos/v3/conversations/$key/head:refs/caos/v3/conversations/$key/head"
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
help=$(call_tool 20 help hello)
case "$help" in
  *'"isError":false'*'Pinned generated tool.'*|*'Pinned generated tool.'*'"isError":false'*) ;;
  *) fail "pinned help missed the generated path: $help" ;;
esac
invalid=$(call_tool 21 run hello)
case "$invalid" in
  *needs*word*) ;;
  *) fail "run did not reject missing arguments: $invalid" ;;
esac
ran=$(call_tool 22 run hello '{"word":"supplied"}')
case "$ran" in
  *'pinned supplied original-input'*) ;;
  *) fail "pinned invocation lost its path, arguments, or original input: $ran" ;;
esac
missing=$(call_tool 23 help missing)
case "$missing" in
  *'"isError":true'*missing*|*missing*'"isError":true'*) ;;
  *) fail "a missing generated path was not a recoverable tool error: $missing" ;;
esac

echo "== a raw locator names a subtree unevaluated ==" >&2
# `raw=1&dir=hello` is the directory as the commit holds it, `.caos-expr` and all;
# without `raw` the same locator evaluates it into the tool's image.
locator="git+git://$CAOS_STUB_HOST:$git_port/tools?rev=$revision&dir=hello"
raw_answer=$(curl -fsS "$CAOS_SERVER_URL/eval?root:@@=$(jq -rn --arg l "$locator&raw=1" '$l|@uri')") \
  || fail "the server refused a raw locator with a dir"
[ "${raw_answer%% *}" = tree ] || fail "a raw dir is not a tree: $raw_answer"
"$CAOS_CLI" get "${raw_answer##* }" /tmp/raw-hello >/dev/null || fail "reading the raw tree"
[ -f /tmp/raw-hello/.caos-expr ] || fail "the raw tree was evaluated: it has no .caos-expr ($(ls -A /tmp/raw-hello))"
evaluated=$(curl -fsS "$CAOS_SERVER_URL/eval?root:@@=$(jq -rn --arg l "$locator" '$l|@uri')") \
  || fail "the server refused the evaluated locator"
[ "${evaluated##* }" != "${raw_answer##* }" ] || fail "raw and evaluated named the same tree"
missing=$(curl -s -o /dev/null -w '%{http_code}' "$CAOS_SERVER_URL/eval?root:@@=$(jq -rn --arg l "${locator%dir=hello}raw=1&dir=nope" '$l|@uri')")
[ "$missing" = 404 ] || fail "a raw dir that is not there answered $missing, not 404"

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
@param request-id Makes each call a new job.
END
curry --base:hash=$bash_image --worker1:@=worker.sh --help=\$HELP
EXPR
cat > slow/worker.sh <<'WORKER'
#!/usr/bin/env bash
set -euo pipefail
caos get /cas/args/seconds
caos get /cas/args/request-id
sleep "$(cat /cas/args/seconds)"
printf 'slept %s' "$(cat /cas/args/seconds)" > /tmp/out
caos put /tmp/out /cas/out
WORKER
git add slow
git -c user.name=test -c user.email=test@caos commit -qm slow-tool
held="mcp-held-$(date +%s)-$$"
hook() { # <event JSON>
  printf '%s\n' "$1" | "$CAOS_CLI" mcp hook $CAOS_MCP_ARGS "$(mcp_base_arg)"
}
hook "{\"hook_event_name\":\"UserPromptSubmit\",\"session_id\":\"$held\",\"prompt\":\"run slow\"}"
held_ref="refs/caos/v3/conversations/$(printf 'cc/%s' "$held" | od -An -tx1 | tr -d ' \n')/head"
send "$(jq -nc --arg session "$held" --arg path "${prefix}slow" --arg request_id "$held" \
  '{jsonrpc:"2.0",id:30,method:"tools/call",params:{name:"run",arguments:{
    caos_session:$session,caos_tool_use_id:"held-30",path:$path,
    arguments:{seconds:"25","request-id":$request_id}}}}')"
# The Stop must land AFTER the call is declared, as a backgrounded call's does.
#
# The STEP declares the call now, on the server, a moment after the request
# reaches it, so the declaration is not in this checkout's copy of the ref:
# fetch it. And it is not the newest commit for long -- the step starts the tool
# right behind it -- so look for it among the last few.
sync_held() { git fetch -q caos "+$held_ref:$held_ref"; }
declared=""
for _ in $(seq 60); do
  if sync_held 2>/dev/null; then
    case "$(git log -4 --format=%s "$held_ref")" in
      *model.complete*) declared=yes; break ;;
    esac
  fi
  sleep 1
done
if [ -z "$declared" ]; then fail "the call was never declared"; fi
hook "{\"hook_event_name\":\"Stop\",\"session_id\":\"$held\",\"last_assistant_message\":\"moved to the background\"}"
sync_held
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
sync_held
subjects=$(git log --format=%s "$held_ref" | tr '\n' ' ')
case "$subjects" in
  "request.terminal model.complete request.claim request.admit message.append request.terminal "*) ;;
  *) fail "the held request was not closed before the next one opened: $subjects" ;;
esac

close_server

echo "== an unreachable caos server is named, not waited on ==" >&2
# A server named at an address nothing listens on, keeping the step the session
# would have used. A caos server reached through a dead tunnel ACCEPTS and never
# answers, so anything that dialled it unbounded would hang and say nothing -- which
# a real cloud container showed. A refused port is the testable half: it proves the
# probe runs and that its message names the server. The swallowing half is the same
# code path with a timeout instead of a refusal.
open_server "--server=http://127.0.0.1:9 ${CAOS_MCP_ARGS#* }"
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
await '"id":1' 10 >/dev/null || fail "the handshake did not answer without a server"
# `help` with no path, for a session: it says
# the server cannot be reached rather than hanging on it.
send "$(jq -nc --arg session "$session" \
  '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:"help",arguments:{caos_session:$session}}}')"
status=$(await '"id":2' 30) || fail "help did not answer while the server was unreachable"
case "$status" in
  *"cannot reach the CAOS server"*) ;;
  *) fail "an unreachable server was not named: $status" ;;
esac
# And a call that needs the server fails promptly, saying so, instead of waiting.
send "$(jq -nc --arg session "$session" \
  '{jsonrpc:"2.0",id:3,method:"tools/call",params:{name:"run",arguments:{caos_session:$session,caos_tool_use_id:"unreachable-1",path:"caos-std/read",arguments:{"file-path":"README.md"}}}}')"
refused=$(await '"id":3' 120) || fail "a call did not fail promptly while the server was unreachable"
case "$refused" in
  *"cannot reach the CAOS server"*|*"no conversation"*|*"cannot open"*) ;;
  *) fail "an unreachable server's call did not say why: $refused" ;;
esac
close_server

echo "mcp-serve: ALL PASS" >&2
