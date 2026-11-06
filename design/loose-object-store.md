# caos: a loose-object store on btrfs

Status: decided, not yet built. Measured on a copy of the caosd-prod object
store on 2026-11-05. Replaces the pack-maintenance part of
`design/caos-store-and-gc.md`; retention (the collector) is still open there.

## Summary

Stop packing. Every object in the server repo becomes a git loose object,
written uncompressed (zlib level 0), on a dedicated btrfs volume that
compresses with zstd. Nothing ever rewrites the store, so there is no repack,
no downtime consolidation, no pack-count ceiling and no reader that can see a
pack move under it. Git and gix keep reading the directory as an ordinary
object database, so every tool and the git protocol keep working.

It is mostly configuration, with three pieces of real work:

1. **Ordered publication.** Every path that writes packs today does so on
   purpose: a pack becomes visible all at once, when its index appears. Loose
   objects appear one file at a time, and git publishes them in directory
   order, not dependency order, so a crash or a concurrent reader can see a
   tree before its children. Each ingress path needs one shared routine that
   publishes children before parents (section 4.1).
2. **Imports and locators** write packs from server code
   (`import.rs`, `locator.rs`), not through git config. They switch to that
   routine.
3. **Migration**: a short downtime that explodes the existing ~1M objects onto
   the new volume (2 min 16 s measured) and swaps it in (section 6).

The cost is about 2.7x the disk of one pack (6.8 GiB against 2.5 GiB today)
and more CPU to serve large fetches. Both were judged acceptable.

## 1. Why

The store only grows packs, and every read pays for every pack.

- Every push leaves a pack (`receive.unpackLimit 1`), every import hard-links
  one, every locator indexes one. caosd-prod went from 2,864 packs at 22:00 to
  3,180 at 23:20 UTC on 2026-11-05, about 4 a minute under load.
