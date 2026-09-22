#!/bin/bash
# shellcheck disable=SC1091,SC2016,SC2034,SC2154
# tests/caos-tools — a WORKER test, in dev/worker-test (it needs git).
#
# Repository tools (SPEC "CaosTools"): a tool is any DIRECTORY whose
# `.caos-expr` binds the javadoc `help` (description as free text, `@param` tags
# as the parameters), addressed by PATH. Both calls evaluate the path to get
# the image. `tool_help` reads its help; `run_tool` validates arguments, curries
# them onto the image and runs it over the original source tree.
#
# NOTHING ENUMERATES THE TOOLS, and the first stage below asserts exactly that:
# the declared tool list holds `tool_help`/`run_tool` and no `hello`, and the
# system prompt carries no per-source-tree schema block. That is what makes the
# list tree-independent, so a conversation that gains a source tree mid-run
# neither re-keys the prompt nor grows it.
#
# Asserts, against the scripted stub LLM: description through `tool_help`
# (doc + required/optional params) and its two not-a-tool answers; invocation
# and same-turn dynamism — a bash edit to the tool changes a later call in the
# same queued batch — the `@param` contract: declared args reach the script at
# /cas/args/<name>, while a missing required arg comes back as an is_error
# tool_result WITHOUT a sub-run — and, at the other end of that spectrum, a tool
# whose sub-run DIES (no result at all) also coming back as an is_error
# tool_result, over an unchanged source tree, with the queued calls and turn
# carrying on.
#
# NOTHING HERE WAS EVER THE CLIENT'S. The tools run in workers, llm-step is a
# worker, and the client only curried the turn and blocked on it —
# std/llm-test/worker-common.sh does the first and polls the conversation ref
# instead of the second.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "stage the tooled source_tree"
llm_test_setup

# The image the fixture tools name. A tool is a DIRECTORY carrying a
# `.caos-expr` (SPEC, "CaosTools"), and that expression names the image it runs on
# — here by `:hash=`, because this fixture source tree holds no std to name by
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
# A directory NAMED like a built-in. Nothing can shadow `bash` any more — a
# repository tool is reached by path, never by name — so this must simply not
# affect the built-in `bash` the model is offered.
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
# A directory that is NOT a tool: its expression binds no `--help`. `tool_help`
# has to say so distinctly from "no such path", because the two have different
# fixes.
mkdir -p /tmp/ws/caos-tools/undocumented
cp /tmp/ws/caos-tools/hello/worker.sh /tmp/ws/caos-tools/undocumented/worker.sh
printf 'curry --base:hash=%s --worker1:@=worker.sh\n' "$bash_img" \
  > /tmp/ws/caos-tools/undocumented/.caos-expr

# An ancestor computes a tools/ directory absent from the stored source tree.
# The target's input must still be the ORIGINAL source tree, with this marker.
printf 'original-source' > /tmp/ws/input-marker
tool generated 'Generated tool.
@param word The supplied word.' <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
caos get /cas/args/in
caos get /cas/args/in/input-marker
caos get /cas/args/word
printf 'generated word=%s input=%s' "$(cat /cas/args/word)" \
  "$(cat /cas/args/in/input-marker)" > /tmp/o
caos put /tmp/o /cas/out
EOF
mkdir -p /tmp/ws/generator/templates
mv /tmp/ws/caos-tools/generated /tmp/ws/generator/templates/hello
cat > /tmp/ws/generator/generate.sh <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
caos get /cas/args/in
caos get /cas/args/in/templates
mkdir -p /tmp/generated
ln -s /cas/args/in/templates /tmp/generated/tools
caos put /tmp/generated /cas/out
EOF
printf 'run --base:hash=%s --worker1:@=generate.sh --in:@=.\n' "$bash_img" \
  > /tmp/ws/generator/.caos-expr

ws=$(publish_tree /tmp/ws /cas/ws "publishing the tooled source_tree")

stage "script the stub LLM (describe; edit; bad call; dead sub-run; good call)"
# All calls share one response and run in order. `tool_help` comes first,
# because that is the order a model works in with nothing listing the tools:
# describe the path, then run it. The missing arg must be answered in place, and
# the dead sub-run must preserve the bash-edited source tree, so the final valid
# hello call can still run the v2 script.
R1='[{"id":"generated-help","input":{"path":"main/generator/tools/hello"},"name":"tool_help","type":"tool_use"},{"id":"generated-run","input":{"path":"main/generator/tools/hello","arguments":{"word":"supplied"}},"name":"run_tool","type":"tool_use"},{"id":"generated-missing","input":{"path":"main/generator/tools/missing"},"name":"tool_help","type":"tool_use"},{"id":"toolu_00","input":{"path":"main/caos-tools/hello"},"name":"tool_help","type":"tool_use"},{"id":"toolu_00b","input":{"path":"main/caos-tools/undocumented"},"name":"tool_help","type":"tool_use"},{"id":"toolu_00c","input":{"path":"main/caos-tools/nope"},"name":"tool_help","type":"tool_use"},{"id":"toolu_01","input":{"cmd":"sed -i s/v1/v2/ main/caos-tools/hello/worker.sh","paths":["main/caos-tools/hello/worker.sh"]},"name":"bash","type":"tool_use"},{"id":"toolu_02","input":{"path":"main/caos-tools/hello"},"name":"run_tool","type":"tool_use"},{"id":"toolu_03","input":{"path":"main/caos-tools/boom"},"name":"run_tool","type":"tool_use"},{"id":"toolu_04","input":{"path":"main/caos-tools/hello","arguments":{"word":"banana","suffix":"-split"}},"name":"run_tool","type":"tool_use"}]'
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

