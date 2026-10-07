#!/usr/bin/env bash
# What std/caos-conversation prints for a LONG conversation: it must be possible
# to read one in pieces. A conversation of a few hundred entries overflowed what
# a caller can read, and `width` (which cuts only tool calls) did not help, so:
#
#   from/to     the entries printed, by the `[n]` numbers the listing shows
#   only        user | assistant | failed
#   msg-width   cut user and assistant message text
#   call-id     one call in full (NOT `call`: that name is reserved by llm-step,
#               which drops a `@param call`, so the tool never accepted it)
#
# The conversation is recorded through `caos mcp hook` only -- no tool server, no
# model -- so it holds user and assistant messages and no tool calls. What that
# leaves untested is `only=failed` against a real failing call and `call-id`
# against a real call id; both were run by hand against recorded sessions.
set -euo pipefail

# The recorded-workspace store lives under $HOME/.cache, and the worker's HOME
# is not writable.
HOME=$(mktemp -d)
export HOME

fail() { echo "FAIL: $*" >&2; exit 1; }

hook() { # <event JSON>
  printf '%s\n' "$1" | "$CAOS_CLI" mcp hook --llm-step:@=DEEP-DEPS/llm-step >/dev/null
}
prompt() { hook "$(jq -nc --arg s "$1" --arg p "$2" \
  '{hook_event_name:"UserPromptSubmit",session_id:$s,prompt:$p}')"; }
stop() { hook "$(jq -nc --arg s "$1" --arg m "$2" \
  '{hook_event_name:"Stop",session_id:$s,last_assistant_message:$m}')"; }
ref_of() {
  printf 'refs/caos/v3/conversations/%s/head' \
    "$(printf '%s' "$1" | od -An -tx1 | tr -d ' \n')"
}

stamp="$(date +%s)-$$"
session="conv-read-$stamp"

echo "== record three turns, the first with a long prompt ==" >&2
filler=$(printf 'x%.0s' $(seq 1 300))
prompt "$session" "first-prompt ${filler} TAIL-MARKER"
stop "$session" "first answer"
prompt "$session" "second prompt"
stop "$session" "second answer"
prompt "$session" "third prompt"
stop "$session" "third answer"
head=$(git rev-parse --verify -q "$(ref_of "cc/$session")") \
  || fail "the conversation was never recorded"

n=0
read_conv() { # <tool args...> ; prints the tool's output
  n=$((n + 1))
  "$CAOS_CLI" run "out$n" --base:@=DEEP-DEPS/caos-conversation --hash="$head" "$@" >&2
  cat "out$n"
}
count() { grep -c "$1" <<<"$2" || true; }

echo "== the default listing: everything, with the header ==" >&2
all=$(read_conv)
[ "$(count '^\[[0-9]*\] USER' "$all")" -eq 3 ] || fail "expected three user entries:
$all"
[ "$(count '^\[[0-9]*\] ASSISTANT' "$all")" -eq 3 ] || fail "expected three assistant entries:
$all"
grep -qF 'TAIL-MARKER' <<<"$all" || fail "messages are cut by default"
grep -qF 'calls: none' <<<"$all" || fail "no calls were made, and the header should say so"
! grep -qF 'showed ' <<<"$all" || fail "a full listing carries no 'showed' footer"

echo "== only=user and only=assistant ==" >&2
users=$(read_conv --only=user)
[ "$(count '^\[[0-9]*\] USER' "$users")" -eq 3 ] || fail "only=user lost a user entry"
[ "$(count 'ASSISTANT' "$users")" -eq 0 ] || fail "only=user printed an assistant entry"
grep -qF 'showed 3 of' <<<"$users" || fail "only=user has no footer: $users"
assistants=$(read_conv --only=assistant)
[ "$(count '^\[[0-9]*\] ASSISTANT' "$assistants")" -eq 3 ] || fail "only=assistant lost an entry"
[ "$(count '^\[[0-9]*\] USER' "$assistants")" -eq 0 ] || fail "only=assistant printed a user entry"

echo "== from and to select entries by number, inclusive ==" >&2
page=$(read_conv --from=1 --to=2)
grep -q '^\[1\] ' <<<"$page" || fail "from=1 did not print entry 1"
grep -q '^\[2\] ' <<<"$page" || fail "to=2 did not print entry 2"
! grep -q '^\[0\] ' <<<"$page" || fail "entry 0 is before from=1"
! grep -q '^\[3\] ' <<<"$page" || fail "entry 3 is after to=2"
grep -qF 'showed 2 of 6' <<<"$page" || fail "the footer does not count the page: $page"
late=$(read_conv --from=5)
[ "$(count '^\[[0-9]*\] ' "$late")" -eq 1 ] || fail "from=5 should print only the last entry:
$late"

echo "== msg-width cuts messages and says how to see the rest ==" >&2
cut=$(read_conv --msg-width=20)
! grep -qF 'TAIL-MARKER' <<<"$cut" || fail "msg-width=20 left the end of a long message"
grep -qF 'more chars; pass from=0 to=0 msg-width=0 to see all' <<<"$cut" \
  || fail "a cut message does not say how to read it: $cut"
grep -qF 'second prompt' <<<"$cut" || fail "msg-width cut a message shorter than it"

echo "== only=failed: user messages, and nothing else when no call failed ==" >&2
failed=$(read_conv --only=failed)
[ "$(count '^\[[0-9]*\] USER' "$failed")" -eq 3 ] || fail "only=failed lost the user messages"
[ "$(count 'ASSISTANT' "$failed")" -eq 0 ] || fail "only=failed printed an assistant entry"
! grep -qF 'entries with a failed call' <<<"$failed" || fail "no call failed, yet the header lists some"

echo "== bad values come back as text, not as a failure ==" >&2
bad=$(read_conv --only=nonsense)
grep -qF 'is not one of user, assistant, failed' <<<"$bad" || fail "bad only: $bad"
nocall=$(read_conv --call-id=toolu_nope)
grep -qF 'no call toolu_nope in this conversation' <<<"$nocall" || fail "bad call-id: $nocall"

echo "  ok" >&2
