#!/usr/bin/env bash
# `/fork-caos-conversation <hash>`, as the hook sees it on a new session's first
# prompt: the new session gets its OWN conversation, whose history is <hash>'s;
# the original does not move.
#
# A head left open (a session that died mid-turn) is forked as it is: a
# conversation is a flat transcript, so the fork copies it unchanged, open
# request and all, and the record shows the fork landing straight on top.
#
# Driven through `caos mcp hook` only -- no tool server, no model.
set -euo pipefail

# The hook's diag journal lives under $HOME/.cache, and the worker's HOME
# is not writable.
HOME=$(mktemp -d)
export HOME

fail() { echo "FAIL: $*" >&2; exit 1; }

hook() { # <event JSON>; prints the hook's stdout
  printf "%s\n" "$1" | "$CAOS_CLI" mcp hook $CAOS_MCP_ARGS "$(mcp_base_arg)"
}
prompt() { # <session> <prompt text>
  hook "$(jq -nc --arg s "$1" --arg p "$2" \
    '{hook_event_name:"UserPromptSubmit",session_id:$s,prompt:$p}')"
}
stop() { # <session> <closing message>
  hook "$(jq -nc --arg s "$1" --arg m "$2" \
    '{hook_event_name:"Stop",session_id:$s,last_assistant_message:$m}')" >/dev/null
}
ref_of() { # <conversation id>
  printf 'refs/caos/v3/conversations/%s/head' \
    "$(printf '%s' "$1" | od -An -tx1 | tr -d ' \n')"
}
# The step writes the conversation on the server, so this checkout has only
# what it fetches: fetch the ref before reading it.
head_of() {
  git fetch -q caos "+$(ref_of "$1"):$(ref_of "$1")" 2>/dev/null || true
  git rev-parse --verify -q "$(ref_of "$1")" || true
}
subjects() { git log --format=%s "$1" | tr '\n' ' '; }

stamp="$(date +%s)-$$"

echo "== a finished conversation to pick up ==" >&2
a="resume-a-$stamp"
prompt "$a" "first prompt of the original" >/dev/null
stop "$a" "the original is done"
head_a=$(head_of "cc/$a")
[ -n "$head_a" ] || fail "the original conversation was never recorded"

echo "== fork: a new branch from the same commit ==" >&2
f="resume-f-$stamp"
out=$(prompt "$f" "/fork-caos-conversation $head_a")
case "$out" in
  *"forked from $head_a"*) ;;
  *) fail "the hook did not say it forked: $out" ;;
esac
head_f=$(head_of "cc/$f")
[ -n "$head_f" ] || fail "the fork has no conversation of its own"
[ "$(head_of "cc/$a")" = "$head_a" ] || fail "forking moved the original"
git merge-base --is-ancestor "$head_a" "$head_f" || fail "the fork does not descend from $head_a"
identity=$(git show "$head_f:.caos/identity.json")
case "$identity" in
  *"\"id\":\"cc/$f\""*) ;;
  *) fail "the fork carries the wrong conversation id: $identity" ;;
esac
git grep -q "first prompt of the original" "$head_f" -- .caos/transcript \
  || fail "the fork lost the original's transcript"
case "$(subjects "$head_f")" in
  *"conversation.fork "*) ;;
  *) fail "no conversation.fork on the fork's branch: $(subjects "$head_f")" ;;
esac
# A second prompt is an ordinary one: the session already has its conversation.
prompt "$f" "/fork-caos-conversation $head_a" >/dev/null
stop "$f" "forked and done"

echo "== a head left open is used as it is ==" >&2
# A session that died mid-turn: its prompt was recorded, its Stop never fired.
b="resume-b-$stamp"
prompt "$b" "a turn that never finished" >/dev/null
head_b=$(head_of "cc/$b")
[ -n "$head_b" ] || fail "the open conversation was never recorded"
case "$(subjects "$head_b")" in
  "request.claim "*) ;;
  *) fail "expected an open request at the head: $(subjects "$head_b")" ;;
esac

g="resume-g-$stamp"
prompt "$g" "/fork-caos-conversation $head_b" >/dev/null
head_g=$(head_of "cc/$g")
[ -n "$head_g" ] || fail "an open head could not be forked"
case "$(subjects "$head_g")" in
  *"conversation.fork request.terminal "*) ;;
  *) fail "the open request was not closed before the fork: $(subjects "$head_g")" ;;
esac
[ "$(head_of "cc/$b")" = "$head_b" ] || fail "forking an open head moved the original"

echo "== a prompt with no server is refused, naming what is missing ==" >&2
# Exit 2 is the one status with which Claude Code blocks the prompt and shows the
# reason; any other lets it through with nothing recorded.
rc=0
err=$(env -u CAOS_SERVER_URL -u CAOS_MCP_DEFAULT_SERVER "$CAOS_CLI" mcp hook \
  --llm-step:hash=0000000000000000000000000000000000000000 2>&1 >/dev/null \
  <<<"$(jq -nc '{hook_event_name:"UserPromptSubmit",session_id:"resume-x",prompt:"hi"}')") || rc=$?
[ "$rc" = 2 ] || fail "a first prompt with no server exited $rc, not 2"
case "$err" in *"--server=<url>"*) ;; *) fail "the refusal does not say how to name a server: $err" ;; esac

echo "mcp-fork: ALL PASS" >&2
