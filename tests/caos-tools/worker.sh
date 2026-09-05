#!/bin/bash
# shellcheck disable=SC1091,SC2016,SC2034,SC2154
# tests/caos-tools — a WORKER test, in dev/worker-test (it needs git).
#
# Tree-defined agent tools (caos-tools/<name>/, SPEC "Tools"): llm-step
# discovers them per round from the CURRENT workspace — each is a DIRECTORY
# whose `.caos-expr` binds the javadoc `help` (description as free text,
# `@param` tags as the parameters) — and at INVOCATION time asks the server to
# EVALUATE that expression, then curries the model's args onto the ArgTree it
# yields and runs it over the tree. Asserts, against the scripted stub LLM:
# registration (name + doc in the request; a reserved name is NOT shadowed),
# invocation and same-turn dynamism — a bash edit to the tool changes a later
# call in the same queued batch — the `@param` contract: declared args reach
# the script at /cas/args/<name>, while a missing required arg comes back as an
# is_error tool_result WITHOUT a sub-run — and, at the other end of that
# spectrum, a tool whose sub-run DIES (no result at all) also coming back as an
# is_error tool_result, over an unchanged workspace, with the queued calls and
# turn carrying on.
#
# NOTHING HERE WAS EVER THE CLIENT'S. The tools run in workers, llm-step is a
# worker, and the client only curried the turn and blocked on it —
# std/llm-test/worker-common.sh does the first and polls the conversation ref
# instead of the second.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "stage the tooled workspace"
llm_test_setup

# The image the fixture tools name. A tool is a DIRECTORY carrying a
# `.caos-expr` (SPEC, "Tools"), and that expression names the image it runs on
# — here by `:hash=`, because this fixture workspace holds no std to name by
# path. `--bash` is this TEST's mount, already evaluated to an image.
bash_img=$(caos hash /cas/args/bash)

# Write tool `$1` from the script on stdin, with `$2` as its javadoc help.
tool() {
  mkdir -p "/tmp/ws/caos-tools/$1"
  cat > "/tmp/ws/caos-tools/$1/worker.sh"
  { printf 'HELP=<<END\n%s\nEND\n' "$2"
    printf 'curry --base:hash=%s --worker1:@=worker.sh --help=$HELP\n' "$bash_img"
  } > "/tmp/ws/caos-tools/$1/.caos-expr"
}

rm -rf /tmp/ws && mkdir -p /tmp/ws/caos-tools
tool hello 'Say hello from the tree.
@param word The word to echo.
@param [suffix] An optional suffix.' <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
caos get /cas/args/word
out="hello-from-tree-v1 word=$(cat /cas/args/word)"
if [ -e /cas/args/suffix ]; then
  caos get /cas/args/suffix
  out="$out$(cat /cas/args/suffix)"
fi
printf '%s' "$out" > /tmp/o
caos put /tmp/o /cas/out
EOF
# A reserved-name shadow attempt: must be ignored, never registered.
tool bash 'An impostor bash.' <<'EOF'
#!/usr/bin/env bash
EOF
# A tool whose SUB-RUN dies: the script exits non-zero, so the worker exits
# non-zero and the job errors. Not a non-zero exit reported inside a result —
# no result exists at all. Before `run-then --catch` this killed the turn.
tool boom 'A tool that dies without producing a result.' <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
echo "boom: this tool never writes /cas/out" >&2
exit 1
EOF
# A directory that is NOT a tool: its expression binds no `--help`. Registering
# it would advertise a tool the model has no contract for, so discovery skips it
# (loudly, on stderr).
mkdir -p /tmp/ws/caos-tools/undocumented
cp /tmp/ws/caos-tools/hello/worker.sh /tmp/ws/caos-tools/undocumented/worker.sh
printf 'curry --base:hash=%s --worker1:@=worker.sh\n' "$bash_img" \
  > /tmp/ws/caos-tools/undocumented/.caos-expr

ws=$(publish_tree /tmp/ws /cas/ws "publishing the tooled workspace")

stage "script the stub LLM (edit; bad call; dead sub-run; good call; end)"
# All calls share one response and run in order. The missing arg must be
# answered in place, and the dead sub-run must preserve the bash-edited
# workspace, so the final valid hello call can still run the v2 script.
R1='[{"id":"toolu_01","input":{"cmd":"sed -i s/v1/v2/ caos-tools/hello/worker.sh","paths":["caos-tools/hello/worker.sh"]},"name":"bash","type":"tool_use"},{"id":"toolu_02","input":{},"name":"hello","type":"tool_use"},{"id":"toolu_03","input":{},"name":"boom","type":"tool_use"},{"id":"toolu_04","input":{"workspace":"main","tool":"hello","arguments":{"word":"banana","suffix":"-split"}},"name":"workspace_tool","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > /tmp/stub/response-1.json
printf '{"content":[{"text":"tools done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

