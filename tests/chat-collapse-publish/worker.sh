#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
# tests/chat-collapse-publish — design/stacks.md's "Publishing", run the way an
# agent runs it, through llm-step's real tool loop with a scripted model:
#
#   turn 1  build a stack with the shell, write one message file per layer,
#           run_tool collapse-stack. The model must be SHOWN the result tree's
#           hash, because the next step needs it and nothing else carries it.
#   turn 2  the one-line link the doc gives, twice (the second time over the
#           link the first made, which is every republish, and which fails
#           unless the shell call declares the link in `paths`), then
#           publish_source on a CHILD of the link.
#
# What this pins is the doc's load-bearing claim: that after
# `ln -s /cas/s publish/mystack`, `publish/mystack/01-feature` is a source tree
# publish_source accepts. The repository is a `.invalid` host, so the push
# itself cannot happen; the call has to get PAST the source-tree check and fail
# reading the remote, and the same call on `publish/mystack` itself — a plain
# directory — has to be refused at that check, so the two are told apart.
#
# The stub answers request N with response-N.json, read when the request
# arrives, so turn 2's script is written after turn 1 has shown the hash.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

# What the model saw, kept as this test's narration: a pass prints it, so the
# recipe's actual output is readable without failing the test to see it.
note() { printf '%s\n' "$*" | tee -a /tmp/narration >&2; }
: > /tmp/narration

stage "a source tree and a stack-building turn"
llm_test_setup

mkdir -p /tmp/ws
echo "base" > /tmp/ws/base.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the source tree")
mkdir -p /tmp/stub
start_stub /tmp/stub
new_llm_conversation collapse-publish "$STUB_PORT" "$ws"

# The tools, staged as chat-tools-mixed stages its shell: a directory whose
# .caos-expr names the image by hash. An agent reaches the same images at
# caos-std/bash-tool and caos-std/collapse-stack.
sh_expr="curry --base:hash=$(caos hash /cas/args/bash-tool)"
collapse_expr="curry --base:hash=$(caos hash /cas/args/collapse)"

# Response bodies are JSON; jq builds them so the messages' newlines and quotes
# are escaped by something that knows how.
respond() { # <n> <json array of content blocks>
  jq -c --argjson c "$2" -n \
    '{content: $c, stop_reason: (if ($c | any(.type == "tool_use")) then "tool_use" else "end_turn" end)}' \
    > "/tmp/stub/response-$1.json" || fail "scripting response $1"
}
sh_call() { # <id> <cmd> [paths json]
  jq -c -n --arg id "$1" --arg cmd "$2" --argjson paths "${3:-[]}" \
    '{type: "tool_use", id: $id, name: "run_tool",
      input: {path: "tools/sh", arguments: ({cmd: $cmd} + (if $paths == [] then {} else {paths: $paths} end))}}'
}
write_call() { # <id> <file-path> <content>
  jq -c -n --arg id "$1" --arg p "$2" --arg c "$3" \
    '{type: "tool_use", id: $id, name: "write", input: {"file-path": $p, content: $c}}'
}
text() { jq -c -n --arg t "$1" '{type: "text", text: $t}'; }

M1="Add the feature ($SALT)

Why it exists,
over two lines.
"
M2="Test the feature ($SALT)

One body line.
"

respond 1 "[$(write_call tu_sh tools/sh/.caos-expr "$sh_expr"),
  $(write_call tu_cx tools/collapse/.caos-expr "$collapse_expr"),
  $(sh_call tu_l1 'mkdir -p mystack && cp -a main mystack/00-base && cp -a main mystack/01-feature && echo feature > mystack/01-feature/feature.txt' '["main"]')]"
respond 2 "[$(sh_call tu_l2 'cp -a mystack/01-feature mystack/02-tests && echo tests > mystack/02-tests/tests.txt' '["mystack"]'),
  $(write_call tu_m1 mystack-messages/01-feature "$M1"),
  $(write_call tu_m2 mystack-messages/02-tests "$M2")]"
respond 3 "[$(jq -c -n --arg onto "$base" \
  '{type: "tool_use", id: "tu_collapse", name: "run_tool",
    input: {path: "tools/collapse", arguments: {stack: "mystack", onto: $onto, messages: "mystack-messages"}}}')]"
respond 4 "[$(text "collapsed")]"

dispatch_turn "build a two-layer stack and collapse it"
wait_turn >/dev/null || fail "turn 1 never reached a terminal head"

