#!/bin/bash
# shellcheck disable=SC1091,SC2016,SC2034,SC2154
# tests/caos-tools — a WORKER test, in dev/worker-test (it needs git).
#
# Repository tools (SPEC "CaosTools"): a tool is any DIRECTORY whose
# `.caos-expr` binds the javadoc `help` (description as free text, `@param` tags
# as the parameters), addressed by PATH. `tool_help` describes one by READING
# that expression — no evaluation, so describing a compiled tool never builds
# it — and `run_tool` asks the server to EVALUATE it, then curries the model's
# args onto the ArgTree it yields and runs it over the tree.
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
# A DECLARED WRITER: `@writer` in its help, a `{prop, out, message}` result,
# and `prop` a TREE so the harness mints the commit — the shape almost every
# writer wants, where the tool never has to know what a commit is.
tool writer 'Add WRITER.md to the source tree.
@writer
@in' <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
# `/cas/args/in`, not `/cas/in`: a bound arg lands under /cas/args (SPEC,
# "CaosTools" -- Receiving args).
caos get -r /cas/args/in
rm -rf /tmp/w && mkdir -p /tmp/w
cp -RL /cas/args/in/. /tmp/w/
chmod -R u+w /tmp/w
printf 'written by the writer tool\n' > /tmp/w/WRITER.md
caos put /tmp/w /cas/prop
rm -rf /tmp/wres && mkdir -p /tmp/wres
printf 'added WRITER.md\n' > /tmp/wres/out
printf 'writer: add WRITER.md\n' > /tmp/wres/message
ln -s /cas/prop /tmp/wres/prop
caos put /tmp/wres /cas/out
EOF
# A writer that returns a COMMIT unrelated to the one it was given. `reconcile`
# ERRORS rather than conflicts on that, which would take the turn down, so the
# harness has to catch it first and say which two commits disagree.
tool orphan 'Return a commit that does not descend from its input.
@writer' <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
rm -rf /tmp/o && mkdir -p /tmp/o
printf 'unrelated\n' > /tmp/o/x
caos put /tmp/o /cas/orphantree
{ printf 'tree %s\n' "$(caos hash /cas/orphantree)"
  printf 'author caos <t@t> 0 +0000\n'
  printf 'committer caos <t@t> 0 +0000\n'
  printf '\nunrelated\n'
} > /tmp/ocommit
caos put-commit /tmp/ocommit /cas/orphancommit
rm -rf /tmp/ores && mkdir -p /tmp/ores
printf 'tried to publish an unrelated commit\n' > /tmp/ores/out
ln -s /cas/orphancommit /tmp/ores/prop
caos put /tmp/ores /cas/out
EOF
# A `{tree}` PARAMETER: the model names a tree and the script gets the tree
# itself at /cas/args/<name>, not bytes naming it. Before declared arg types
# every agent-supplied arg was a literal, so a tool like this worked when run
# by hand and silently got an empty blob when an agent called it.
#
# It declares no `@in`: the tree it reads is the ARGUMENT, so binding the tree
# it was run on as well would only bloat its key.
tool countdir 'Count the files in a tree.
@param {tree} src The tree to count.' <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
caos get -r /cas/args/src || { echo "src did not arrive as a tree" >&2; exit 1; }
n=$(find /cas/args/src -type f | wc -l)
printf 'files=%s\n' "$n" > /tmp/o
caos put /tmp/o /cas/out
EOF
# A directory that is NOT a tool: its expression binds no `--help`. `tool_help`
# has to say so distinctly from "no such path", because the two have different
# fixes.
mkdir -p /tmp/ws/caos-tools/undocumented
cp /tmp/ws/caos-tools/hello/worker.sh /tmp/ws/caos-tools/undocumented/worker.sh
printf 'curry --base:hash=%s --worker1:@=worker.sh\n' "$bash_img" \
  > /tmp/ws/caos-tools/undocumented/.caos-expr

ws=$(publish_tree /tmp/ws /cas/ws "publishing the tooled source_tree")

