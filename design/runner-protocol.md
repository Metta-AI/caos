# Runner protocol — sequential jobs on warm workers

**Status:** design agreed, not yet implemented. Replaces the existing
backends outright: `dispatch_docker`/`dispatch_serve`/`dispatch_fly`, the
`Backend` enum, the worker-slot semaphore, `caos entrypoint`, and `caos serve`
are all deleted, and the dev stack gains `caos runnerd` as a required daemon.
Builds on the runner-pool decomposition (`runner-pool-and-cloud-builds.md`):
that doc removes the per-worker *image*; this one removes the per-job
*container start*.

---

## Problem

Every job today pays a worker start: a fresh `--rm` container (Docker backend)
or a machine dispatch (fly). A burst of jobs for the same image — the normal
shape under `map-then` — restarts identical workers over and over. We want a
finished worker to stay warm and take the next matching job, without a
scheduler owning worker lifecycles.

## Shape

Pull, not push. Anything that can run work **long-polls the server**; the set
of parked polls *is* the available capacity, at its current warmth. The server
never starts, stops, or counts workers — it matches pending jobs against
hanging polls and answers them.

- A poll carries **required args**: key → git-oid pairs. It matches a job iff
  every required entry equals the job's args-tree top-level entry of that name
  (pure oid equality — a `docker://` image matches on the oid of its ref blob).
- A runner with `required: {}` is **generic**: it matches anything, and is how
  new capacity gets minted (a host agent that runs `docker run`). There is no
  server config file — generic runners are ordinary self-registered pollers,
  just ones that happen to be daemons.
- **Most required keys wins; ties go LIFO** (most recently parked), which
  concentrates work on a stable hot runner and lets the surplus tail idle out.
- **Nesting rule:** a process that starts an inner runner (host agent → its
  container; later, container → a resident worker daemon) does not poll again
  until the inner one dies. At any moment each physical slot has exactly one
  hanging poll somewhere in its lineage — capacity accounting is structural.

## Wire protocol

Two endpoints. Both take `Authorization: Bearer <CAOS_RUNNER_TOKEN>` (shared
secret from env; the job payload carries the token so children inherit it —
closes "anyone can poll jobs containing curried creds").

### `POST /runner/poll`

```json
{
  "required": { "image": "<oid>" },   // {} for a generic runner
  "lineage":  [ {} ],                 // ancestors' required sets, outermost first
  "ttl_ms": 2000
}
```

Hangs until one of three replies (all 200, JSON):

- `{"job": {...}}` — work (below).
- `{"idle": true}` — TTL expired, no match. The runner chooses: poll again or
  exit. **A runner's idle budget is its poll TTL** — one poll per idle window,
  exit on `idle`. Ski-rental: set TTL ≈ own restart cost.
- `{"exit": true}` — eviction: a pending job matches this runner's *lineage*
  but not the runner itself. Exit so the parent resumes polling; the parent
  matches or is kicked in turn. This cascade is the anti-starvation mechanism
  (an idle warm runner can't indefinitely hog a slot a generic job needs).

The job payload is the rendezvous ids plus only what the runner can't derive:

```json
{
  "req": "<reqHash>", "nonce": "<hex>",
  "image_ref": "<docker ref>",
  "deadline_ms": 0,
  "token": "<runner token>"
}
```

No `args`/`std`/`salt`: those are exactly the entries of the `req` tree, which
the runner unpacks itself (one tree fetch, plus the `std` ref blob) — `req`
has to travel anyway for the result post, and sending only it means the
payload can't disagree with the request. `image_ref` is genuinely
non-derivable: it's the docker-pullable ref produced by the server's git→OCI
convert + registry push (`resolve_image`). It's always sent (fixed-shape
payload); a warm runner that pinned `image` just ignores it, and conversions
are Redis-cached, so re-resolving a seen image is a lookup, not a push. No
other scheduling metadata:
a runner learns the oids for its next `required` set from its own
materialization of the job's args (see "container runner" below).

### `POST /runner/result`

```json
{ "req": "...", "nonce": "...", "ok": true,  "result": "tree <hash>" }
{ "req": "...", "nonce": "...", "ok": false, "error": "...", "log": "<stderr tail>" }
{ "req": "...", "nonce": "...", "requeue": true }
```

First post per nonce wins; unknown/consumed nonce → 410, dropped. `requeue`
returns the job to the pending table under a fresh nonce (for provision-style
runners, below). A `promise <hash>` result flows through unchanged —
`resolve_promise` runs in `run_req` after the rendezvous, as today.

