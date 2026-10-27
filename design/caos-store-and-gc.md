# caos: owning the object store, and collecting garbage in it

Status: proposal for discussion. Written against `Metta-AI/caos` at
`ca150247b2982fd7bf119371aea215def5a433bb`, with PR #351 (`0f41904`,
2026-10-27) read separately.

## Summary

caos uses git for three separable things: the **object format** (blobs,
trees, commits, gitlinks, content addressing), the **wire protocol and
ecosystem at the edges** (import and publish with history, locator fetches,
merge/log/diff, humans running `git`), and **repository management** (the
object database layout, packs, maintenance, reachability as liveness,
reflogs, `http-backend` writing into the directory that `gix` reads).

The format fits caos well and the edges are worth keeping. Every workaround
in the codebase is a cost of the third thing: git manages a store whose
liveness model is not git's. Garbage collection is off because git's
collector would delete most of the store, and it cannot be switched on
without first teaching git what caos means by live, which git has no hook
for.

The proposal is to keep git objects as the encoding, keep git at the edges,
and make caos the owner of the store: its own content-addressed store keyed
by git object id, its own ref table, and its own collector. The two open
questions are what the store is built on and how the collector copes with
the fact that, in a content-addressed store whose hashes travel through
models, caches and tickets, a dead object can come back to life.

## 1. What git does for us today

One bare repository per server holds everything, and the server is both its
in-process reader and the parent of the git subprocesses that write to it.

### 1.1 Topology

- The repo is `CAOS_GIT_DIR` = `$CAOS_STACK_STATE/git` (`$CAOS_DATA/stack/git`
  under `caosd`; prod mounts `/data`). Dev stacks on one machine share a
  repo in the named volume `caos-vol-caos-dev`.
- The server opens it once, in process, as a `gix::ThreadSafeRepository`
  for its lifetime, and at the same time serves it to `git http-backend`
  (`/info/refs`, `/git-upload-pack`, `/git-receive-pack`), whose
  subprocesses write to the same directory.
- Side repositories inside `git_dir` point at the server's objects through
  `objects/info/alternates`: `caos-commit-checks/` (validates pushed commits
  with `pack-objects --stdout | index-pack --strict`), `caos-imports/`
  (staging for `POST /git/import`, plus completion markers), and the
  locator's disposable `--depth=1` repo. `secrets.git` is deliberately not
  linked: no alternates, no fetch endpoint, `gc.auto 0`,
  `core.logAllRefUpdates false`, `git prune --expire=now` after every push,
  because a replaced secret must be gone.
- Not git: Redis (`caos:result:*` maps an ArgTree hash to `"<type> <hash>"`;
  `caos:trace:*`, images, leases), bounded by `allkeys-lru` at 2 GB by
  default. The registry holds converted images; git holds the image
  inputs. `/cas` exists only inside worker containers, materialised from
  git objects.
- Workers and clients each use a throwaway local repo with `origin` =
  server, `gc.auto 0`, `receive.autogc false`, exact-ref fetches
  (`fetch.negotiationAlgorithm=noop`, tree filters) and `--atomic`,
  `--force-with-lease` pushes.

### 1.2 How objects get in

Four ingress paths, each leaving a different on-disk shape.

| path | what it writes | notes |
| --- | --- | --- |
| `POST /object` | one loose object per call, through gix (`rename()` of a temp file; gix never fsyncs, so `storage.rs::sync_loose_object` does) | workers over HTTP write objects as they build them; `ensure_pushed` is a no-op for them; `caos put` can land thousands of loose objects per call |
| `git push` via receive-pack | one pack per push, never exploded into loose objects (`receive.unpackLimit 1`, so `receive.fsckObjects` and `index-pack --strict` verify it as a unit) | a test suite adds hundreds of packs; each pushed object also leaves a transient `refs/caos/req/<hash>` as a negotiation base |
| `POST /git/import` | a verified pack hard-linked into `objects/pack/`, index published only after the pack is synced | fetched into a staging repo; refuses anything but a single-pack layout; writes a completion marker under `caos-imports/` that is explicitly not a ref, GC root, permission or invocation record |
| locator resolution | only the tree closure, pinned at `refs/caos/locator-trees/<rev>` | `--depth=1` fetch into a disposable repo; nothing anchors the commit, by design (a locator wants a snapshot, not a history) |