stage "script the stub LLM (describe; edit; bad call; dead sub-run; good; write)"
# All calls share one response and run in order. `tool_help` comes first,
# because that is the order a model works in with nothing listing the tools:
# describe the path, then run it. The missing arg must be answered in place, and
# the dead sub-run must preserve the bash-edited source tree, so the final valid
# hello call can still run the v2 script.
R1='[{"id":"toolu_00","input":{"path":"main/caos-tools/hello"},"name":"tool_help","type":"tool_use"},{"id":"toolu_00b","input":{"path":"main/caos-tools/undocumented"},"name":"tool_help","type":"tool_use"},{"id":"toolu_00c","input":{"path":"main/caos-tools/nope"},"name":"tool_help","type":"tool_use"},{"id":"toolu_01","input":{"cmd":"sed -i s/v1/v2/ main/caos-tools/hello/worker.sh","paths":["main/caos-tools/hello/worker.sh"]},"name":"bash","type":"tool_use"},{"id":"toolu_02","input":{"path":"main/caos-tools/hello"},"name":"run_tool","type":"tool_use"},{"id":"toolu_03","input":{"path":"main/caos-tools/boom"},"name":"run_tool","type":"tool_use"},{"id":"toolu_04","input":{"path":"main/caos-tools/hello","arguments":{"word":"banana","suffix":"-split"}},"name":"run_tool","type":"tool_use"},{"id":"toolu_05","input":{"path":"main/caos-tools/writer"},"name":"tool_help","type":"tool_use"},{"id":"toolu_06","input":{"path":"main/caos-tools/writer"},"name":"run_tool","type":"tool_use"},{"id":"toolu_07","input":{"path":"main/caos-tools/orphan"},"name":"run_tool","type":"tool_use"},{"id":"toolu_08","input":{"path":"main/caos-tools/hello","arguments":{"word":"banana","suffix":"-split"}},"name":"run_tool","type":"tool_use"},{"id":"toolu_09","input":{"path":"main/caos-tools/countdir","arguments":{"src":"main/caos-tools/hello"}},"name":"run_tool","type":"tool_use"},{"id":"toolu_10","input":{"path":"main/caos-tools/countdir","arguments":{"src":"main/nope"}},"name":"run_tool","type":"tool_use"}]'
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

stage "tool_help: doc and the @param contract, without evaluating anything"
grep -qF 'Say hello from the tree.' /tmp/stub/request-2.json \
  || fail "tool_help did not carry the tool's description"
grep -qF 'word (required)' /tmp/stub/request-2.json \
  || fail "tool_help did not mark a bare @param name required"
grep -qF 'suffix (optional)' /tmp/stub/request-2.json \
  || fail "tool_help did not mark @param [name] optional"
grep -qF 'The word to echo.' /tmp/stub/request-2.json \
  || fail "tool_help dropped a @param's documentation"
# Whether a tool changes the tree is STATED, not left to an absent line: these
# fixtures declare no `@writer`, so they must be described as read-only.
grep -qF 'Read-only' /tmp/stub/request-2.json \
  || fail "tool_help did not say whether the tool changes the tree"
echo "  ok: word required, suffix optional, read-only stated, docs carried" >&2

stage "tool_help: the two not-a-tool answers, each naming the siblings"
grep -qF 'binds no `--help`' /tmp/stub/request-2.json \
  || fail "an expression with no --help was not distinguished from a non-tool"
grep -qF 'no such path' /tmp/stub/request-2.json \
  || fail "a path that does not exist was not reported as such"
# Discovery is documentation, so a wrong path is the ordinary mistake: the
# siblings turn it into a self-correcting one instead of a round trip.
grep -qF 'Directories in main/caos-tools:' /tmp/stub/request-2.json \
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
# Six: the two bad `tool_help` paths, the missing required arg, the dead
# sub-run, the writer whose commit does not descend from its input, and the
# `{tree}` argument naming a path that is not there. Every
# one is a value the model can read, not a dead turn.
#
# On a mismatch this DUMPS EVERY tool_result, because the count alone says only
# that something changed -- not which call, and not why.
errors=$(grep -oF '"is_error":true' /tmp/stub/request-2.json | wc -l)
if [ "$errors" != 6 ]; then
  echo "--- $errors is_error blocks, expected 6. Every tool_result:" >&2
  jq -r '..|objects|select(.type=="tool_result")
         | "  \(.tool_use_id) is_error=\(.is_error // false): \((.content[0].text // "")[0:300])"' \
    /tmp/stub/request-2.json >&2 || true
  fail "the tool_help, validation, sub-run and ancestry failures were not all is_error"
fi
# The good call is after both failures in the same queue. Its result proves
# that the queue continued, the bash edit survived the failed sub-run, and the
# declared args reached the script at /cas/args/<name>.
grep -qF 'hello-from-tree-v2 word=banana-split' /tmp/stub/request-2.json \
  || fail "the queued tool lost its args or the edited source_tree"
$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/caos-tools.records
# toolu_04 (the good `hello`) and toolu_06 (the writer) must have run on THE
# SAME source commit: hello is read-only, so nothing moved the pointer between
# them. That is the queue continuing on one tree, which is what this ever meant.
#
# NOT "the source tree at the end of the turn", which it used to compare
# against: those matched only while toolu_04 was the last call to move the
# tree, and a writer after it makes the final tree a commit that did not exist
# when toolu_04 ran. And NOT bash's own resolution `output` either -- bash is
# UNSCOPED (it may edit conversation files and several trees at once), so
# `plan_file_changes` records its gitlink move under `files` and leaves
# `source_tree_resolution` null. Only a source-tree-SCOPED call carries an
# `output`.
hello_input=$(jq -r 'select(.id == "toolu_04") | .input_commit' /tmp/caos-tools.records)
writer_input=$(jq -r 'select(.id == "toolu_06") | .input_commit' /tmp/caos-tools.records)
assert_oid "$hello_input" "the source commit the queued hello call ran on"
[ "$hello_input" = "$writer_input" ] \
  || fail "queued calls did not share one source commit: $hello_input vs $writer_input"
