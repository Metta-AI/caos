#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail
caos get /cas/args/common
# shellcheck disable=SC1090
source /cas/args/common

stage "public inline import in an empty conversation"
llm_test_setup
# The unchanged turn helper fetches its configuration before publishing the
# conversation. Git 2.54 refuses that fetch when every server ref is hidden.
# Advertise one private fixture ref; the conversation itself still has no code.
fixture_ref="refs/heads/llm-import-fixture-$(date +%s%N)-$$"
fixture=$(git -c user.name=test -c user.email=test@example.invalid commit-tree "$(git mktree </dev/null)" -m fixture)
git push -q caos "$fixture:$fixture_ref"
trap 'git push -q caos ":$fixture_ref" >/dev/null 2>&1 || true; llm_test_cleanup' EXIT
mkdir -p /tmp/stub
cat > /tmp/stub/response-1.json <<'JSON'
{"content":[{"type":"tool_use","id":"import","name":"import_source","input":{"source":"https://github.com/octocat/Hello-World.git","into":"imports/hello/base"}}],"stop_reason":"tool_use"}
JSON
cat > /tmp/stub/response-2.json <<'JSON'
{"content":[{"type":"tool_use","id":"read","name":"read","input":{"file-path":"imports/hello/base/README"}}],"stop_reason":"tool_use"}
JSON
cat > /tmp/stub/response-3.json <<'JSON'
{"content":[{"type":"tool_use","id":"occupied","name":"import_source","input":{"source":"https://github.com/octocat/Hello-World.git","into":"imports/hello/base"}},{"type":"tool_use","id":"local","name":"import_source","input":{"source":"/tmp/repo","into":"imports/local"}},{"type":"tool_use","id":"missing","name":"import_source","input":{"source":"https://github.com/octocat/Hello-World.git","revision":"refs/heads/caos-nonexistent-import-fixture","into":"imports/missing"}}],"stop_reason":"tool_use"}
JSON
printf '%s\n' '{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}' > /tmp/stub/response-4.json
start_stub /tmp/stub
new_llm_conversation llm-import "$STUB_PORT" - "Import the requested code and report errors."
dispatch_turn "Import the public Hello-World repository. ($SALT)"
wait_turn 600
imported=$(source_tree_commit "$head" imports/hello/base)
fetch_code "$imported" "fetching imported history"
[ "$(git show "$imported:README")" = "Hello World!" ] || fail "wrong imported README"
[ "$(git rev-list --count "$imported")" -gt 1 ] || fail "missing ancestor history"
record "$head" imports/hello/base.source.json > /tmp/provenance
jq -e --arg commit "$imported" '.commit == $commit and .repository == "https://github.com/octocat/Hello-World.git" and (.default_branch | length > 0)' /tmp/provenance >/dev/null || fail "wrong provenance"
jq -e '.tools | any(.name == "import_source")' /tmp/stub/request-1.json >/dev/null || fail "empty conversation did not offer import_source"
grep -qF 'Hello World!' /tmp/stub/request-3.json || fail "inline read could not see imported blob"
for call in occupied local missing; do
  jq -e --arg id "$call" '[.messages[].content | select(type == "array") | .[] | select(.type == "tool_result" and .tool_use_id == $id)] | length == 1 and .[0].is_error == true' /tmp/stub/request-4.json >/dev/null || fail "$call did not yield one error result"
done
[ "$(source_tree_commit "$head" imports/hello/base)" = "$imported" ] || fail "occupied import changed existing code"
assert_spine "$head"
pass llm-import
