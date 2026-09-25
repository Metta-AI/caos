#!/bin/bash
# Stateless scoped replay through the real agent handler and server object APIs
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail
caos get /cas/args/common
# shellcheck disable=SC1090
source /cas/args/common
llm_test_setup

stage "source history and a conflicting new base"
mkdir -p /tmp/source
printf 'original\n' > /tmp/source/file
at=$(publish_tree /tmp/source /cas/a-tree "base tree")
a=$(mint_commit /cas/a "$at" "original ($SALT)")
printf 'intermediate\n' > /tmp/source/file
xt=$(publish_tree /tmp/source /cas/x-tree "intermediate tree")
x=$(mint_commit /cas/x "$xt" "tool call" "$a")
printf 'feature\n' > /tmp/source/file
bt=$(publish_tree /tmp/source /cas/b-tree "working tree")
b=$(mint_commit /cas/b "$bt" "another tool call" "$x")
printf 'second layer\n' > /tmp/source/second
ct=$(publish_tree /tmp/source /cas/c-tree "upper layer tree")
c=$(mint_commit /cas/c "$ct" "upper tool call" "$b")
rm /tmp/source/second
printf 'upstream\n' > /tmp/source/file
ht=$(publish_tree /tmp/source /cas/h-tree "upstream tree")
h=$(mint_commit /cas/h "$ht" "new upstream" "$a")
fixture_ref="refs/heads/llm-rebase-fixture-$(date +%s%N)-$$"
fetch_code "$h" "fetching fixture tip"
git push -q caos "$h:$fixture_ref"
trap 'git push -q caos ":$fixture_ref" >/dev/null 2>&1 || true; llm_test_cleanup' EXIT

stage "public repository tool with the evaluated worker pinned"
stack_worker=$(caos hash /cas/args/stack-worker)
assert_oid "$stack_worker" "stack worker image"
base_binding='--base:@=DEEP-DEPS/git-stack'
caos get /cas/args/rebase-expr
expression=$(cat /cas/args/rebase-expr)
pinned=${expression/"$base_binding"/"--base:hash=$stack_worker"}
[ "$pinned" != "$expression" ] || fail "replay wrapper lacks the shared worker binding"
mkdir -p /tmp/repository-tools/git-rebase-i
printf '%s\n' "$pinned" > /tmp/repository-tools/git-rebase-i/.caos-expr
tools_tree=$(publish_tree /tmp/repository-tools /cas/repository-tools "repository tools")

committer='Replay <test@caos> 1700000001 +0000'
plan=$(printf 'onto=%s\ncommitter=%s\npick=%s..%s\nmessage=messages/first\nbranch=feature\npick=%s\nmessage=messages/second\nbranch=second\n' "$h" "$committer" "$a" "$b" "$c")
promotion_plan=$(printf 'onto=%s\ncommitter=%s\npick=%s..%s\nmessage=message\nbranch=promoted\nbranch=work\n' "$a" "$committer" "$a" "$b")
setup_cmd=$(cat <<EOF
mkdir -p feature/rebase promotion/rebase
caos get-hash $b /cas/test-layer-b
caos get-hash $c /cas/test-layer-c
caos get-hash $tools_tree /cas/test-tools
ln -s /cas/test-tools tools
ln -s /cas/test-layer-b feature/00-work
ln -s /cas/test-layer-c feature/01-work
ln -s /cas/test-layer-b promotion/00-work
printf '%s\n' $a > feature/00.base
printf '%s\n' $b > feature/01.base
printf '%s\n' $a > promotion/00.base
printf 'preserve notes\n' > feature/notes
printf 'preserve promotion notes\n' > promotion/notes
EOF
)
promotion_message=$'  promoted subject  \n\nkeep message whitespace\n\n'
mkdir -p /tmp/stub
jq -n --arg cmd "$setup_cmd" --arg plan "$plan" --arg promotion_plan "$promotion_plan" --arg message "$promotion_message" '{content:[
 {type:"tool_use",id:"setup",name:"bash",input:{cmd:$cmd,paths:[]}},
 {type:"tool_use",id:"plan",name:"write",input:{"file-path":"feature/rebase/plan",content:$plan}},
 {type:"tool_use",id:"promotion-plan",name:"write",input:{"file-path":"promotion/rebase/plan",content:$promotion_plan}},
 {type:"tool_use",id:"promotion-message",name:"write",input:{"file-path":"promotion/message",content:$message}},
 {type:"tool_use",id:"first-message",name:"write",input:{"file-path":"feature/messages/first",content:"first feature\n"}},
 {type:"tool_use",id:"second-message",name:"write",input:{"file-path":"feature/messages/second",content:"second feature\n"}}
],stop_reason:"tool_use"}' > /tmp/stub/response-1.json
cat > /tmp/stub/response-2.json <<'JSON'
{"content":[
 {"type":"tool_use","id":"help","name":"tool_help","input":{"path":"tools/git-rebase-i"}},
 {"type":"tool_use","id":"promote","name":"run_tool","input":{"path":"tools/git-rebase-i","scope":"promotion","arguments":{}}},
 {"type":"tool_use","id":"start","name":"run_tool","input":{"path":"tools/git-rebase-i","scope":"feature","arguments":{}}}
],"stop_reason":"tool_use"}
JSON
cat > /tmp/stub/response-3.json <<'JSON'
{"content":[
 {"type":"tool_use","id":"report","name":"read","input":{"file-path":"feature/rebase/conflicts"}},
 {"type":"tool_use","id":"draft","name":"read","input":{"file-path":"feature/rebase/work/file"}},
 {"type":"tool_use","id":"resolve","name":"write","input":{"file-path":"feature/rebase/work/file","content":"resolved\n"}}
],"stop_reason":"tool_use"}
JSON
# The resolved gitlink's commit R is only known after the edit. The agent changes
# the failed instruction to H..R; no saved progress or continue operation exists.
rewrite_cmd=$(cat <<EOF
caos put feature/rebase/work /cas/resolved-work
resolved=\$(caos hash /cas/resolved-work)
[[ "\$resolved" =~ ^[0-9a-f]{40}$ ]] || exit 1
printf 'onto=%s\ncommitter=%s\npick=%s..%s\nmessage=messages/first\nbranch=feature\npick=%s\nmessage=messages/second\nbranch=second\n' '$h' '$committer' '$h' "\$resolved" '$c' > feature/rebase/plan
EOF
)
jq -n --arg cmd "$rewrite_cmd" '{content:[
 {type:"tool_use",id:"rewrite-plan",name:"bash",input:{cmd:$cmd,paths:["feature/rebase/work","feature/rebase/plan"]}}
],stop_reason:"tool_use"}' > /tmp/stub/response-4.json
cat > /tmp/stub/response-5.json <<'JSON'
{"content":[{"type":"tool_use","id":"replay","name":"run_tool","input":{"path":"tools/git-rebase-i","scope":"feature","arguments":{}}}],"stop_reason":"tool_use"}
JSON
printf '%s\n' '{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}' > /tmp/stub/response-6.json
start_stub /tmp/stub
new_llm_conversation llm-rebase "$STUB_PORT" - "Use the repository replay tool to promote work and restack the supplied feature."
dispatch_turn "Promote the work and restack the feature. ($SALT)"
wait_turn 600