jq -e 'select(.id == "toolu_04") | .task != null' /tmp/caos-tools.records >/dev/null \
  || fail "the queued hello call was never dispatched"
final_source_tree=$(source_tree_commit "$head")
fetch_code "$final_source_tree" "fetching final source_tree"
case "$(git show "$final_source_tree:caos-tools/hello/worker.sh")" in
  *hello-from-tree-v2*) ;;
  *) fail "the failed sub-run lost the earlier source_tree edit" ;;
esac
echo "  ok: the dead sub-run came back as a value and the queued tool still ran" >&2

stage "a DECLARED writer changes the source tree; a reader cannot"
# `@writer` is the whole difference. `hello` ran twice above and returned a
# blob; the source tree moved only for the tool that declared itself a writer.
grep -qF 'Writes: proposes a change' /tmp/stub/request-2.json \
  || fail "tool_help did not describe the writer as one"
grep -qF 'added WRITER.md' /tmp/stub/request-2.json \
  || fail "the writer's out entry did not reach the model"
case "$(git show "$final_source_tree:WRITER.md" 2>&1)" in
  *"written by the writer tool"*) ;;
  *) fail "the writer's proposal was not applied to the source tree" ;;
esac
# It built on the bash edit rather than replacing the tree it was handed.
case "$(git show "$final_source_tree:caos-tools/hello/worker.sh")" in
  *hello-from-tree-v2*) ;;
  *) fail "the writer's proposal discarded the earlier edit" ;;
esac
# The tool's own `message` became the commit message. Without it the mint falls
# back to the tool path, which is why source history was a column of identical
# one-word messages.
[ "$(git log -1 --format=%s "$final_source_tree")" = "writer: add WRITER.md" ] \
  || fail "the writer's message did not become the commit message: $(git log -1 --format=%s "$final_source_tree")"
echo "  ok: WRITER.md applied over the edit, with the tool's commit message" >&2

stage "a tool that declares no @in does not key on the tree"
# toolu_04 and toolu_08 are the SAME hello call with the SAME arguments, and
# the writer moved the source tree between them. hello declares no `@in`, so
# the tree is not in its ArgTree and both calls form one identical task --
# which is the whole point: `caos-test-result`, whose input is a hash, used to
# carry the entire source tree and so could never hit the memo twice.
first=$(jq -r 'select(.id == "toolu_04") | .task' /tmp/caos-tools.records)
again=$(jq -r 'select(.id == "toolu_08") | .task' /tmp/caos-tools.records)
assert_oid "$first" "the first hello task"
[ "$first" = "$again" ] \
  || fail "hello re-keyed across a tree change it never reads: $first vs $again"
# And their input commits DID differ, so the tree really did move underneath.
[ "$(jq -r 'select(.id == "toolu_04") | .input_commit' /tmp/caos-tools.records)" \
  != "$(jq -r 'select(.id == "toolu_08") | .input_commit' /tmp/caos-tools.records)" ] \
  || fail "the writer did not move the tree, so this proves nothing"
# The writer, which DOES declare `@in`, keys on the tree by construction: its
# task carries one and hello's does not.
echo "  ok: one task for both calls, across a real tree change" >&2

stage "a {tree} argument arrives as a tree, named by a conversation path"
# hello's directory holds exactly worker.sh and .caos-expr, and a `.caos-expr`
# is NOT stripped here: nothing is being evaluated, the tree is just read.
grep -qF 'files=2' /tmp/stub/request-2.json \
  || fail "the {tree} argument did not arrive as a readable tree"
# A path that resolves to nothing is the model's mistake, named as such.
grep -qF 'no such path: main/nope' /tmp/stub/request-2.json \
  || fail "a bad {tree} path was not reported to the model"
echo "  ok: a path became a tree; a bad path became a value" >&2

stage "a writer's commit must descend from the one it was given"
grep -qF 'does not descend from the commit it was given' /tmp/stub/request-2.json \
  || fail "an unrelated proposal commit was not reported to the model"
# And it did not land: reconcile would have ERRORED on it, taking the turn.
if git show "$final_source_tree:x" >/dev/null 2>&1; then
  fail "the unrelated commit was applied to the source tree"
fi
echo "  ok: the orphan commit was refused as a value, naming both commits" >&2

pass caos-tools
