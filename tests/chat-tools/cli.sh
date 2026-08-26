#!/usr/bin/env bash
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI):
#
# The subject here IS the tested client's `chat` subcommand — specifically its
# inline file-tool loop, which lives nowhere else. When a scripted model turn
# returns a `tool_use` block, it is the CLIENT that must:
#   - dispatch each call to the built-in file tools (write/read/edit/ls),
#   - APPLY those tools to the conversation turn's git tree (we assert the turn
#     tree ends with `goodbye world` and keeps the untouched sibling file),
#   - emit the per-tool progress lines it prints to the user,
#   - marshal every result back as a `tool_result` in the NEXT model request —
#     the exact byte-level payloads (`wrote … (11 bytes)`, `edited … (1
#     replacement)`, the ls listing, the read content), AND mark a failed edit
#     with `"is_error":true` plus the `old_string not found` explanation,
#   - and do all of that within ONE extra model round (request-3 must not exist).
#
# Every one of those properties is a behaviour of the client binary compiled
# from the tree under test: the model is stubbed, the tools run in-process, and
# the only way to observe the loop — the tree it writes, the lines it prints,
# and the request it sends back — is to let a real `chat` invocation execute it.
# There is no host-side script that reproduces this; a different client build
# would answer differently, which is exactly the property we pin down. Hence the
# test drives $CAOS_CLI directly rather than exercising the subject some other
# way (contrast tests/std-lint, whose subject is inert scripts).
#
# Five calls share one model response and no compute sub-run; mixed queues and
# grep are independent sibling tests rather than later turns in this conversation.
set -euo pipefail
# The dependency is mounted only inside the test wrapper and exports globals.
# shellcheck disable=SC1091
source DEEP-DEPS/llm-test/common.sh

stage "workspace and scripted inline-tool turn"
llm_test_setup
stub_host="${stub_host:?llm_test_setup did not set stub_host}"
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
commit "workspace"
git config user.name tester
git config user.email tester@example.com
base=$(mkcommit "HEAD:ws" base)

INLINE_CALLS='[
 {"id":"tu_w","input":{"file_path":"notes/new.txt","content":"hello world"},"name":"write","type":"tool_use"},
 {"id":"tu_r","input":{"file_path":"notes/new.txt"},"name":"read","type":"tool_use"},
 {"id":"tu_e","input":{"file_path":"notes/new.txt","old_string":"hello","new_string":"goodbye"},"name":"edit","type":"tool_use"},
 {"id":"tu_x","input":{"file_path":"notes/new.txt","old_string":"never there","new_string":"x"},"name":"edit","type":"tool_use"},
 {"id":"tu_l","input":{"path":"notes"},"name":"ls","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$INLINE_CALLS" | tr -d '\n')" > stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"file tools done","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-2.json
stub_pid=""
port=""
start_stub stub stub_pid port
stub_pid="${stub_pid:?start_stub did not set stub_pid}"

test_run_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_run_id}-tools-inline"
conversation_ref="refs/caos/v2/conversations/$conv/head"
opts=(--model test-model --base-url "http://$stub_host:$port")

stage "write, read, edit, failed edit, and ls in one round"
"$CAOS_CLI" chat "$conv" -m "exercise the file tools" \
  --base "$base" "${opts[@]}" > inline.out
while IFS= read -r line; do echo "  inline| $line" >&2; done < inline.out
turn=$(remote_tip "$conversation_ref") || fail "inline-tool conversation has no head"
git fetch -q caos "$turn"
[ "$(git show "$turn:notes/new.txt")" = "goodbye world" ] \
  || fail "write+edit did not land in the turn tree"
[ "$(git show "$turn:notes/todo.txt")" = "hello notes" ] || fail "sibling file lost"
grep -qF "write notes/new.txt" inline.out || fail "write progress line missing"
grep -qF "read notes/new.txt" inline.out || fail "read progress line missing"
grep -qF "ls notes" inline.out || fail "ls progress line missing"
grep -qF '"hello world"' stub/request-2.json || fail "read result not sent"
grep -qF 'wrote notes/new.txt (11 bytes)' stub/request-2.json || fail "write result not sent"
grep -qF 'edited notes/new.txt (1 replacement)' stub/request-2.json || fail "edit result not sent"
grep -qF 'new.txt\ntodo.txt' stub/request-2.json || fail "ls listing not sent"
grep -qF '"is_error":true' stub/request-2.json || fail "bad edit not marked is_error"
grep -qF 'old_string not found' stub/request-2.json || fail "bad edit error not explained"
[ ! -f stub/request-3.json ] || fail "inline tools cost extra model rounds"

stage "done"
echo "chat-tools: ALL PASS" >&2
