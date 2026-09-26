#!/bin/bash
# Runs INSIDE a bash worker, as the `worker1` of the test's main run.
#
# Reports the RECORDED HASH of each `:@@=`-resolved arg — read off the
# placeholder, so nothing is fetched to learn it — which is the only thing the
# caller needs to check its claim: that a locator resolves to the foreign repo's
# own oid, and to the same oid a local `:@=` of the same bytes produces.
#
# It also resolves one FROM IN HERE. That used to be the assertion that a worker
# CANNOT: resolution was the client's, so `caos curry --x:@@=…` in a worker was
# refused on principle. The principle was always about the cache key — a locator
# must become an oid before the request is formed — and never about a sandbox;
# a worker has a network and uses it. What it lacks is a repository to fetch
# into, and now it does not need one: the SERVER resolves, and a worker reaches
# the server over the same HTTP it uses for everything else. The oid this
# produces has to be the one the client got, or "the URL is not in the key" is
# not true.
set -euo pipefail

out=/tmp/report
: > "$out"
for name in tree file whole local; do
  printf '%s %s\n' "$name" "$(caos hash "/cas/args/$name")" >> "$out"
done

# Not just resolved — DELIVERED. The oids came from a repo neither this worker
# nor the client has ever held, so reading the content here proves the server's
# fetch published into the object store every consumer reads from.
caos get -r /cas/args/tree
printf 'note %s\n' "$(cat /cas/args/tree/note.txt)" >> "$out"

caos get /cas/args/repo
caos get /cas/args/sha
repo=$(cat /cas/args/repo)
sha=$(cat /cas/args/sha)
worker_node=$(caos curry --base:@=/cas/args/base "--x:@@=$repo?rev=$sha&dir=payload") \
  || { echo "FAIL: a worker could not resolve a remote ref" >&2; exit 1; }
# Read the binding back off the curried node as a PLACEHOLDER — `resolve` records
# the hash without fetching the content, which is the same way the four args
# above are read.
caos resolve "$worker_node" args/x /cas/worker-x
printf 'worker-tree %s\n' "$(caos hash /cas/worker-x)" >> "$out"

caos put "$out" /cas/out
