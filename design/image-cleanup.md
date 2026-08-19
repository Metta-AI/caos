# Image cleanup

CAOS has two image caches with different jobs:

- Docker is the working set. Once no container uses a CAOS image, its local
  copy is disposable because Docker can pull it back from the local registry.
- The registry is the backing cache. It retains recently used content up to a
  size ceiling, plus correctness roots.

`caosd image-cleanup` applies that distinction. It is a dry run unless passed
`--execute`.

## Last used and the registry budget

Immediately before dispatch, the server retags a local manifest as
`caos-used-<manifest-sha256>`. The tag points at the same immutable manifest,
so it changes neither the image nor its digest. Replacing the tag updates
Distribution's tag-link mtime, which is the durable last-used clock. Each
server process writes at most once per manifest per hour.

The record lives in the registry because host and nested test servers share the
registry but not a filesystem or Redis instance. A manifest without a usage
tag is treated as used now and receives a tag on the first executing cleanup.

The default policy removes anything unused for seven days, then applies a
20 GiB ceiling to what remains. Non-root manifests are considered newest first.
Their manifest, config, and layer blobs are added to a set, so shared layers are
counted once; the least-recently-used closures that do not fit are removed.
`--unused-for=<days>d` and `--max-size=<gib>GiB` override the defaults.

## Roots and execution

The direct image digests under `refs/caos/seed` are permanent roots for the
current std generation. A seed hit already promises that its digest exists, so
deleting it would turn a valid result into a later Docker pull failure.
Republishing std moves the ref and makes the old generation ordinary LRU data.

`--execute` refuses while a `caos-worker-*` container is active. Otherwise it:

1. stops the host stack if it was running and removes idle test stacks;
2. tags retained legacy manifests and deletes selected manifests;
3. runs Distribution's offline garbage collector when a manifest was deleted;
4. clears Redis only after registry deletion, because cached results can carry
   deleted digests indirectly;
5. removes every unused local Docker copy of a CAOS registry image and obsolete
   `caos-stack-src:*` tags; and
6. restores the host stack if it was running.

Nix is a machine-wide store rather than a CAOS-owned cache. This command does
not run global Nix garbage collection.
