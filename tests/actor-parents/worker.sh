#!/bin/bash
# SPIKE (design/actors.md, open question 6): can finish push a commit whose parent
# the scratch repo does not hold, so it need not fetch the whole commit chain?
# Pure git against the server, no actor code. Always fails at the end so the
# report shows in the suite output.
set -uo pipefail

: "${CAOS_SERVER_URL:?needs CAOS_SERVER_URL}"
id="$(date +%s%N)-$$-$RANDOM"
ref="refs/heads/actors/spike-$id"
aux="refs/heads/actors-spike/aux-$id"
N=30
report=/tmp/report
: > "$report"
say() { echo "$*" | tee -a "$report" >&2; }

# Seed: a chain of N commits on $ref, and an aux commit whose state/ tree stands
# for "new state that exists only on the server".
rm -rf /tmp/seed
git init -q /tmp/seed
cd /tmp/seed
git config user.email s@caos
git config user.name s
git remote add caos "$CAOS_SERVER_URL"
prev=""
for i in $(seq 1 $N); do
  blob=$(printf '%s\n' "$i" | git hash-object -w --stdin)
  sub=$(printf '100644 blob %s\tf%s\n' "$blob" "$i" | git mktree)
  root=$(printf '040000 tree %s\tstate\n' "$sub" | git mktree)
  if [ -n "$prev" ]; then prev=$(git commit-tree "$root" -p "$prev" -m "c$i"); else prev=$(git commit-tree "$root" -m "c$i"); fi
done
head=$prev
blob=$(printf 'new\n' | git hash-object -w --stdin)
sub=$(printf '100644 blob %s\tnew\n' "$blob" | git mktree)
newroot=$(printf '040000 tree %s\tstate\n' "$sub" | git mktree)
auxc=$(git commit-tree "$newroot" -m aux)
git push -q caos "$head:$ref" "$auxc:$aux" || { say "seed push failed"; cat "$report"; exit 1; }
newstate=$(git rev-parse "$newroot:state")
say "seeded: chain of $N, head=$head, new state tree=$newstate"

# variant <name> <parent handling>: a fresh promisor scratch repo, the same steps
# finish takes, parent handled as described. Prints OK/FAIL and local commit count.
variant() {
  local name=$1 dir=/tmp/v-$1
  rm -rf "$dir"
  git init -q --bare "$dir"
  cd "$dir"
  git config user.email v@caos
  git config user.name v
  git remote add origin "$CAOS_SERVER_URL"
  git config core.repositoryformatversion 1
  git config extensions.partialClone origin
  git config remote.origin.promisor true
  git config remote.origin.partialclonefilter tree:0
  git fetch -q --no-tags --no-write-fetch-head --filter=tree:0 origin "$newstate" 2>/dev/null \
    || { say "[$name] fetching the new state tree failed"; return; }
  case $name in
  full)    git fetch -q --no-tags --no-write-fetch-head --filter=tree:0 origin "$head" ;;
  noshallowfile)
    git fetch -q --no-tags --no-write-fetch-head --depth=1 --filter=tree:0 origin "$head"
    rm -f shallow ;;
  rawparent) : ;; # parent never fetched at all
  esac
  local tree commit
  tree=$(printf '040000 tree %s\tstate\n' "$newstate" | git mktree --missing 2>&1) \
    || { say "[$name] mktree: $tree"; return; }
  commit=$(printf 'tree %s\nparent %s\nauthor a <a@a> 0 +0000\ncommitter a <a@a> 0 +0000\n\nactor state\n' \
    "$tree" "$head" | git hash-object -t commit -w --stdin --literally 2>&1) \
    || { say "[$name] hash-object: $commit"; return; }
  local out
  if out=$(git push --force-with-lease="$ref:$head" origin "$commit:$ref" 2>&1); then
    say "[$name] OK  commits held locally: $(git rev-list --count --all --missing=allow-any 2>/dev/null || echo '?')"
    # restore the ref so the next variant starts from the same head
    git push -q --force origin "$head:$ref" 2>/dev/null
  else
    say "[$name] FAIL: $(echo "$out" | tr '\n' ' ' | cut -c1-300)"
  fi
}

variant full
variant noshallowfile
variant rawparent
echo "---- spike report ----" >&2
cat "$report" >&2
exit 1