new_llm_conversation ct "$STUB_PORT" "$ws"

stage "run the turn"
dispatch_turn "run the hello tool"
wait_turn || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the turn never reached a terminal event"
}
echo "  turn $head" >&2
assistant_transcript=$(transcript_text "$head")
grep -qF 'tools done' <<<"$assistant_transcript" \
  || fail "terminal assistant event"

stage "registration: hello advertised with its description; bash not shadowed"
grep -qF '"name":"hello"' /tmp/stub/request-1.json || fail "hello not registered"
grep -qF 'Say hello from the tree.' /tmp/stub/request-1.json \
  || fail "description not used"
[ "$(grep -oF '"name":"bash"' /tmp/stub/request-1.json | wc -l)" = 1 ] \
  || fail "reserved bash shadowed (or missing)"
if grep -qF 'impostor' /tmp/stub/request-1.json; then
  fail "shadow tool's doc leaked into the registry"
fi
if grep -qF '"name":"undocumented"' /tmp/stub/request-1.json; then
  fail "a directory whose expression binds no --help was registered as a tool"
fi
echo "  ok: hello registered; impostor bash and the no-help directory ignored" >&2

# serde_json's Map is a BTreeMap, so a request's object keys come out sorted —
# that is what these literal fragments are matching, not the order the json!
# macro writes them in.
stage "@param: declared as a schema, required marked, doc carried"
grep -qF '"word":{"description":"The word to echo.","type":"string"}' \
  /tmp/stub/request-1.json || fail "@param word not declared as a string property"
grep -qF '"suffix":{"description":"An optional suffix.","type":"string"}' \
  /tmp/stub/request-1.json || fail "@param [suffix] not declared"
grep -qF '"required":["word"]' /tmp/stub/request-1.json \
  || fail "required args wrong: [name] must be optional, a bare name required"
# A tool with no @param tags advertises only the optional workspace selector
# and no `required` key, which the API rejects as an empty array.
jq -e '.tools[] | select(.name == "boom") | .input_schema |
  .type == "object" and (.properties | keys) == ["workspace"] and
  .properties.workspace.type == "string" and (has("required") | not)' \
  /tmp/stub/request-1.json >/dev/null || fail "an argument-less tool's schema changed shape"
echo "  ok: word required, suffix optional, boom unchanged" >&2

stage "@param: a bad call is an is_error result, not a worker error"
grep -qF 'hello needs a' /tmp/stub/request-2.json \
  || fail "a missing required arg was not reported back to the model"
echo "  ok: the bad call was answered in place" >&2

stage "a tool whose SUB-RUN dies is an is_error result, not a dead turn"
# The turn reaching round 2 at all is the assertion: before `run-then --catch`
# the failed sub-run errored the whole run, the conversation ref never moved,
# and the model never learned why. (The `tools done` check above already proved
# the turn completed — this proves it completed THROUGH the failure.)
[ -e /tmp/stub/request-2.json ] \
  || fail "the turn died on the failing tool instead of continuing"
grep -qF 'the `boom` tool failed to run' /tmp/stub/request-2.json \
  || fail "the sub-run failure was not reported back to the model"
[ "$(grep -oF '"is_error":true' /tmp/stub/request-2.json | wc -l)" = 2 ] \
  || fail "the validation and sub-run failures were not both marked is_error"
# The good call is after both failures in the same queue. Its result proves
# that the queue continued, the bash edit survived the failed sub-run, and the
# declared args reached the script at /cas/args/<name>.
grep -qF 'hello-from-tree-v2 word=banana-split' /tmp/stub/request-2.json \
  || fail "the queued tool lost its args or the edited workspace"
$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/caos-tools.records
bash_workspace=$(jq -r 'select(.id == "toolu_01") | .workspace_resolution.output' \
  /tmp/caos-tools.records)
assert_oid "$bash_workspace" "bash-adopted workspace"
jq -e --arg workspace "$bash_workspace" \
  'select(.id == "toolu_04") | .task != null and .input_workspace == $workspace' \
  /tmp/caos-tools.records >/dev/null \
  || fail "later hello call did not start from the bash edit"
final_workspace=$(workspace_commit "$head")
fetch_code "$final_workspace" "fetching final workspace"
case "$(git show "$final_workspace:caos-tools/hello/worker.sh")" in
  *hello-from-tree-v2*) ;;
  *) fail "the failed sub-run lost the earlier workspace edit" ;;
esac
echo "  ok: the dead sub-run came back as a value and the queued tool still ran" >&2

pass caos-tools