**Reply responsibility follows the poll, but may be delegated**: the host
agent polls job 1 and hands it to the container it starts; the *container*
posts the result and then polls for itself. The delegator is the crash
backstop — container exited nonzero → the agent posts an error result with
captured stderr (harmlessly 410'd if the container already posted).

## Server internals

`run_req` is untouched before and after dispatch (cache, cycle detection,
promise resolution, caching). The runner backend replaces only the
`dispatch_*` call: enqueue `{req, nonce, payload, deadline}` → match/deliver →
block on a condvar until a result posts.

One mutex over two tables, matched in both directions on every arrival:

- `parked: Vec<Poll>` — required, lineage, expiry, delivery channel.
- `pending: Vec<Job>` — nonce, payload, deadline, waker.

Details:

- **TTL margin**: a poll stops matching in its last ~1s, so a job isn't handed
  to a connection the runner is abandoning.
- **No job deadline** (2026-07-22): a claimed job runs until its result
  arrives; `deadline_ms` is always 0 (kept for payload shape). The earlier
  deadline-plus-requeue presumed a slow worker dead and raced a fresh one
  against it — nothing killed the old container, so duplicate long jobs
  (20-core toolchain bakes) compounded until the machine thrashed.
  "Spurious re-runs are wasteful, never wrong" is only survivable when the
  waste is small. Dead-worker detection is future work, likely leases
  (liveness from the worker's own server traffic, never inferred from
  slowness). The *voluntary* requeue (`requeue: true` — the provisioning
  handoff) is unaffected.
- **Pending deadline**: a job no poll and no lineage can serve waits for new
  capacity, then fails 503 after a dispatch timeout (~60s).
- **No worker-slot semaphore on this path**: capacity is runner-side.
  `CAOS_MAX_WORKERS` becomes the host agent's `CAOS_RUNNER_SLOTS`.

## Runner roles

**Host agent** (`caos-runnerd` — the one new daemon; config via env: server
URL, token, `CAOS_RUNNER_SLOTS`, docker bin/network). N independent loops:
poll `{}` → `docker run --rm … --entrypoint /bin/caos <image_ref> runner
--job=<json>` → wait for container exit → post error on nonzero exit. Always
polls again; generic runners never idle out.

**Container runner** (`caos runner`, successor to `serve`). Where `serve`
shells out to `caos entrypoint` per job (three processes: serve → entrypoint →
`/worker`), the runner loop **owns the CAS lifecycle itself**: `entrypoint`'s
bundled setup/run/teardown splits into library stages, and the long-lived
runner process calls them in-process (setup `/cas`, fork `/worker`
unprivileged, read `/cas/out`, teardown, `reset_after_job`) — two processes,
no middle layer. Doing the materialization itself means the runner has the
args tree's top-level name→oid listing in hand as a side effect — that's where
its next poll's `required: {image: <oid>}` comes from (`fetch_and_materialize`
already fetches the tree and stamps each child's oid; no extra round trip, no
protocol field).

A one-off job is just the degenerate case: a runner whose linger TTL is zero.
So runner mode subsumes both `entrypoint` and `serve` — **both are deleted**,
along with the old dispatch arms, in the same change (no transitional
wrapper: once the arms go, nothing invokes them). Nothing calls `entrypoint`
as a subprocess anymore — the runner loop calls the stages in-process. If
hand-running a container without a server rendezvous matters for debugging, a
`--print` flag on runner mode covers it. Loop: run the handed-in job, post result, poll `{image}` with
TTL ≈ container start cost, until `idle`/`exit` → exit 0.

**Resident worker daemon** (`{image, bin}`) — **deferred**. The container
runner already serves any `bin` for its image at fork+exec cost; the only
further win is warm process state, which drags in real questions (who cycles
the root-owned `/cas` for a live worker; hermeticity becomes the daemon
author's problem). The protocol needs nothing new for it later: the worker
posts its own result and polls `{image, bin}` while the caos parent waits on
child death, per the nesting rule.

## Fly (sequenced last)

The docker agent is exec-style — it hands the job to the container it starts.
A fly agent is provision-style: it can only ensure a machine for the image is
up (booting `caos runner` in pure-poll mode), so it **requeues** the job and
the machine's `{image}` poll claims it. Guard against ping-pong (the agent
re-claiming the job it just requeued before the machine warms): the requeue
carries `defer_generic_ms`, during which the job matches only polls with ≥1
required key. Fly support leaves the tree with the other backends (most of it
lives on `fly-serve-backend` anyway) and returns as this agent; the `requeue`
verb is specced now so the protocol is stable when it does.

## Out of scope

- Resident worker daemons (above).
- Batch object fetch (a tree plus its children's contents in one hop) — a nice
  transport optimization someday; nothing here needs it. A plain `caos get` on
  a tree is already a one-hop "get children" at the names+oids level.