- gix probes each pack's index until one hits. On dev, 8,583 packs took the
  test suite from 2 min 28 s to 68 min 39 s (PR #351).
- gix fixes its pack slot table at open: the server reserves the maximum,
  32,767, and refuses to start at that count (`main.rs`).
- Consolidating is only safe with the stack down (a live repack crashes gix
  readers and truncates fetches), so it runs at `caosd up`
  (`repack --geometric=2 -d` under `CAOS_STORE_CHECK`). Between restarts the
  count climbs.

Loose objects have none of this: a read is one path lookup, a write is one new
file, and no operation ever moves an existing object.

## 2. Measurements

All on a copy of the prod store (993,381 objects, 14.99 GB uncompressed),
rebuilt on a loopback btrfs on the prod host. Caveats: the loop image sat on
ext4 on EBS, reads were capped at 1,500 IOPS, and cold wall times are
dominated by that cap. Compare CPU across columns, not wall time.

### 2.1 What is in the store

| type | n | total | p50 | p90 | p99 | max | <= 2 KiB |
| --- | --- | --- | --- | --- | --- | --- | --- |
| all | 991,823 | 14.3 GiB | 294 B | 8.8 KB | 279 KB | 124 MB | 79.8% |
| blob | 290,757 | 12.3 GiB | 1,977 B | 43 KB | 546 KB | 124 MB | 50.5% |
| tree | 623,181 | 1.8 GiB | 160 B | 1.2 KB | 52 KB | 489 KB | 92.7% |
| commit | 77,885 | 158 MiB | 561 B | 2.8 KB | 28 KB | 2.1 MB | 86.2% |

Only 269-296 MB of the ~2.5 GiB (packed) is reachable from the 14,773 refs:
about 90% of the store is addressed by hash alone, which is why git's own gc
must never run here.

### 2.2 Disk

| layout | on disk |
| --- | --- |
| live repo today | 2.24 GiB (2,986 packs, 2.03 GiB, plus 49,408 loose, 214 MiB) |
| one fresh pack (`--window=10 --depth=50`) | 2.51 GiB |
| loose, level 0, btrfs `compress-force=zstd:3` | 6.79 GiB: data 4.97 GiB, metadata 934 MiB stored twice (DUP) |

- zstd alone compresses 14.99 GB to 5.2 GB (compsize); the rest of the gap to
  a pack is delta compression.
- 837,640 objects (84%) are stored inline in btrfs metadata, with no data
  block. Raising `max_inline` to 4095 moved 13k more inline and saved nothing.
- Single-copy metadata (`-m single`) would save ~0.9 GiB, at the cost of the
  second copy that guards metadata against corruption on one disk.
- Converting all objects took 2:16 wall, 105 s user + 36 s sys, 2.2 GB RSS.
  Building the one pack took 8:51-9:16 and ~900 s CPU, 3.5 GB RSS.

### 2.3 Reads (10,000 random objects, `cat-file --batch`)

| | loose cold | loose warm | one pack cold | one pack warm |
| --- | --- | --- | --- | --- |
| wall | 7.37 s | 0.12 s | 13.72 s | 0.43 s |
| user+sys | 0.72 s | 0.11 s | 0.87 s | 0.41 s |

Warm reads are 3.6x faster loose: no delta chains to resolve.

### 2.4 Writes (20,000 objects, `unpack-objects`)

| no fsync | fsync per object | `core.fsyncMethod=batch` |
| --- | --- | --- |
| 4.2 s | 2:48.9 (8.2 ms/object) | 9.9 s (0.29 ms/object) |

The per-object figure includes the loop device and the ext4 file beneath it;
native btrfs on EBS should be lower.

### 2.5 Serving fetches (`pack-objects`, as upload-pack runs it)

user+sys CPU / bytes out. L10 is git's default delta window; L0 is
`pack.window=0`; L0C1 adds `pack.compression=1`. "Live" is today's repo, warm.

| fetch | objects | loose L10 | loose L0 | loose L0C1 | one pack | live |
| --- | --- | --- | --- | --- | --- | --- |
| all refs | - | 192 s / 269 MB | 32.7 s / 561 MB | 16.3 s / 745 MB | 12 s / 296 MB | 8.3 s / 242 MB |
| conversation head | 794 | 0.19 s / 307 KB | 0.17 s / 682 KB | 0.12 s / 1.07 MB | 0.41 s / 549 KB | 0.15 s / 315 KB |
| imported source commit | 13,283 | 3.42 s / 31.8 MB | 4.04 s / 70.9 MB | 2.48 s / 92.5 MB | 0.85 s / 60.4 MB | 0.33 s / 35.0 MB |
| incremental, 10 commits | 30 | 0.03 s / 31.5 KB | 0.03 s / 43.8 KB | 0.03 s / 80.7 KB | 0.18 s / 31.4 KB | 0.07 s / 31.5 KB |

Small and incremental fetches cost what they cost today. Large fetches cost
several times more CPU, because the loose store compresses every object again
for every fetch, while the live packs hand out the deltas clients already
computed when they pushed. Accepted: large fetches are rare and not on any
latency-sensitive path. End-to-end fetches by hash through `upload-pack`
returned the right commits.

## 3. The store

- **Format**: git loose objects under `objects/xx/yyyy...`, zlib level 0
  (deflate stored blocks: the raw `type size\0body` plus ~11 bytes of
  framing). Every git and gix reader handles it. The object id hashes the
  uncompressed bytes, so the level never changes an id, and objects written
  at other levels can sit beside them.
- **Never packed.** `objects/pack` stays empty. Nothing deletes or rewrites an
  object except a future collector, by unlink.
- **Volume**: all of `/data` becomes btrfs, so every piece of state (the
  repo, the registry, redis, docker's data root) stays in one place and no
  path in the stack changes. `prod/caosd/configuration.nix` already finds the
  data disk by label; what changes is the filesystem and these:
  - `mkfs.btrfs` with default profiles (DUP metadata).
  - Mount options `noatime,compress=zstd:3`. The measurements used
    `compress-force`; with the whole disk on one filesystem, plain `compress`
    spares the CPU on registry blobs and image layers that are already
    gzipped. It gives up on a file whose first chunk does not compress, which
    never happens to a level-0 object; the migration rehearsal confirms the
    store still comes out near 6.8 GiB. Default `max_inline`.
  - Docker's storage driver pinned to `overlay2` (`daemon.settings`), so
    dockerd does not switch to its btrfs driver on a btrfs data root.
  - The redis directory (`/data/caos/stack/redis`) marked no-copy-on-write
    (`chattr +C` while empty): its append-only file is rewritten and synced
    constantly, which fragments under copy-on-write. That also turns off
    compression and checksums there, which is fine for a cache.
  - `autoResize` grows only ext filesystems: a small unit runs
    `btrfs filesystem resize max /data` at boot instead.
  - Size and IOPS: the disk is 1 TB with 202 GB used today; provision IOPS
    for cold reads, which cost one I/O per object.
- **Dev stacks** keep whatever filesystem they have. Loose objects work on
  ext4 too, with more waste per small object; at dev's size that does not
  matter. btrfs is a prod setting.

Server config, set unconditionally at start (`main.rs`) like the rest:

| setting | value | why |
| --- | --- | --- |
| `core.looseCompression` | `0` | btrfs compresses; level 0 makes inflate a copy |
| `receive.unpackLimit` | `2147483647` (replaces `1`) | pushes arrive as loose objects in quarantine |
| `transfer.unpackLimit`, `fetch.unpackLimit` | `2147483647` | anything fetched into this repo lands loose |
| `gc.auto 0`, `receive.autogc false`, `maintenance.geometric-repack.enabled false` | unchanged | still nothing may maintain this repo |
| `core.fsync objects,reference`, `core.fsyncMethod batch` | unchanged | |

## 4. What is more than configuration

### 4.1 Ordered publication

The invariant (SPEC): an object is stored only after everything it references
is stored. `POST /object` enforces it per object (`require_object`), and the
startup closure check refuses a store that breaks it. Packs keep it for free:
a pack becomes visible when its index is published, after the pack is synced
(`import.rs`: "publishing loose objects one at a time could expose a child
before a parent").

Loose publication breaks it in two ways if done naively:

- **Concurrent readers.** git migrates a push's quarantine into the store in
  directory order. A tree can be visible before its subtrees. A `POST /object`
  naming that tree passes `require_object` (the tree exists) and the new object
  rests on an incomplete closure.
- **Crash.** A crash mid-migration leaves the visible prefix behind. The next
  start's closure check fails, and the server will not start until someone
  intervenes.

The fix is one routine, `publish_loose(staging_objects_dir)`, used by every
ingress path:

1. The objects are already written and verified in a staging directory on the
   same filesystem (loose, level 0).
2. `syncfs` once, so every staged byte is durable.
3. Rename each object into `objects/` in children-first order: a depth-first
   walk that renames an object only after every object it references that is
   also in staging. Objects already in the store are skipped.
4. `syncfs` once more, then return.

A reader can then never see a parent without its children. After a crash the
store holds a closure-complete subset, provided renames reach disk in the
order they were issued; btrfs commits metadata in ordered transactions, which
should give exactly that. This must be verified with a power-cut test (a VM or
`dm-flakey`) before prod, and the startup closure check stays as the audit.

The ingress paths:

| path | today | after |
| --- | --- | --- |
| `git push` (receive-pack) | pack, `receive.unpackLimit 1` | `unpack-objects --strict` writes loose objects into git's quarantine (`objects/incoming-*`, same filesystem). A managed `pre-receive` hook runs `publish_loose` over `$GIT_QUARANTINE_PATH`; git then finds nothing left to migrate and updates the refs. A push whose ref update then fails leaves closure-complete objects behind, which is harmless. The hook is a fixed script that execs `$CAOS_SERVER_EXE publish-quarantine`; `git.rs` sets that variable on every `http-backend` it spawns, so servers of different builds sharing one repo never fight over the hook, and a push that does not come through a server is declined. Every push in the tree goes through the server over HTTP. |
| `POST /git/import` | verified pack hard-linked into `objects/pack` | verify in the staging repo as now, explode the pack into the staging repo's loose objects, `publish_loose` |
| locator | `index-pack --strict` of the tree closure straight into the live store | index into the staging repo (alternates to the live store), explode, `publish_loose` |
| `POST /object`, `store_git_blob`, `store_git_tree` | gix loose write + `sync_loose_object` | one object at a time, closure-checked as before, through the server's own level-0 writer (`loose::write_durable`): temporary file, fsync, rename, fsync the fan-out directory. gix's writer has no compression setting and never fsyncs, which is what `sync_loose_object` patched after the fact; the new writer cannot leave a zero-length object behind. |
| seed (`stack/serve` copies `CAOS_STACK_SEED_GIT`) | copies packs | unchanged: a seeded test store may keep its packs, which readers handle; the server warns at start whenever `objects/pack` holds anything |

The hook reintroduces a managed `pre-receive` hook, which the server removed
before for a different reason (an obsolete validator mode). Alternatives, if
the hook is unwanted: the server's `git.rs` already fronts every
`/git-receive-pack` request and parses its commands, so it could run
receive-pack against a staging repo and apply the ref updates itself. The hook
is less code.

### 4.2 Durability cost

`loose::write_durable` fsyncs each object and its directory: 8.2 ms an
object in the test (pessimistic, through a loop device), against 0.29 ms in
batch mode. That is the same two fsyncs `sync_loose_object` paid per object
before, so this does not make it worse; the batch paths (push, import,
locator) flush once per batch with `syncfs`. If it shows up, the fix is group commit in
`storage.rs`: concurrent writes share one `syncfs` per few milliseconds and
each request is answered only after its sync.

### 4.3 What goes away

- The `repack --geometric=2 -d` in the `CAOS_STORE_CHECK` block of `main.rs`
  and its comments; the closure check stays.
- The pack-count fatal and warning at startup become a warning if
  `objects/pack` holds anything at all, since nothing should write a pack any
  more.
- `sync_loose_object`'s "already packed" branch.
- The watchdog runbook's pack-count step (`prod/watchdog/runbook/CLAUDE.md`)
  becomes "any pack is a bug".

### 4.4 What does not change

- No garbage collection. The store keeps growing, now at about 0.45 GiB on
  disk per GB of raw objects. With 800+ GB free on the host that is years at
  today's rate; retention stays in `design/caos-store-and-gc.md`, and is
  simpler on this layout (deleting is an unlink, with no pack to rewrite).
- Refs, reflogs, repair, the git protocol and every client.
- `secrets.git` keeps its own rules (no alternates, immediate prune).

## 5. Risks

| risk | mitigation |
| --- | --- |
| btrfs reorders renames across a crash | power-cut test before prod; closure check at every start |
| a path still writes a pack | startup warning; watchdog alert on any file in `objects/pack` |
| btrfs metadata ENOSPC on a full volume | size generously; alert at 70% used, from `btrfs filesystem usage` |
| dockerd picks its btrfs storage driver on the new data root | `storage-driver = "overlay2"` pinned in `daemon.settings` |
| redis's append-only file fragments under copy-on-write | its directory is `chattr +C` |
| backups or snapshots of `/data` assume ext4 | check whatever snapshots the data disk before the swap |
| large fetches cost more CPU | accepted; a pack cache for immutable roots is the fix if it ever matters |

## 6. Migration

All of `/data` moves to a new btrfs volume; the old ext4 volume stays
attached read-only, then detached, as the rollback. What is on it
(2026-11-06, 202 GB used of 1 TB):

| path | size | in the move |
| --- | --- | --- |
| `/data/caos` (repo, registry, redis, iroh state) | 21 GB | copied; the repo's objects exploded (step 4) |
| `/data/docker` (containerd image snapshots) | 183 GB | not copied if its images can be pulled or rebuilt; the rehearsal confirms which |
| `/data/exp`, `/data/caos-store-test` | 2.4 GB | copied or dropped |

One downtime, rehearsed first on a copy as in section 2. The copy of
`/data/caos` and the explode (2 min 16 s measured) set its length, well under
an hour if `/data/docker` is rebuilt rather than copied.

1. Attach a new volume, format it btrfs with the data disk's label
   (`prod/caosd/configuration.nix` finds the disk by label), mount it at a
   temporary path, `chattr +C` an empty `caos/stack/redis` on it.
2. Stop `caosd` and dockerd.
3. Copy everything under `/data` but `caos/stack/git/objects` (and
   `/data/docker` if it is to be rebuilt) with `rsync -aHAX --numeric-ids`.
   In the repo that includes config, `HEAD`, `refs/`, `packed-refs`, `logs/`
   (repair restores refs from reflogs), `hooks/`, `caos-imports/`,
   `caos-commit-checks/`, `caos-locators/`, and `secrets.git` if it lives
   there.
4. Explode every object into the new `objects/`:
   `git cat-file --batch-all-objects --batch-check='%(objectname)'` in the old
   repo, piped as names to `pack-objects --stdout --window=0`, piped to
   `git -c core.looseCompression=0 unpack-objects` in the new one. Order does
   not matter: the server is down, and a failure means start again.
5. Verify: the object count matches, 2,000 sampled objects are
   byte-identical through `cat-file --batch`, and the closure check passes.
6. Deploy the configuration with the new volume as `/data` and a server
   with this change; start.
7. Keep the old volume for two weeks. Rollback: stop, copy the new repo's
   loose objects into the old volume's `objects/` (old servers read loose
   objects fine), swap the volumes back, deploy the old configuration and
   server.

The side repos' `objects/info/alternates` hold the canonical path of the
server's `objects/`, which is the same after the swap.

## 7. Plan

1. This document.
2. Server: config (section 3), `publish_loose` and the three ingress paths
   (4.1), level-0 gix writes, removal of the repack (4.3), tests: a push, an
   import and a locator each leave zero packs, and a reader polling during a
   push never sees a parent without its children.
3. Power-cut test of `publish_loose` on btrfs.
4. `prod/caosd/configuration.nix`: the volume; the watchdog alert.
5. Rehearse the migration on a copy, then run it.

## 8. Open questions

- `-m single` or DUP for metadata (0.9 GiB today).
- Does gix-odb let the server choose the loose compression level?
- Should the server serve with `pack.window=0`? It cuts the CPU of a large
  fetch from 192 s to 33 s and roughly doubles its bytes; small fetches do
  not care.