stage "a short replay plan promotes work and starts the next layer"
promoted=$(source_tree_commit "$head" promotion/00-promoted)
next_work=$(source_tree_commit "$head" promotion/01-work)
fetch_code "$promoted" "fetching promoted source history"
[ "$promoted" != "$b" ] || fail "promotion retained the raw work commit"
[ "$(git rev-parse "$promoted^{tree}")" = "$bt" ] || fail "promotion changed the work tree"
[ "$(git rev-parse "$promoted^")" = "$a" ] || fail "promotion did not squash onto its recorded base"
[ "$(git rev-list --count "$a..$promoted")" = 1 ] || fail "tool-call history remains in promoted layer"
[ "$next_work" = "$promoted" ] || fail "next work does not point to promoted commit"
[ "$(record "$head" promotion/00.base)" = "$a" ] || fail "promotion changed its recorded base"
[ "$(record "$head" promotion/01.base)" = "$promoted" ] || fail "next work has the wrong base"
[ "$(record "$head" promotion/notes)" = 'preserve promotion notes' ] || fail "promotion lost ordinary files"
if git cat-file -e "$head:promotion/00-work" 2>/dev/null; then fail "promoted work name remains"; fi
printf '%s' "$promotion_message" > /tmp/expected-message
git cat-file commit "$promoted" | sed '1,/^$/d' > /tmp/promoted-message
cmp -s /tmp/expected-message /tmp/promoted-message || fail "promotion changed the literal message bytes"

stage "replayed resolution retains source changes and intentional history"
first=$(source_tree_commit "$head" feature/00-feature)
second=$(source_tree_commit "$head" feature/01-second)
fetch_code "$second" "fetching finished source history"
[ "$(git rev-parse "$first^")" = "$h" ] || fail "first output has the wrong parent"
[ "$(git rev-parse "$second^")" = "$first" ] || fail "second output has the wrong parent"
[ "$(git rev-list --count "$h..$second")" = 2 ] || fail "draft tool-call commits leaked into output history"
[ "$(git show "$second:file")" = resolved ] || fail "resolution was lost while replaying the upper commit"
[ "$(git show "$second:second")" = 'second layer' ] || fail "upper commit was not applied"
[ "$(git show -s --format=%B "$first")" = 'first feature' ] || fail "wrong first message"
[ "$(git show -s --format=%B "$second")" = 'second feature' ] || fail "wrong second message"
[ "$(record "$head" feature/00.base)" = "$h" ] || fail "wrong first base"
[ "$(record "$head" feature/01.base)" = "$first" ] || fail "wrong second base"
[ "$(record "$head" feature/notes)" = 'preserve notes' ] || fail "ordinary feature files were lost"
if git cat-file -e "$head:feature/rebase" 2>/dev/null; then fail "completed replay directory remains"; fi
if git cat-file -e "$head:feature/00-work" 2>/dev/null; then fail "old layer name remains"; fi
grep -qF 'CONFLICT' /tmp/stub/request-4.json || fail "native conflict report was not readable"
grep -qF '<<<<<<<' /tmp/stub/request-4.json || fail "draft omitted Git conflict markers"
jq -e '.tools | any(.name == "run_tool") and all(.name != "git-rebase-i")' /tmp/stub/request-1.json >/dev/null || fail "replay must be a repository tool"
$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/rebase-tools.jsonl
jq -s -e 'all(.[]; .status == "complete")' /tmp/rebase-tools.jsonl >/dev/null || fail "a replay tool call failed"
for response in /tmp/stub/request-{2,3,4,5,6}.json; do
  jq -e '[.messages[].content | select(type == "array") | .[] | select(.type == "tool_result")] | all(.is_error != true)' "$response" >/dev/null || fail "agent received an error result"
done
assert_spine "$head"
pass llm-rebase
