# Multiplexed workers — many concurrent jobs in one worker process

**Status:** design (2026-W45-5). Nothing is built. The first user is the wasm
host (`design/wasm-workers.md`): many jobs, each in its own V8 isolate, in one
Node process.

## Problem

A worker process handles one job at a time. Even a resident worker
(`design/daemons.md`) takes its jobs in series: `caos next` means "this job is
done AND give me the next one", and the runner has one job, one `/cas/args`
and one `/cas/out`.

That is right for a process whose jobs share its memory, its `/tmp` and its
processes. It is wrong for a host whose jobs are already isolated from each
other inside the process, such as V8 isolates, where a job costs a thread and
a few MB rather than a process. Such a host wants N jobs in flight at once,
and wants each to start and finish on its own.

## Where "one job per process" lives today

| layer | what assumes one job | changes? |
|---|---|---|
| server (`rust/crates/server/src/runner.rs`) | Nothing, for unkeyed jobs. A poll carries no runner identity, so one process may keep several polls hanging; each result is posted by its own nonce; `/sub-run` is authorized by nonce. Only keyed jobs have the one-in-flight rule (`owner_poll`: `409 tenure has a job in flight`). | **No**, for unkeyed jobs. |
| container runner (`rust/crates/caos/src/runner.rs`) | One `RunnerJob` and one state machine (`life`, `supervise`, `next`). One `/cas/args`, `/cas/out`, `/cas/out-trace`, `/cas/nonce` (`prepare`) and one `/secret` (`write_secrets`). `reset_after_job` wipes the scratch dirs and reaps every worker-uid process; `narrow_reset` removes the single job's entries. | **Yes**: this is most of the work. |
| worker-side `caos` (`rust/crates/caos/src/lib.rs`) | Finds its job's context under one CAS root: `args`, `out`, and the `nonce` file (`job_nonce`, `job_salt`). | **Yes**: it must be told which job a call is for. |
| `caos next` | "Done" and "want more" are one message, and only for a keyed job. | **Replaced**, for this mode, by a stream with credits. |

## Shape

An image declares `CAOS_CONCURRENCY=N` in its environment, like
`CAOS_RESIDENT`. A caller cannot ask for it. Such an image's `/worker` is a
**multiplexed worker**: it starts once, stays up, and runs up to N jobs at a
time, each in a namespace of its own.

```text
runnerd ── container: caos runner (root) ── /worker (uid 1000, one process)
                 │   N polls hanging,               │   job j1 ─ isolate
                 │   one claim per job,             │   job j2 ─ isolate
                 │   posts each result              │   …
                 └──── caos jobs --stream ─────────┘   (one line protocol)
```

The runner stays the only poller and the only poster, as for resident
workers. The worker needs no HTTP, no token and no knowledge of the runner
protocol.

## Per-job namespaces

Each job gets its own root-owned directories, made by the runner when the job
is claimed and removed when its result is posted:

```text
/cas/jobs/<id>/args/…      the ArgTree, materialized lazily as today
/cas/jobs/<id>/out         the result, single-assignment
/cas/jobs/<id>/out-trace   optional perf data
/cas/jobs/<id>/nonce       root-only, read by /bin/caos
/secret/<id>/<name>        the job's granted secrets
```

`<id>` is minted by the runner: short, never reused within the container's
life, never derived from anything a worker chose.

**The worker-side `caos` takes `--job <id>`** (or `CAOS_JOB=<id>`). With it,
the CAS root for that call is `/cas/jobs/<id>`: relative arg paths, `out`,
the nonce for `sub-run`, and the salt from `args/salt`.

- `/bin/caos` is setuid root, so it validates the id: a single path
  component, and `/cas/jobs/<id>` must exist and be owned by root.
- It must NOT be a path from the environment. See "Security" below on why
  `CAOS_CAS_DIR` is already a problem.
- Without `--job`, behaviour is unchanged, so single-job images are
  untouched.

A host that isolates its jobs maps each job's view onto its own directory, for
example a wasm guest's `/cas` onto `/cas/jobs/<id>`. The job cannot tell the
difference.

## The stream: `caos jobs --stream`

The worker opens it once, through the setuid `caos`, to a root-only unix
socket (as `caos next` does). One line per message, in each direction.

```text
worker → runner:  ready <n>          I can take n more jobs (credits)
runner → worker:  job <id>           a job is set up at /cas/jobs/<id>
worker → runner:  done <id>          post <id>'s result (what is at its out)
worker → runner:  error <id> <text>  fail <id> with this text
runner → worker:  cancel <id> <why>  stop <id> now; it has already been failed
runner → worker:  drain              take nothing new; finish what you have
worker → runner:  bye                (after drain) nothing in flight; I am exiting
```

**Credits are the point.** They separate "this job is finished" from "I want
more work", which `caos next` fuses:

- `done` says nothing about capacity. A host that wants to shrink stops
  sending `ready`.
- The runner keeps at most `credits` polls hanging, and never more in flight
  than the image's `CAOS_CONCURRENCY`.
- A host starts with `ready N` and typically sends `ready 1` after each
  `done`.

**The first job.** runnerd hands the container one job (`--job`), as today.
The runner sets it up as `/cas/jobs/<id>`, starts `/worker`, and sends
`job <id>` as soon as the stream opens. It counts against the first `ready`.