stage "nothing enumerates the tools: the declared list is tree-independent"
jq -r .system /tmp/stub/request-1.json > /tmp/system-prompt
jq -e '[.tools[].name] | index("run_tool") != null and index("tool_help") != null
        and index("hello") == null and index("boom") == null' \
  /tmp/stub/request-1.json >/dev/null \
  || fail "repository tools must be reached through tool_help/run_tool, not declared"
# The per-source-tree schema block is gone. Its absence is the whole point: with
# it, a conversation that gained a source tree re-keyed the prompt and grew it.
if grep -qF 'Repository tools for source tree' /tmp/system-prompt; then
  fail "the system prompt still enumerates a source tree's tools"
fi
if grep -qF 'Say hello from the tree.' /tmp/stub/request-1.json; then
  fail "a repository tool's doc reached the model without being asked for"
fi
[ "$(grep -oF '"name":"bash"' /tmp/stub/request-1.json | wc -l)" = 1 ] \
  || fail "built-in bash missing (or duplicated by the same-named directory)"
if grep -qF 'impostor' /tmp/stub/request-1.json; then
  fail "the same-named directory's doc leaked into the declared tools"
fi
echo "  ok: tool_help + run_tool declared; no tool of the tree's own" >&2

stage "tool_help: doc and the @param contract after evaluation"
grep -qF 'Say hello from the tree.' /tmp/stub/request-2.json \
  || fail "tool_help did not carry the tool's description"
grep -qF 'word (required)' /tmp/stub/request-2.json \
  || fail "tool_help did not mark a bare @param name required"
grep -qF 'suffix (optional)' /tmp/stub/request-2.json \
  || fail "tool_help did not mark @param [name] optional"
grep -qF 'The word to echo.' /tmp/stub/request-2.json \
  || fail "tool_help dropped a @param's documentation"
echo "  ok: word required, suffix optional, docs carried" >&2

stage "tool_help: invalid definitions and missing paths with sibling hints"
grep -qF 'binds no `--help`' /tmp/stub/request-2.json \
  || fail "an expression with no --help was not distinguished from a non-tool"
grep -qF 'no such path' /tmp/stub/request-2.json \
  || fail "a path that does not exist was not reported as such"
# Discovery is documentation, so a wrong path is the ordinary mistake: the
# siblings turn it into a self-correcting one instead of a round trip.
grep -qF 'Directories in caos-tools:' /tmp/stub/request-2.json \
  || fail "a bad tool path did not name the sibling directories"
echo "  ok: no-help, no-such-path, and the sibling listing" >&2

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
grep -qF 'the `run_tool` tool failed to run' /tmp/stub/request-2.json \
  || fail "the sub-run failure was not reported back to the model"
# Five: the three bad `tool_help` paths, the missing required arg, and the dead
# sub-run. Every one is a value the model can read, not a dead turn.
[ "$(grep -oF '"is_error":true' /tmp/stub/request-2.json | wc -l)" = 5 ] \
  || fail "the tool_help, validation and sub-run failures were not all is_error"
# The good call is after both failures in the same queue. Its result proves
# that the queue continued, the bash edit survived the failed sub-run, and the
# declared args reached the script at /cas/args/<name>.
grep -qF 'hello-from-tree-v2 word=banana-split' /tmp/stub/request-2.json \
  || fail "the queued tool lost its args or the edited source_tree"
$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/caos-tools.records
bash_source_tree=$(source_tree_commit "$head")
assert_oid "$bash_source_tree" "bash-adopted source_tree"
jq -e --arg source_tree "$bash_source_tree" \
  'select(.id == "toolu_04") | .task != null and .input_commit == $source_tree' \
  /tmp/caos-tools.records >/dev/null \
  || fail "later hello call did not start from the bash edit"
final_source_tree=$(source_tree_commit "$head")
fetch_code "$final_source_tree" "fetching final source_tree"
case "$(git show "$final_source_tree:caos-tools/hello/worker.sh")" in
  *hello-from-tree-v2*) ;;
  *) fail "the failed sub-run lost the earlier source_tree edit" ;;
esac
echo "  ok: the dead sub-run came back as a value and the queued tool still ran" >&2

stage "generated tools use ancestor evaluation and preserve the original input"
grep -qF 'Generated tool.' /tmp/stub/request-2.json \
  || fail "help did not describe the evaluated generated tool"
grep -qF 'generated word=supplied input=original-source' /tmp/stub/request-2.json \
  || fail "generated invocation lost its arguments or original source input"
grep -qF 'main/generator/tools/missing' /tmp/stub/request-2.json \
  || fail "missing generated path did not produce a useful error"
pass caos-tools
