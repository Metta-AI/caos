#!/usr/bin/env bash
# `/fork-caos-conversation <hash>` and `/resume-caos-conversation <hash>`,
# as the hook sees them on a new session's first prompt.
#
#   fork    the new session gets its OWN conversation, whose history is <hash>'s;
#           the original does not move.
#   resume  the new session records into the SAME conversation, on top of <hash>;
#           it gets no conversation of its own.
#   a head left open (a session that died mid-turn) is used as it is: the
#   protocol refuses to fork or continue a running request, so the hook closes
#   it as new commits on top, and the record says so.
#
# Driven through `caos mcp hook` only -- no tool server, no model. What that
# leaves untested is the closing of a CALL a dead session left running or
# declared; the request-only case below covers `settle`'s path through the
# terminal, and the call paths are what `llm-step`'s `drain` already does.
set -euo pipefail

# The recorded-workspace store lives under $HOME/.cache, and the worker's HOME
# is not writable.
HOME=$(mktemp -d)
export HOME

fail() { echo "FAIL: $*" >&2; exit 1; }

hook() { # <event JSON>; prints the hook's stdout
  printf '%s\n' "$1" | "$CAOS_CLI" mcp hook --llm-step:@=DEEP-DEPS/llm-step
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
head_of() { git rev-parse --verify -q "$(ref_of "$1")" || true; }
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

echo "== resume: the same branch ==" >&2
r="resume-r-$stamp"
out=$(prompt "$r" "/resume-caos-conversation $head_a")
case "$out" in
  *"resumed at"*) ;;
  *) fail "the hook did not say it resumed: $out" ;;
esac
[ -z "$(head_of "cc/$r")" ] || fail "a resumed session got a conversation of its own"
head_r=$(head_of "cc/$a")
[ "$head_r" != "$head_a" ] || fail "the resumed session recorded nothing on the original branch"
git merge-base --is-ancestor "$head_a" "$head_r" || fail "the branch was not extended"
stop "$r" "the resumed session is done"
case "$(subjects "$(ref_of "cc/$a")")" in
  "request.terminal model.complete request.claim request.admit message.append request.terminal "*) ;;
  *) fail "the resumed turn was not recorded as one: $(subjects "$(ref_of "cc/$a")")" ;;
esac

echo "== resume needs the head, and says so ==" >&2
# EXIT 2 exactly: it is the only status with which Claude Code blocks the prompt
# and shows the reason. Any other failure lets the prompt through with no
# conversation behind it and the reason dropped.
old_head=$head_a   # the branch has moved past it now
rc=0
prompt "resume-stale-$stamp" "/resume-caos-conversation $old_head" >/tmp/stale.out 2>/tmp/stale.err || rc=$?
[ "$rc" = 2 ] || fail "a resume from a stale head exited $rc, not 2: $(cat /tmp/stale.err)"
grep -q "not the head" /tmp/stale.err || fail "the refusal does not say why: $(cat /tmp/stale.err)"
rc=0
prompt "resume-bad-$stamp" "/resume-caos-conversation main" >/dev/null 2>/tmp/bad.err || rc=$?
[ "$rc" = 2 ] || fail "a resume naming a branch exited $rc, not 2: $(cat /tmp/bad.err)"
grep -q "40-character" /tmp/bad.err || fail "the refusal does not say what is wanted: $(cat /tmp/bad.err)"

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

h="resume-h-$stamp"
prompt "$h" "/resume-caos-conversation $head_b" >/dev/null
case "$(subjects "$(ref_of "cc/$b")")" in
  "request.claim request.admit message.append request.terminal "*) ;;
  *) fail "the open request was not closed on the resumed branch: $(subjects "$(ref_of "cc/$b")")" ;;
esac

echo "== a session's checkout is recorded once, and nothing else is consulted ==" >&2
# A later hook from a directory that is no repository still lands in the same
# conversation, because it reads the recorded entry rather than discovering.
w="resume-w-$stamp"
prompt "$w" "first prompt, records the checkout" >/dev/null
head_w=$(head_of "cc/$w")
[ -n "$head_w" ] || fail "the first prompt recorded no conversation"
elsewhere=$(mktemp -d)
(cd "$elsewhere" && env -u CLAUDE_PROJECT_DIR \
  "$CAOS_CLI" mcp hook --llm-step:@=DEEP-DEPS/llm-step <<<"$(jq -nc --arg s "$w" \
    '{hook_event_name:"Stop",session_id:$s,last_assistant_message:"from elsewhere"}')" >/dev/null) \
  || fail "a hook from another directory failed despite a recorded workspace"
new_head=$(head_of "cc/$w")
[ "$new_head" != "$head_w" ] || fail "the hook from another directory recorded nothing"
git merge-base --is-ancestor "$head_w" "$new_head" || fail "the new head does not descend from the old"

# No entry, wrong directory: the hooks fail, loudly, naming the session.
x="resume-x-$stamp"
rc=0
err=$(cd "$elsewhere" && env -u CLAUDE_PROJECT_DIR \
  "$CAOS_CLI" mcp hook --llm-step:@=DEEP-DEPS/llm-step 2>&1 >/dev/null <<<"$(jq -nc --arg s "$x" \
    '{hook_event_name:"PreToolUse",session_id:$s,tool_name:"mcp__caos__x"}')") || rc=$?
[ "$rc" = 2 ] || fail "PreToolUse with no recorded workspace exited $rc, not 2"
case "$err" in *"$x"*) ;; *) fail "the failure did not name the session: $err" ;; esac
rc=0
(cd "$elsewhere" && env -u CLAUDE_PROJECT_DIR \
  "$CAOS_CLI" mcp hook --llm-step:@=DEEP-DEPS/llm-step >/dev/null 2>&1 <<<"$(jq -nc --arg s "$x" \
    '{hook_event_name:"UserPromptSubmit",session_id:$s,prompt:"hi"}')") || rc=$?
[ "$rc" = 2 ] || fail "a first prompt outside any repository exited $rc, not 2"

echo "mcp-resume: ALL PASS" >&2
