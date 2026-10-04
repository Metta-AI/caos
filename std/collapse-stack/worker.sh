#!/usr/bin/env bash
# The `collapse-stack` tool's worker. Its DOCS live in the sibling `.caos-expr`
# here-string, not in this header (SPEC, "CaosTools").
#
# Args, materialized under /cas/args:
#   stack     a tree whose entries include the layer gitlinks
#   onto      the commit the first layer is published onto
#   messages  a tree of message files, one per layer, named after its entry
#
# Returns a tree of gitlinks, `{<entry>: <collapsed commit>}`, which the agent
# links into the conversation as a whole.
#
# A refusal is a VALUE, a `report` tree with a FAILED banner, never a job
# error: the caller is an agent that has to read it and merge.
#
# The image is std/git-runner: bash, coreutils and git, and nothing else — no
# sed, grep or awk (CLAUDE.md).
set -euo pipefail

refuse() {
  local r=/tmp/collapse-refused
  rm -rf "$r"
  mkdir -p "$r"
  printf 'FAILED: %s\n' "$1" > "$r/report"
  caos put "$r" /cas/out
  exit 0
}

caos get /cas/args/stack
caos get /cas/args/messages
onto=$(caos hash /cas/args/onto)

# The layers ARE the message files, in byte order: a glob sorts by the
# collation, so pin it rather than let a locale reorder `01-x` and `01_x`.
export LC_ALL=C
names=()
tips=()
shopt -s nullglob
for file in /cas/args/messages/*; do
  name=${file##*/}
  if [ "$(caos kind "$file")" != blob ]; then
    refuse "messages/$name is not a file; each layer's message is one file named after its stack entry"
  fi
  caos get "$file"
  if [ ! -s "$file" ]; then
    refuse "messages/$name is empty; write layer $name's commit message into it"
  fi
  if [ ! -e "/cas/args/stack/$name" ]; then
    refuse "stack has no entry $name, which messages/$name names"
  fi
  if [ "$(caos kind "/cas/args/stack/$name")" != commit ]; then
    refuse "stack entry $name is not a source gitlink"
  fi
  names+=("$name")
  tips+=("$(caos hash "/cas/args/stack/$name")")
done
shopt -u nullglob
if [ "${#names[@]}" = 0 ]; then
  refuse "messages has no files; write one per layer to publish, named after its stack entry"
fi

# The commit GRAPH only (--filter=tree:0), as std/merge fetches it: ancestry
# and the tip commits' own headers are all this reads, and a layer's tree is
# reused by oid rather than fetched.
repo=/tmp/collapse-repo
rm -rf "$repo"
mkdir -p "$repo"
git -C "$repo" init -q
git -C "$repo" remote add origin "$CAOS_SERVER_URL"
git -C "$repo" config extensions.partialClone origin
git -C "$repo" config remote.origin.promisor true
git -C "$repo" config remote.origin.partialclonefilter tree:0
git -C "$repo" fetch -q --filter=tree:0 origin "$onto" "${tips[@]}"

# Every check before any mint, so a refusal leaves nothing half-made behind.
below="the base $onto"
below_tip=$onto
for i in "${!names[@]}"; do
  code=0
  git -C "$repo" merge-base --is-ancestor "$below_tip" "${tips[$i]}" || code=$?
  if [ "$code" = 1 ]; then
    refuse "layer ${names[$i]} (${tips[$i]}) does not contain $below ($below_tip). Merge $below into ${names[$i]} first."
  elif [ "$code" != 0 ]; then
    echo "collapse-stack: git merge-base failed (exit $code)" >&2
    exit 1
  fi
  below="layer ${names[$i]}"
  below_tip=${tips[$i]}
done

# Author and committer are copied verbatim from the layer tip, never taken
# from the clock: that is what makes a re-run mint the same commits, so a
# republished stack whose layers did not move pushes nothing new.
out=/tmp/collapse-out
rm -rf "$out"
mkdir -p "$out"
parent=$onto
for i in "${!names[@]}"; do
  raw=$(git -C "$repo" cat-file commit "${tips[$i]}")
  tree='' author='' committer=''
  while IFS= read -r header; do
    if [ -z "$header" ]; then
      break
    fi
    case $header in
      "tree "*) tree=${header#tree } ;;
      "author "*) author=$header ;;
      "committer "*) committer=$header ;;
    esac
  done <<< "$raw"
  message="/cas/args/messages/${names[$i]}"
  {
    printf 'tree %s\n' "$tree"
    printf 'parent %s\n' "$parent"
    printf '%s\n%s\n\n' "$author" "$committer"
    # The message file's bytes as written, so a body keeps its blank lines; a
    # missing final newline is the one thing supplied.
    cat "$message"
    if [ -n "$(tail -c 1 "$message")" ]; then
      printf '\n'
    fi
  } > /tmp/collapse-commit
  parent=$(caos put-commit /tmp/collapse-commit "/cas/c$i")
  ln -s "/cas/c$i" "$out/${names[$i]}"
done
caos put "$out" /cas/out