**Failure of the stream.** If the worker exits or closes the stream, every job
in flight is failed with the worker's stderr tail, and the runner exits (it
does not go back to warm polling: the image's worker is meant to be long-lived,
and a crash is a reason to start fresh).

## The runner

A third mode beside "one job" and "resident", chosen by
`CAOS_CONCURRENCY`. The existing two are untouched.

- **Polls.** One thread per hanging poll, each with required `{base}` and
  lineage `[{}]`, as `next_job` does today. A job that arrives is set up
  (namespace, secrets, nonce) and announced. A setup failure is posted as that
  job's failure, and the credit comes back.
- **Posting.** On `done <id>`: read `/cas/jobs/<id>/out` and `out-trace`, post,
  remove `/cas/jobs/<id>` and `/secret/<id>`. On `error`: post the error with
  that job's log tail. One thread owns all job state; the poll threads and the
  stream reader report to it over a channel, as `supervise` does today.
- **No per-job reset of shared things.** `reap_uid` and the `/tmp` wipe kill
  or remove what every job shares, so they run only when the worker exits.
  Isolating `/tmp` per job is the worker's job (the wasm host gives each guest
  `/tmp/<id>`).
- **Idle.** The runner exits when nothing is in flight and every hanging poll
  came back `idle`: it sends `drain`, waits for `bye` (or the grace period),
  and exits.
- **Eviction.** A poll answered `exit` means the parent (runnerd) needs this
  slot back, and runnerd only gets it when the container exits. So the runner
  stops polling, sends `drain`, lets in-flight jobs finish, sends `cancel` to
  any still running at `CAOS_RESIDENT_GRACE_SECS`-style grace
  (`CAOS_DRAIN_GRACE_SECS`), and exits. A running job cannot be evicted
  today either, so this is not a new kind of delay.
- **Job time limits.** Optional, per image (`CAOS_JOB_MAX_SECS`): the runner
  fails the job, then sends `cancel`. The server has no job deadline
  (`design/runner-protocol.md`, "No job deadline"), so this is the worker's own
  backstop, not the server's.

## Logs

The worker has one stdout and one stderr, shared by every job.

- The worker prefixes each line with `[<id>] `, and lines without a prefix
  belong to the process.
- The runner demultiplexes: it keeps a bounded tail per job (for that job's
  failure message) and one for the process (for failing everything if the
  worker dies), and relays every line to the container log.
- Masking covers the secrets of every job in flight, in every line, whichever
  job it claims to be from. A worker cannot be trusted to label lines
  honestly.

## Security

**The worker process sees the union of its jobs.** Every job's args, outs and
secrets are readable by the one worker uid, so job A's code could read job B's
secrets unless the worker isolates them.

- Multiplexing is therefore for workers whose jobs run as sandboxed guests,
  such as wasm in V8, where the host exposes only `/cas/jobs/<id>` and
  `/secret/<id>` to job `<id>`. It is not for running arbitrary scripts
  concurrently.
- The container and the privilege drop still apply underneath.
- The runner token stays in the runner. A V8 escape reaches what the worker
  uid can read, the same as any worker today, but not the token and not other
  containers' jobs.

**`CAOS_CAS_DIR` in the setuid binary.** `cas_dir()` (`lib.rs`) reads
`CAOS_CAS_DIR` from the caller's environment, and `/bin/caos` neither clears
nor checks it. That appears to let an unprivileged worker redirect root writes
and the job-context reads to any directory. It needs confirming and fixing on
its own (ignore the variable when real and effective uid differ), and it is
why `--job` takes a checked id rather than a path.

## Out of scope

- **Keyed jobs.** A multiplexed worker serves unkeyed jobs only. Each job
  could later carry its own tenure and lease (one `Heartbeat` per job).
- **Capacity weighting.** One runnerd slot now does up to N jobs' work. For
  now the operator sizes `CAOS_RUNNER_SLOTS` knowing which images multiplex.
  If that is not enough, runnerd could read an image's `CAOS_CONCURRENCY` and
  count it as more than one slot.
- **The runner's language.** The runner is Rust today, and the codebase is
  moving away from Rust. The stream protocol and the namespace layout are the
  contract; whatever replaces the runner implements them.

## Plan

1. `caos --job <id>` with its validation, and the `CAOS_CAS_DIR` fix. Unit
   tests for both.
2. The runner's multiplexed mode: polls, namespaces, the stream, demultiplexed
   logs, drain and eviction.
3. `tests/multiplexed`: a small Go worker on `std/go` with
   `CAOS_CONCURRENCY=4` that runs each job in a goroutine. It checks:
   - four jobs in flight at once (each waits on a barrier file the test
     releases)
   - results land on the right callers
   - one failing job does not fail the others, and its error carries its own
     log tail
   - a `cancel` on time limit
   - eviction mid-flight drains and exits
   - the worker dying fails every job in flight, uncached
4. The wasm host adopts it (`design/wasm-workers.md`, plan step 4).

## Open questions

1. **Credits on `done`?** `done <id> ready 1` in one line would halve the
   messages; separate lines are simpler to read. Start separate.
2. **Should the runner go back to warm polling** after the worker crashes,
   instead of exiting? Exiting is simpler and safer; measure restart cost
   first.
3. **Per-job `/tmp` by the runner** (`/tmp/jobs/<id>`, owned by the worker
   uid, removed on post) instead of leaving it to the worker. Cheap, and it
   makes cleanup exact.