# What the model saw for each call, as text, by tool_use id.
seen() { # <request n> <tool_use id>
  jq -r --arg id "$2" '.messages[-1].content[]
    | select(.type == "tool_result" and .tool_use_id == $id)
    | (if .is_error then "ERROR: " else "" end)
      + (.content | if type == "string" then . else map(.text // "") | join("") end)' \
    "/tmp/stub/request-$1.json"
}
for call in "2 tu_l1" "3 tu_l2" "3 tu_m1" "3 tu_m2"; do
  out=$(seen $call)
  case $out in ERROR:*) fail "turn 1 call ${call#* } failed: $out" ;; esac
done
collapsed=$(seen 4 tu_collapse)
note "collapse-stack, as the model saw it: $collapsed"
case $collapsed in
  "result tree "*": 01-feature 02-tests") ;;
  *) fail "collapse-stack's result did not render as its hash and two layers: $collapsed" ;;
esac
T=${collapsed#result tree }
T=${T%%:*}
assert_oid "$T" "the collapsed tree"

# The commits inside T, and what they must be: each layer's tree on the one
# below, with the message file's bytes.
caos get-hash "$T" /cas/t >/dev/null || fail "fetching the collapsed tree $T"
C1=$(caos hash /cas/t/01-feature)
C2=$(caos hash /cas/t/02-tests)
[ "$(caos kind /cas/t/01-feature)" = commit ] || fail "T/01-feature is not a gitlink"
L1=$(source_tree_commit "$head" mystack/01-feature)
L2=$(source_tree_commit "$head" mystack/02-tests)
fetch_code "$C2" "fetching the collapsed commits"
fetch_code "$L2" "fetching the layers"
[ "$(git rev-parse "$C1^{tree}")" = "$(git rev-parse "$L1^{tree}")" ] || fail "C1 is not layer 1's tree"
[ "$(git rev-parse "$C2^{tree}")" = "$(git rev-parse "$L2^{tree}")" ] || fail "C2 is not layer 2's tree"
[ "$(git rev-parse "$C1^@")" = "$base" ] || fail "C1's parents are not [onto]"
[ "$(git rev-parse "$C2^@")" = "$C1" ] || fail "C2's parents are not [C1]"
git cat-file commit "$C1" > /tmp/c1
body=$(cat /tmp/c1)
body=${body#*$'\n\n'}
[ "$body" = "${M1%$'\n'}" ] || fail "C1's message is not the file's, verbatim: [$body]"
note "ok: T = {01-feature: $C1, 02-tests: $C2}, each a layer tree on the one below"

stage "the link line, twice, then publish_source on a child of it"
link="caos get-hash $T /cas/s; rm -rf publish/mystack; mkdir -p publish; ln -s /cas/s publish/mystack"
REPO=https://caos-collapse-publish.invalid/repo.git
publish() { # <id> <source_tree>
  jq -c -n --arg id "$1" --arg s "$2" --arg r "$REPO" \
    '{type: "tool_use", id: $id, name: "publish_source",
      input: {source_tree: $s, repository: $r, branch: "feature", rewrite: true}}'
}
# `paths` names the link: on a republish `publish/mystack` already exists, and
# the shell may only replace what it has materialized. On the first publish
# the path does not exist yet, and declaring it is harmless.
respond 5 "[$(sh_call tu_link "$link" '["publish/mystack"]')]"
respond 6 "[$(sh_call tu_relink "$link" '["publish/mystack"]')]"
respond 7 "[$(publish tu_pub publish/mystack/01-feature), $(publish tu_dir publish/mystack)]"
respond 8 "[$(text "published")]"
dispatch_turn "publish the stack"
wait_turn >/dev/null || fail "turn 2 never reached a terminal head"

for call in "6 tu_link" "7 tu_relink"; do
  out=$(seen $call)
  note "${call#* }: ${out:-<no output>}"
  case $out in ERROR:*) fail "the link line failed (${call#* }): $out" ;; esac
done
[ "$(source_tree_commit "$head" publish/mystack/01-feature)" = "$C1" ] \
  || fail "publish/mystack/01-feature is not C1 in the conversation"
[ "$(source_tree_commit "$head" publish/mystack/02-tests)" = "$C2" ] \
  || fail "publish/mystack/02-tests is not C2 in the conversation"
note "ok: publish/mystack/{01-feature,02-tests} are source trees at C1, C2"

child=$(seen 8 tu_pub)
dir=$(seen 8 tu_dir)
note "publish_source publish/mystack/01-feature: $child"
note "publish_source publish/mystack:            $dir"
case $dir in
  *"requires an existing source gitlink"*) ;;
  *) fail "the control call on a plain directory was not refused as one: $dir" ;;
esac
case $child in
  *"requires an existing source gitlink"*) fail "a linked layer is not a source tree to publish_source: $child" ;;
  # read_branch's error: the call found the source gitlink and went on to read the remote.
  *"remote branch lookup failed"*) ;;
  *) fail "publish_source on the linked layer failed somewhere unexpected: $child" ;;
esac
note "ok: publish_source took the linked layer and stopped only at the remote"

stage "done"
printf 'chat-collapse-publish: ALL PASS\n' >> /tmp/narration
caos put /tmp/narration /cas/out
