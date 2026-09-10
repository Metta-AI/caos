#!/usr/bin/env bash
# What `caos cc serve` must do, in the terms Claude Code judges it by.
#
# Both assertions here are regressions that already happened. The handshake
# used to resolve the step inline, which meant it did not answer inside the
# client's startup budget and the session reported a server that closed. The
# fix left an empty tool list on failure, and a model with an empty list
# reports that caos is absent -- so an empty registry now carries one tool that
# explains itself.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# One JSON-RPC exchange over the server's stdio, as a coprocess: the protocol
# is request/response with UNSOLICITED notifications interleaved, so a reader
# has to skip lines it did not ask for rather than assume the next line is its
# answer.
open_server() { # <llm-step argument>
  coproc SERVER { "$CAOS_CLI" cc serve "$1" 2>/tmp/cc-serve.err; }
  SERVER_IN=${SERVER[1]}
  SERVER_OUT=${SERVER[0]}
}

# WAITED FOR, not merely killed: bash refuses to start a second coprocess while
# the first is still known to it, and the second half of this test opens one.
close_server() {
  exec {SERVER_IN}>&- 2>/dev/null || true
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
# TEN SECONDS FOR A HANDSHAKE THAT RESOLVES NOTHING. The point is not that ten
# is the right number; it is that this must not scale with the step, which
# takes minutes to build the first time in a tree.
await '"id":1' 10 >/dev/null || fail "the handshake did not answer"

send '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
listing=$(await '"id":2' 10) || fail "tools/list did not answer"
case "$listing" in
  *caos_status*) ;;
  *) fail "a server with no tools offered nothing to ask: $listing" ;;
esac
case "$listing" in
  *'"name":"bash"'*) fail "an unresolvable step somehow offered the real tools" ;;
esac

# ASKED UNTIL IT KNOWS, because "still resolving, nothing has failed yet" is a
# correct answer before the first attempt returns, and a test that demands the
# failure immediately is testing the clock. What must hold is that a reason
# appears PROMPTLY -- not that it is there on the first ask.
status=""
for id in 3 4 5 6 7 8; do
  send "{\"jsonrpc\":\"2.0\",\"id\":$id,\"method\":\"tools/call\",\"params\":{\"name\":\"caos_status\",\"arguments\":{}}}"
  status=$(await "\"id\":$id" 20) || fail "caos_status did not answer"
  case "$status" in *no-such-entry*) break ;; esac
  sleep 2
done
# The REASON, not just an admission. A status that says only "no tools" costs
# the same round trip and settles nothing.
case "$status" in
  *no-such-entry*) ;;
  *) fail "caos_status never named what failed: $status" ;;
esac
close_server

echo "== the real step: the handshake is still immediate, tools arrive after ==" >&2
open_server "--llm-step:@=DEEP-DEPS/llm-step"
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
started=$(date +%s)
await '"id":1' 10 >/dev/null || fail "the handshake waited for the step to resolve"
elapsed=$(( $(date +%s) - started ))
[ "$elapsed" -lt 10 ] || fail "the handshake took ${elapsed}s"

# GENEROUS, because this is the wait the design exists to move off the
# handshake: the first resolution in a tree builds the step. It arrives as a
# notification, which is how a client learns to ask again.
await 'notifications/tools/list_changed' 600 >/dev/null \
  || fail "the tools never resolved (see the server's stderr below)"

send '{"jsonrpc":"2.0","id":9,"method":"tools/list"}'
listing=$(await '"id":9' 30) || fail "tools/list did not answer after the notification"
for tool in bash read ls write edit grep merge log show diff caos-build caos-test; do
  case "$listing" in
    *"\"name\":\"$tool\""*) ;;
    *) fail "the resolved listing is missing $tool" ;;
  esac
done
case "$listing" in
  *caos_status*) fail "caos_status is still offered once the real tools exist" ;;
esac
close_server

echo "cc-serve: ALL PASS" >&2
