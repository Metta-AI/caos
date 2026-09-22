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
  *'"name":"bash"'*) fail "an unresolvable step somehow offered the real tools" ;;
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
for tool in bash read ls write edit grep merge log show diff caos-build caos-test; do
  case "$listing" in
    *"\"name\":\"$tool\""*) ;;
    *) fail "the resolved listing is missing $tool" ;;
  esac
done
# caos_status rides ALONGSIDE the real tools, always: its provisioning
# diagnostics are wanted most on a working session you are trying to confirm.
case "$listing" in
  *caos_status*) ;;
  *) fail "caos_status vanished once the real tools resolved" ;;
esac

echo "== MCP resolves pinned generated paths on the client ==" >&2
# This foreign repo exists only in THIS client container. Server evaluation
# cannot fetch it; both help and invocation must consume the client handoff.
foreign=$(mktemp -d)
mkdir -p "$foreign/hello" "$foreign/help-only" generated
bash_image=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash)
cat > "$foreign/hello/.caos-expr" <<EXPR
HELP=<<END
Pinned generated tool.
@param word The word.
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
cat > "$foreign/help-only/.caos-expr" <<'EXPR'
HELP=<<END
Pinned help without target evaluation.
@param word The word.
END
run --base=invalid-image --help=$HELP
EXPR
git -C "$foreign" init -q
git -C "$foreign" add -A
git -C "$foreign" -c user.name=test -c user.email=test@caos commit -qm pinned-tools
revision=$(git -C "$foreign" rev-parse HEAD)
printf 'curry --base:docker=unused --tools:@@=git+file://%s?rev=%s\n' \
  "$foreign" "$revision" > generated/.caos-expr
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
help=$(call_tool 20 tool_help help-only)
case "$help" in
  *'"isError":false'*'Pinned help without target evaluation.'*|*'Pinned help without target evaluation.'*'"isError":false'*) ;;
  *) fail "pinned help evaluated the target or missed the generated path: $help" ;;
esac
invalid=$(call_tool 21 run_tool help-only)
case "$invalid" in
  *needs*word*) ;;
  *) fail "run_tool evaluated before validating arguments: $invalid" ;;
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
