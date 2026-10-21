#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail
caos get /cas/args/common
# shellcheck disable=SC1090
source /cas/args/common

stage "an import run by path, in an empty conversation"
llm_test_setup
# The turn helper fetches its configuration before publishing the conversation; git
# refuses that when every server ref is hidden, so advertise one private fixture ref.
fixture_ref="refs/heads/llm-import-path-fixture-$(date +%s%N)-$$"
fixture=$(git -c user.name=test -c user.email=test@example.invalid commit-tree "$(git mktree </dev/null)" -m fixture)
git push -q caos "$fixture:$fixture_ref"
trap 'git push -q caos ":$fixture_ref" >/dev/null 2>&1 || true; llm_test_cleanup' EXIT

# The entry is a tool like any other: a directory whose `.caos-expr` names an image
# (the built std/import-source, by hash). The model stages it with `write`.
entry=$(caos hash /cas/args/import-source)
mkdir -p /tmp/stub
jq -n --arg entry "$entry" '{content:[{type:"tool_use",id:"stage",name:"write",
  input:{"file-path":"tools/import-source/.caos-expr",content:("curry --base:hash=" + $entry + "\n")}}],
  stop_reason:"tool_use"}' > /tmp/stub/response-1.json
cat > /tmp/stub/response-2.json <<'JSON'
{"content":[{"type":"tool_use","id":"help","name":"tool_help","input":{"path":"tools/import-source"}},{"type":"tool_use","id":"import","name":"run_tool","input":{"path":"tools/import-source","arguments":{"source":"https://github.com/octocat/Hello-World.git","into":"imports/hello/base"}}}],"stop_reason":"tool_use"}
JSON
cat > /tmp/stub/response-3.json <<'JSON'
{"content":[{"type":"tool_use","id":"read","name":"read","input":{"file-path":"imports/hello/base/README"}},{"type":"tool_use","id":"occupied","name":"run_tool","input":{"path":"tools/import-source","arguments":{"source":"https://github.com/octocat/Hello-World.git","into":"imports/hello/base"}}},{"type":"tool_use","id":"stray","name":"run_tool","input":{"path":"tools/import-source","arguments":{"source":"https://github.com/octocat/Hello-World.git","into":"imports/other","branch":"main"}}}],"stop_reason":"tool_use"}
JSON
printf '%s\n' '{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}' > /tmp/stub/response-4.json
start_stub /tmp/stub
new_llm_conversation llm-import-path "$STUB_PORT" - "Import the requested code and report errors."
dispatch_turn "Import the public Hello-World repository by path. ($SALT)"
wait_turn 600

imported=$(source_tree_commit "$head" imports/hello/base)
fetch_code "$imported" "fetching imported history"
[ "$(git show "$imported:README")" = "Hello World!" ] || fail "wrong imported README"
[ "$(git rev-list --count "$imported")" -gt 1 ] || fail "missing ancestor history"
record "$head" imports/hello/base.source.json > /tmp/provenance
jq -e --arg commit "$imported" '.commit == $commit and .repository == "https://github.com/octocat/Hello-World.git"' /tmp/provenance >/dev/null \
  || fail "wrong provenance"

# `tool_help` describes the entry: its parameters are the model's whole documentation.
jq -r '.messages[].content | select(type == "array") | .[] | select(.type == "tool_result" and .tool_use_id == "help") | .content | if type == "array" then .[0].text else . end' /tmp/stub/request-3.json > /tmp/help.text
grep -q "into" /tmp/help.text || fail "tool_help did not describe the entry: $(cat /tmp/help.text)"
grep -q "Unused conversation path" /tmp/help.text || fail "tool_help lost the parameter docs: $(cat /tmp/help.text)"
grep -qF 'Hello World!' /tmp/stub/request-4.json || fail "the read could not see the imported blob"

# Mistakes are values the model can act on: a taken path, and an argument the operation
# does not declare, each a single error result.
for call in occupied stray; do
  jq -e --arg id "$call" '[.messages[].content | select(type == "array") | .[] | select(.type == "tool_result" and .tool_use_id == $id)] | length == 1 and .[0].is_error == true' /tmp/stub/request-4.json >/dev/null \
    || fail "$call did not yield one error result"
done
[ "$(source_tree_commit "$head" imports/hello/base)" = "$imported" ] || fail "the occupied import changed existing code"
assert_spine "$head"
pass llm-import-path