The invariant on every path: an object is stored only after everything it
references is stored (SPEC: uploads need deps first; transfers verified
before visible). `verify_object_closure` at startup enforces it for the
whole store: every object is a root, `rev-list --objects --missing=error`,
and the server refuses to start if anything is missing, never deleting
history to make an upgrade pass.

### 1.3 Loose objects and packs

Git's object database has two tiers. Loose objects are one file per object,
individually zlib-compressed, no deltas: cheap to write, expensive to have
many of (inodes, directory scans, one `open` per read). Packs are
append-only files of many objects, delta-compressed against each other, with
an `.idx` for binary-search lookup. Reading an object means probing every
pack's index until one hits, so read cost grows with pack count, and gix
fixes its pack slot table at open time (default 32, hard ceiling 32,767;
the server opens with the maximum and refuses to start when it is reached).

With maintenance off, both tiers only grow:

| store | measured | effect |
| --- | --- | --- |
| prod, 2026-10-24 (PR #346) | 94,074 files, 3.2 GB, mostly loose | downtime `repack -a -d --keep-unreachable` took 3 min 46 s; the closure walk after it 11 s |
| dev, 2026-10-27 (PR #351) | 8,583 packs and 111k loose objects | test suite 2 min 28 s to 68 min 39 s, because every gix read paid for every pack; one pack fixed it; a multi-pack index was measured and was worse |
| dev, earlier (design/faster-tests.md) | 2,255 refs, 116k loose, 5 packs | a full ref advertisement cost 42 ms; "the leak is still real and still unaddressed; it is a disk problem" |

## 2. What git's maintenance would do, and why it is off

A normal repository stays healthy because git maintains itself after writes;
every one of those mechanisms is disabled here, for two reasons that are
really two different problems.

| mechanism | git's default | what it would do here | state in caos |
| --- | --- | --- | --- |
| `gc --auto` after writes (`gc.auto`, `gc.autoPackLimit`, `receive.autogc`) | 6,700 loose objects or 50 packs; receive-pack forks it after every push | repack reachable objects, delete unreachable ones after `gc.pruneExpire` (2 weeks) | `gc.auto 0`, `receive.autogc false` |
| `--cruft` packs | part of gc since 2.37 | move unreachable objects to a cruft pack with mtimes, drop them on a later run: two-stage, but keyed on reachability | never run |
| `geometric-repack` maintenance task | added in git 2.54; ignores every `gc.*` knob | repack on a geometric schedule while readers are live | `maintenance.geometric-repack.enabled false` |
| `reflog expire` | 90 days (30 for unreachable), run by gc | drop the history `repair.rs` restores refs from | never run; `core.logAllRefUpdates always` |

**Reason 1: a rewrite under a live reader (a crash problem).** A repack
moves packs. `git-upload-pack` streaming a fetch from a pack being rewritten
can truncate the stream (`fetch-pack: invalid index-pack output`), and the
in-process gix repository, whose slot table is fixed at open, asserts ("if
the generation changed, the slot index must have changed for sure") and
unwinds the request thread into a body-less 500 that logs nothing. This is
why the only sanctioned repack is in `caosd up`, when the stack is down, no
worker container is alive, and a shared flock is held: `git repack
--geometric=2 -d` since PR #351, which works from packs rather than refs and
keeps unreachable objects; `CAOS_STORE_CHECK=yes` adds `repack -a -d
--keep-unreachable` plus the full closure walk at `caosd` start.

**Reason 2: reachability is not liveness (a data-loss problem).** From
`main.rs`: "nearly every object here is unreachable from a ref (results and
trees are addressed by hash), so a cruft repack sorts the CAS into a cruft
pack and drops whatever has not been touched in two weeks, while redis keeps
handing out result hashes that point at it." No git knob fixes this; git has
no hook for an external liveness model.

So the operational problem (pack and loose-object growth) is solved on main,
in downtime. The retention problem is untouched by design, because nothing
can yet say what is safe to drop. Two statements in the repo are stale and
should be fixed regardless of what follows: `main.rs` cites "design/chat.md
lists retention as a known shortcoming" (no such text exists), and README's
Cleanup note tells a deployment to "define a policy and run `git gc`", which
as written would delete most of the store.

## 3. Refs, reads, integrity and repair

Refs are caos' only durable names, the compare-and-swap primitive, and a
per-connection cost; everything else is fetched by bare hash.

| ref | writer | role |
| --- | --- | --- |
| `refs/caos/req/<hash>` | client `ensure_pushed` (`push <hash>:refs/caos/req/<hash>`) | negotiation base for that client's next push; hidden from upload-pack; the server's only deletion: swept every 120 s, kept 600 s by loose-ref mtime; "they anchor nothing" |
| `refs/caos/res/<argTree>` | server `pin_result`, top-level external runs only | durability pin "so a client can fetch it by ref and it survives gc"; hidden from advertisement; never read back as a cache; accumulates forever. Sub-runs, map children and cache hits pin nothing |
| `refs/caos/locator-trees/<rev>` | locator | rev-to-tree memo and retention root for a tree closure; hidden |
| `refs/caos/v3/conversations/<id>/head` | llm-step, run-and-update-ref, MCP hook, TUI | conversation tip; never rewound by repair |
| `refs/caos/v3/users/<key>/conversations/{active,archived}/<key>` | clients | membership |
| `refs/caos/seed`, `refs/caos/dev`, `refs/caos/bins` | bootstrap, `caosd up`, test-stack image | seed records, the dev checkout, host binaries |
| `refs/heads/actors/<name>` | `std/actor` | actor state branch, CAS by `--force-with-lease` |

Three costs live here. The advertisement is sent whole on every connection:
2,560 refs was a 251 KB advertisement and a 258 ms push of one small object,
against 181 ms at 1,231 refs, which is why `req` refs are swept and `res`
and `locator-trees` are hidden. Reflogs are kept for every ref and never
expired, because `repair.rs` restores a damaged `req`, `res` or `v3` ref
from its newest readable reflog value and no retention policy exists. Ref
updates are the CAS primitive: `update-ref` with an expected value, retried
on `.lock` contention (`pin_result_in` retries eight times),
`--force-with-lease` from workers.

**Reads.** The server reads through gix in process for every ArgTree
evaluation and for `GET`/`HEAD /object/<hash>`. Clients fetch anything by
bare hash (`uploadpack.allowAnySHA1InWant true`, `uploadpack.allowFilter
true`), with hidden-ref juggling so result objects stay visible to
receive-pack negotiation. `caos-conversation-list` is `git ls-remote --refs`
over the v3 prefixes; `caos-conversation` walks the first-parent chain from
a tip hash.

**Integrity and repair.** `core.fsync objects,reference`, `core.fsyncMethod
batch`, `receive.fsckObjects`, `receive.shallowUpdate false`, commit
validation in a staging repo, the closure check at startup. `repair.rs`
deletes zero-length loose objects left by a crash, then drops or
reflog-restores refs that name unreadable objects, except that a v3 head is
never rewound: its loose ref is dropped and the reflog kept for a human to
restore from.

## 4. Liveness in caos is not git reachability

Objects fall into two classes, and git's model is nearly right for one and
wrong for the other.

**Ref-rooted.** Conversation heads and membership, actor branches, seed,
dev, bins, pinned top-level results, locator trees. One hole: gitlinks. A
conversation tree names every attached source tree as a mode-160000 entry,
and result trees carry `commit <hash>` entries the same way. Git's
reachability never crosses a gitlink (SPEC: "gitlinks name separate
histories"), so every imported repository a conversation depends on is
unreachable from that conversation's head.

**Hash-rooted.** Everything else, which is most of the store: every ArgTree
after its `req` ref is swept; every result of a sub-run, map child or cache
hit; every imported or locator-fetched commit; `/cas/out-trace` blobs whose
hash lives only in a Redis trace event; every `<kind> <hash>` that
`GET /eval` returned to a model and that the model later passes back as
`root` or `in`; the tip hashes that `caos-conversation` and
`/fork-caos-conversation` take. Their referrers are Redis values, process
memory, a model's context window, tickets, trace lines, and text inside
conversation events (`"result":"<object>"`), none of which git can see.

### 4.1 Resurrection

Here the analogy with a memory collector breaks, and it shapes the whole
design. In a memory GC a dead object cannot become live again: nothing
holds its address. In caos:

1. **Hashes travel outside the store.** A model holds `root=4299faef…` from
   an `eval-path` it ran an hour ago; Redis holds the result hash of a run
   from last month; a ticket names a task whose terminal event names a
   result. Any of them can be presented back at any time, as a read or,
   worse, as a reference inside a new write.
2. **New writes can point at currently-dead objects.** A client probes
   `HEAD /object/H` (exists), builds tree `T` naming `H`, and `POST`s `T`.
   If a sweep removed `H` between the probe and the write, the store now
   holds `T` with a missing child, and the closure invariant the startup
   check enforces is broken by a write that looked valid.
3. **Content addressing means identity survives deletion.** If `H` is
   deleted and a client later uploads the same bytes, `H` is back under the
   same name. "Dead" can only mean "the bytes are not here now", never "this
   hash is retired". That is good (resurrection by re-upload is always
   correct) and dangerous (a reference to `H` is never provably wrong, so
   the store cannot reject it on principle; it can only check presence at a
   moment).

So a mark phase is never once-and-done, and mutator and collector cannot be
separated by reachability alone. The collector needs a write barrier and a
notion of time.

## 5. Problems we probably need to solve

Fourteen, grouped: liveness (P1–P6), policy (P7–P8), the store itself
(P9–P10), edges and operations (P11–P14).

**P1. Roots and references must be structural.** The walker can only follow
tree entries, commit parents and gitlinks. Today some durable references
are textual (`"result":"<hash>"` in conversation events, out-trace hashes in
Redis, `<kind> <hash>` strings). Either the protocol makes everything a
conversation depends on a tree entry or gitlink in its tree, or the walker
parses every such format forever. Hashes a model writes into free text are
not roots; the grace window covers them.

**P2. The write barrier.** Every write validates the closure of what it
writes at commit time, inside the same critical section the collector uses,
and either rejects ("missing object H"; the client re-uploads, idempotent
under content addressing) or accepts and atomically marks `H` live. The
ingress paths already check closure; the check becomes the barrier instead
of a preflight.

**P3. Reads and probes are liveness signals.** `GET`/`HEAD /object/<H>` and
every gix read during evaluation bump `H`'s last-touched time, so an object
a model is using is never in the oldest tier. Git's only version of this is
loose-object mtime and the cruft-pack mtime table.

**P4. Grace window and epochs.** Mark runs against snapshot epoch `E`.
Anything written or touched after `E` is live regardless. Anything dead at
`E` and untouched for longer than `G` (proposed 30 d) is a candidate. This
makes P1's textual references and the model-holding-a-hash case safe
without enumerating them.

**P5. Two-stage deletion.** Candidates are tombstoned (hidden from new
references, bytes kept, reads answer "gone, recoverable") for a second
window before physical reclaim. A reference to a tombstone revives it.
Cruft packs do this for reachability; caos needs it for its own liveness,
and it is the undo path when a root class was forgotten.

**P6. Closure-complete deletion.** The live set is closed under references
by construction, so no live object points at a dead one. The sweep deletes
only from the complement, never partially; a crash mid-sweep leaves either
state, not a mix. The startup closure check stays as the audit.

**P7. Retention policy.** Proposed classes: conversation heads, membership,
actors, seed, dev, bins: keep. Pinned results and locator trees: `T_res`
(90 d) since last pin or touch, then the pin drops. Import markers:
`T_cache` (90 d); re-import re-verifies. `req` refs: 600 s as now. Redis
result cache: a cache; a hit whose object is gone is a miss and recomputes,
so the compute path verifies presence on a hit rather than trusting Redis.

**P8. Refs, CAS and history.** A ref table with compare-and-swap replaces
`update-ref` and `--force-with-lease`. A bounded journal replaces reflogs
for repair, with explicit retention instead of forever, and the v3 rule
(heads never rewound automatically) preserved.

**P9. Storage efficiency.** Git delta-compresses similar objects: successive
versions of a file, tool outputs, near-identical trees. A store with
per-object compression loses that. Measure it now on the real repo
(`git count-objects -v` size-pack after a full repack, against the sum of
per-object zstd sizes) to set how aggressively the new store should delta.

**P10. Concurrent compaction.** Reclaiming space rewrites the container, and
readers must never see a torn object. Immutable segments plus a
generation-swapped index (readers hold a snapshot) is the standard answer
and exactly what gix cannot do over git's layout.

**P11. The edges.** Import: fetch into a staging repo as now, then ingest
the pack (gix-pack decodes a pack without a repository). Publish:
materialise the closure into a staging repo and push. `log`, `show`,
`diff`, `merge`: run git in a materialised staging repo for that source
tree. Clients that `git fetch caos <hash>` directly lose that; the iroh
transport already dropped the git services (PR #343).

**P12. Sharing and isolation.** Several dev stacks share one store; the
collector coordinates like the repack does (one flock) or is a single
server-side process. `secrets.git` stays separate with immediate deletion.

**P13. Migration.** Export every object from the current repo (the closure
walk already visits all of them), dual-run with the new store
authoritative, verify by sampled reads, then make the git repo a disposable
export. The export must be all objects, not reachable objects: the gitlink
roots conversations depend on are not reachable today.

**P14. Observability first.** Before any deletion: a store report counting
objects and bytes by root class, age since last touch, and gitlink fan-out;
a dry-run collector that reports what it would tombstone; several cycles on
dev and prod.

## 6. What to build the store on

Lean: caos-owned pack segments (option B), with a small embedded KV for the
index and per-object metadata.

caos needs what packfiles are for, whatever it is built on: a store of
hundreds of thousands of small objects needs a container (too many files is
the loose-object problem already hit), an index for lookup, sequential I/O
for bulk reads and writes, and delta compression between similar objects.
An LSM key-value store gives the first three (an SSTable is an immutable
packfile with an index, plus compaction) but not the fourth, and caos
content deltas well: versions of source files, near-identical trees, tool
outputs. Git's pack format gives all four, gix-pack reads and writes it, and
a segment is pushable and importable at the edge unchanged. The live-reader
problem is not inherent to packs; it comes from git rewriting them in place
under a fixed slot table, which immutable segments and a generation-swapped
index remove. What git's format lacks (last-touched, tombstone state, root
class) goes in the side index. Deltas stay within a segment, so compaction
rebases them when it rewrites one. The P9 measurement then sets how
aggressively to delta, not which container to use.

Criteria, from section 5: per-object metadata; compaction that does not
block readers; large blobs; a closure check inside a transaction; an answer
to P9; the Rust ecosystem; how much we write ourselves.

| option | what it is | gets for free | costs |
| --- | --- | --- | --- |
| A. Embedded KV (RocksDB, or a Rust-native LSM such as fjall) | objects, refs and metadata as column families | compaction with live readers, transactions, per-key metadata: most of P2–P8 and P10 | no deltas (P9), write amplification, large-blob handling (BlobDB or a side file store), an operational dependency |
| B. caos-owned pack segments | objects in git pack format written by gix-pack, immutable once sealed; a small KV index (id to segment, offset, last-touched, state) | keeps deltas; a segment is directly pushable at the edge; compaction = new segment from the live set, swap the index generation, unlink the old | we write the index, the swap and the crash safety: an LSM over packs |
| C. Sharded files with per-object zstd | one file per object | simplest | the loose-object problem again (inodes, directory scans); mtime as the only metadata |
| D. Keep the git ODB, own refs and maintenance | caos computes the live set (gitlinks, grace window) and expresses it as one octopus "retention commit" at `refs/caos/retain/<ts>` so stock `git repack --cruft` deletes correctly in downtime | cheapest to build | keeps every live-reader constraint and the downtime requirement; P2 is unsolvable while receive-pack writes behind the server's back. The fallback if a migration is more than a year out |

## 7. The collector

Concurrent and online, with no downtime, because the store owns its index
and readers hold snapshots.

1. **Snapshot.** Epoch `E` = now. Enumerate roots (sections 4 and 5, P1 and
   P7) at `E`.
2. **Mark.** Traverse structurally (commit parents, trees, gitlinks, result
   `commit` entries) to a fixpoint. Everything visited is live.
3. **Barrier.** During and after marking, every write checks closure and
   marks what it references; every read bumps last-touched. Anything
   touched after `E` is live.
4. **Tombstone.** Objects not marked, last-touched older than `E - G`,
   become tombstones. Reads answer "gone, recoverable"; a write referencing
   one revives it.
5. **Reclaim.** Tombstones older than the second window are physically
   removed at the next compaction. From then on a reference to that hash is
   "missing": a write is rejected and the client re-uploads; a cache hit
   recomputes.
6. **Audit.** The closure check, now over the store's own index, runs after
   each reclaim and at startup.

Resurrection is handled at each stage: before tombstoning, a touch or a
reference keeps the object out of the candidate set; during the tombstone
window, a reference revives it; after reclaim, the write barrier refuses the
dangling reference and content addressing lets the client put the bytes
back under the same name. At no point can a write that succeeded leave the
store with a missing child.

Policy knobs: `G`, the tombstone window, `T_res`, `T_cache`, and whether
archived conversations expire.

## 8. Sequencing

Measure before building, make references structural before either store,
and move the hash-rooted class first.

1. Fix the two stale statements now (section 2): the `main.rs` reference to
   design/chat.md and README's "run `git gc`".
2. Measure: P9 (delta savings on the real repo) and P14 (store report by
   class and age), on prod and dev.
3. Make conversation references structural (P1). Independently valuable and
   a prerequisite for either store.
4. Build the caos store behind the existing object API, with the ref table
   and journal; dual-run; migrate the hash-rooted class first (results,
   ArgTrees, traces, imports), the population git's model is wrong for and
   most of the bytes.
5. Collector in dry-run; then tombstoning on dev stacks; then prod.
6. Decide what the remaining git repo is: an export cache that can be
   freely gc'd because it is no longer the store of record, or gone.

## 9. Open questions

- `G` and the tombstone window: 30 d each is a guess; the store report will
  show the real age distribution of untouched objects.
- Is losing a sub-run result older than `G` and never pinned acceptable
  when the recompute is an `llm-call`?
- Does anything external depend on `git fetch caos <hash>` or on
  `ls-remote` of conversation refs?
- How aggressively to delta within a segment, after the P9 measurement.
- Do archived conversations ever expire, and are a conversation's gitlinked
  source trees pinned per conversation or shared?
