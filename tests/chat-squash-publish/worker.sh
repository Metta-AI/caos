#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
# tests/chat-squash-publish — design/stacks.md's "Publishing", run through
# llm-step's real tool loop with a scripted model, in one turn: build a stack
# with the shell, write one message file per layer, run squash-layers into
# publish/mystack, reword one message and run it again (a republish over what
# the first run wrote), then call publish_source.
#
# It checks that squash-layers leaves publish/mystack/01-feature as a source
# tree that publish_source accepts. The repository is a `.invalid` host, so no
# push happens: the call must get past the source-tree check and fail reading
# the remote. The same call on publish/mystack, a plain folder, must be refused
# at that check.
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
new_llm_conversation squash-publish "$STUB_PORT" "$ws"

# The tools, staged as chat-tools-mixed stages its shell: a directory whose
# .caos-expr names the image by hash. An agent reaches the same images at
# caos-std/bash-tool and caos-std/squash-layers.
sh_expr="curry --base:hash=$(caos hash /cas/args/bash-tool)"
squash_expr="curry --base:hash=$(caos hash /cas/args/squash)"

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
  $(write_call tu_cx tools/squash/.caos-expr "$squash_expr"),
  $(sh_call tu_l1 'mkdir -p mystack && cp -a main mystack/00-base && cp -a main mystack/01-feature && echo feature > mystack/01-feature/feature.txt' '["main"]')]"
respond 2 "[$(sh_call tu_l2 'cp -a mystack/01-feature mystack/02-tests && echo tests > mystack/02-tests/tests.txt' '["mystack"]'),
  $(write_call tu_m1 mystack-messages/01-feature "$M1"),
  $(write_call tu_m2 mystack-messages/02-tests "$M2")]"
M2B="Test the feature, reworded ($SALT)

A republish: only this message changed.
"
squash_call() { # <id>
  jq -c -n --arg id "$1" --arg onto "$base" \
    '{type: "tool_use", id: $id, name: "run_tool",
      input: {path: "tools/squash", arguments: {stack: "mystack", onto: $onto, messages: "mystack-messages", into: "publish/mystack"}}}'
}
REPO=https://caos-squash-publish.invalid/repo.git
publish() { # <id> <source_tree>
  jq -c -n --arg id "$1" --arg s "$2" --arg r "$REPO" \
    '{type: "tool_use", id: $id, name: "publish_source",
      input: {source_tree: $s, repository: $r, branch: "feature", rewrite: true}}'
}
respond 3 "[$(squash_call tu_squash)]"
# The republish: reword one message and squash again over the publish/mystack
# the first call wrote. Nothing to link, nothing to declare.
respond 4 "[$(write_call tu_m2b mystack-messages/02-tests "$M2B"), $(squash_call tu_squash2)]"
respond 5 "[$(publish tu_pub publish/mystack/01-feature), $(publish tu_dir publish/mystack)]"
respond 6 "[$(text "published")]"

dispatch_turn "build a two-layer stack, squash it, republish it, publish it"
wait_turn >/dev/null || fail "the turn never reached a terminal head"

# What the model saw for each call, as text, by tool_use id.
seen() { # <request n> <tool_use id>
  jq -r --arg id "$2" '.messages[-1].content[]
    | select(.type == "tool_result" and .tool_use_id == $id)
    | (if .is_error then "ERROR: " else "" end)
      + (.content | if type == "string" then . else map(.text // "") | join("") end)' \
    "/tmp/stub/request-$1.json"
}
for call in "2 tu_l1" "3 tu_l2" "3 tu_m1" "3 tu_m2" "5 tu_m2b"; do
  out=$(seen $call)
  case $out in ERROR:*) fail "call ${call#* } failed: $out" ;; esac
done
# The commit a squash reported for an entry: its `  <into>/<entry>  <oid>` line.
reported() { # <out> <entry>
  local line
  while IFS= read -r line; do
    case $line in "  publish/mystack/$2  "*) printf '%s\n' "${line##* }"; return ;; esac
  done <<< "$1"
}
first=$(seen 4 tu_squash)
second=$(seen 5 tu_squash2)
note "squash-layers, as the model saw it:"
note "$first"
note "and on the republish:"
note "$second"
case $first$second in *ERROR:*|*FAILED*) fail "a squash was refused" ;; esac
C1=$(reported "$first" 01-feature)
C2=$(reported "$first" 02-tests)
C2B=$(reported "$second" 02-tests)
assert_oid "$C1" "the first squash's 01-feature"
assert_oid "$C2" "the first squash's 02-tests"
assert_oid "$C2B" "the republish's 02-tests"
[ "$(reported "$second" 01-feature)" = "$C1" ] \
  || fail "an unchanged layer squashed to a different commit on the republish"
[ "$C2B" != "$C2" ] || fail "a reworded message did not change its layer's commit"

# The conversation holds exactly the republished pair at publish/mystack, as
# gitlinks: the first run's tree replaced, not merged into.
listing=$(git ls-tree "$head:publish/mystack")
want="160000 commit $C1	01-feature
160000 commit $C2B	02-tests"
[ "$listing" = "$want" ] || fail "publish/mystack is not exactly {01-feature: C1, 02-tests: C2'}:
$listing"
[ "$(source_tree_commit "$head" publish/mystack/01-feature)" = "$C1" ] \
  || fail "publish/mystack/01-feature is not a source tree at C1"

# Each commit is its layer's tree on the one below, with the file's message.
L1=$(source_tree_commit "$head" mystack/01-feature)
L2=$(source_tree_commit "$head" mystack/02-tests)
fetch_code "$C2B" "fetching the squashed commits"
fetch_code "$L2" "fetching the layers"
[ "$(git rev-parse "$C1^{tree}")" = "$(git rev-parse "$L1^{tree}")" ] || fail "C1 is not layer 1's tree"
[ "$(git rev-parse "$C2B^{tree}")" = "$(git rev-parse "$L2^{tree}")" ] || fail "C2' is not layer 2's tree"
[ "$(git rev-parse "$C1^@")" = "$base" ] || fail "C1's parents are not [onto]"
[ "$(git rev-parse "$C2B^@")" = "$C1" ] || fail "C2's parents are not [C1]"
body=$(git cat-file commit "$C2B")
body=${body#*$'\n\n'}
[ "$body" = "${M2B%$'\n'}" ] || fail "C2's message is not the reworded file, verbatim: [$body]"
note "ok: publish/mystack = {01-feature: $C1, 02-tests: $C2B}, each a layer tree on the one below"

child=$(seen 6 tu_pub)
dir=$(seen 6 tu_dir)
note "publish_source publish/mystack/01-feature: $child"
note "publish_source publish/mystack:            $dir"
case $dir in
  *"requires an existing source gitlink"*) ;;
  *) fail "the control call on a plain directory was not refused as one: $dir" ;;
esac
case $child in
  *"requires an existing source gitlink"*) fail "a squashed layer is not a source tree to publish_source: $child" ;;
  # read_branch's error: the call found the source gitlink and went on to read the remote.
  *"remote branch lookup failed"*) ;;
  *) fail "publish_source on the squashed layer failed somewhere unexpected: $child" ;;
esac
note "ok: publish_source took the squashed layer and stopped only at the remote"

stage "done"
printf 'chat-squash-publish: ALL PASS\n' >> /tmp/narration
caos put /tmp/narration /cas/out
