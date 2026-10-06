//! The runner protocol: pull-based dispatch (see `design/runner-protocol.md`).
//!
//! Anything that can run work long-polls `POST /runner/poll` with its
//! *required args* — name → oid pairs a job's ArgTree top level must match
//! exactly. The server never starts, stops, or counts workers: the set of
//! parked polls *is* the available capacity, and dispatch is matching pending
//! jobs against hanging polls. A poll is answered with a job, `idle` (its TTL
//! ran out — the runner's cue to exit or re-poll), or `exit` (eviction: a
//! pending job matches the runner's lineage but not the runner, so it should
//! die and let its parent poll). Results come back via `POST /runner/result`,
//! keyed by (req, nonce); the first post per nonce wins.
//!
//! [`dispatch`] is the compute pipeline's entry: it enqueues the job, waits on
//! a per-dispatch channel, and handles the two timeouts — a job no runner
//! claims fails 503 after [`pending_timeout`]. A claimed job has NO execution
//! deadline: it runs until its result arrives (a forced requeue would race a
//! fresh worker against the still-running one; dead-worker detection is
//! future work).
//!
//! A job carrying the reserved `affinity` entry is KEYED (design/daemons.md): the
//! first one claimed makes its runner the key's OWNER, and every other job for
//! the key queues for that owner and for nobody else, whether the owner is parked
//! or busy. The owner holds a LEASE that its runner renews from a thread of its
//! own. A lapse is how a dead owner is noticed: it fails the job in flight and
//! hands the queue back to the pool.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::HttpError;

/// A runner's required args / a job's ArgTree top level: name → git oid.
type ArgTree = BTreeMap<String, String>;

/// How long a job may sit unclaimed before the dispatch fails 503. New capacity
/// may register meanwhile (a kicked runner's parent, a fresh runnerd slot).
/// Default 60s; a deployment whose pool is deliberately small relative to its
/// job lengths (e.g. a few slots feeding one local LLM) overrides with
/// CAOS_PENDING_TIMEOUT_SECS so queued work waits patiently instead of 503ing.
fn pending_timeout() -> Duration {
    static SECS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    Duration::from_secs(*SECS.get_or_init(|| {
        std::env::var("CAOS_PENDING_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60)
    }))
}

/// How long a SEEDED sentinel waits before the server is willing to call a
/// same-sentinel disagreement permanent (see [`seeded_verdict`]).
///
/// It exists only to cover the two windows in which a parked seeder poll is
/// legitimately out of date: the core-seeder-runner rescans `refs/caos/seed`
/// every 5s (`RESCAN`), and a poll it parked before a republish keeps
/// advertising the OLD `required` until its 20s TTL turns over (`POLL_TTL`).
/// Inside those 25s a mismatch is transient and failing would be wrong;
/// outside them it never resolves on its own. Default 45s — most of a factor
/// of two over the turnover, and a twentieth of the 900s pending timeout a
/// stack sets. Raise it if you lengthen either seeder constant.
fn seeded_grace() -> Duration {
    static SECS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    Duration::from_secs(*SECS.get_or_init(|| {
        std::env::var("CAOS_SEEDED_GRACE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(45)
    }))
}

/// How long an owner's lease lasts after its runner last renewed it. The runner
/// renews every third of this, so three missed renewals are a lapse. Liveness
/// is the runner's own traffic and is never inferred from a job being slow.
fn lease_ttl() -> Duration {
    static SECS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    Duration::from_secs(*SECS.get_or_init(|| {
        std::env::var("CAOS_LEASE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15)
    }))
}

/// The lease a NEW owner starts with: long, because the claiming poll is
/// usually runnerd's and the container that will renew it has yet to be pulled
/// and started. A container that dies at start posts its own failure (runnerd's
/// backstop), so this only has to outlast a cold start.
fn lease_start_ttl() -> Duration {
    static SECS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    Duration::from_secs(*SECS.get_or_init(|| {
        std::env::var("CAOS_LEASE_START_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600)
    }))
}

/// A poll stops matching this close to its TTL, so a job isn't handed to a
/// connection the runner is about to abandon. Proportional for short polls
/// (a fifth of the TTL), capped at this for long ones.
const MAX_POLL_MARGIN: Duration = Duration::from_secs(1);

/// Bounds on a poll's TTL (a runner asking for more just re-polls; one asking
/// for less is effectively an immediate-or-nothing check).
const MIN_POLL_TTL: Duration = Duration::from_millis(10);
const MAX_POLL_TTL: Duration = Duration::from_secs(300);

/// After a requeue, how long the job matches only non-generic polls (unless the
/// requeue names its own `defer_generic_ms`) — so a provision-style runner
/// doesn't immediately re-claim the job it just requeued.
const DEFAULT_DEFER_GENERIC: Duration = Duration::from_secs(10);

/// Shared secret runners present as `Authorization: Bearer <token>`. Unset =
/// auth disabled (single-tenant dev stack).
const TOKEN_ENV: &str = "CAOS_RUNNER_TOKEN";

/// What a parked poll is answered with.
enum PollReply {
    /// A matching job: the payload JSON to hand the runner.
    Job(String),
    /// Eviction: exit so your parent resumes polling.
    Exit,
}

/// What a dispatch is answered with (over its per-dispatch channel).
enum Outcome {
    /// The worker's `"<type> <hash>"` (possibly a `promise` the caller resolves).
    Done(String),
    /// The runner reported failure.
    Failed(String),
}

/// Messages delivered to the compute thread that owns an in-flight job. A
/// sub-run request is handled there because that thread owns the unhashed run
/// context; the rendezvous table retains only this channel, never the context.
enum DispatchEvent {
    /// Something worth recording happened to the job. Reported to the compute
    /// thread rather than written here because both sites ([`claim`] and
    /// [`result`]) run under the rendezvous lock, and a trace write is a redis
    /// round trip — one lock, held by everything, is not the place for network
    /// I/O.
    Note(Note),
    Outcome(Outcome),
    SubRun {
        arg_tree: String,
        reply: mpsc::Sender<Result<(), (u16, String)>>,
    },
}

/// A git object hash, in the one form this server writes and accepts.
///
/// Checked where a runner's word becomes a record: the value ends up in a trace
/// entry and is handed back through `/status` as something to `caos get`, so a
/// malformed one would be a piece of junk that outlives the run that posted it.
fn is_object_hash(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A traceable fact about a job, on its way to the compute thread that owns it.
pub(crate) enum Note {
    /// A runner claimed the job and the work is now under way. A requeued job
    /// claims again and says so again: that is two real starts, and the record
    /// keeps the latest.
    Started,
    /// The worker left perf data at `/cas/out-trace`, by hash. Deliberately NOT
    /// part of the result: a result is content-addressed and keys everything
    /// downstream of it, so timings and cache statistics inside one would make
    /// every consumer miss whenever they changed. Out of band is the point.
    OutTrace(String),
}

/// A hanging `POST /runner/poll`, parked until matched, kicked, or expired.
struct ParkedPoll {
    /// Monotone arrival id — ties between equally specific polls go to the
    /// largest (LIFO: concentrate work on a hot runner, let the tail idle out).
    id: u64,
    required: ArgTree,
    /// The required sets of the runner's ancestors (outermost first). A pending
    /// job nothing matches kicks the deepest poll whose lineage could serve it.
    lineage: Vec<ArgTree>,
    /// Stops matching here — the TTL minus a margin, so a job isn't handed to
    /// a connection the runner is about to abandon.
    matchable_until: Instant,
    reply: mpsc::Sender<PollReply>,
    /// Set on the poll of a key's OWNER: the tenure it polls on behalf of. Such
    /// a poll is answered only from that owner's queue.
    tenure: Option<String>,
}

/// A dispatched job's lifecycle phase.
enum Phase {
    /// Waiting for a matching poll.
    Pending {
        deadline: Instant,
        /// While set (and in the future), only polls with ≥1 required key match.
        defer_generic_until: Option<Instant>,
    },
    /// A keyed job waiting in its owner's mailbox. No deadline: the pending
    /// timeout measures how long NO runner wanted a job, and this one has a
    /// runner that is alive and merely busy. If the owner goes away the job
    /// returns to `Pending` with a fresh deadline.
    Queued,
    /// Handed to a runner; runs until its result arrives. No execution
    /// deadline: a deadline + forced requeue races a fresh worker against the
    /// still-running one (nothing kills the old container), and duplicate
    /// 20-core bakes ground the machine — dead-worker detection is future
    /// work, likely leases.
    Inflight,
}

/// One dispatched job, from enqueue to result.
struct Job {
    arg_tree: String,
    /// Docker-pullable image reference (always sent; warm runners ignore it).
    image_ref: String,
    /// The ArgTree's top-level name → oid map, what `required` matches against.
    arg_entries: ArgTree,
    /// Secrets this job is entitled to (design/secrets.md): name → value pairs
    /// the runner drops at `/secret/<name>`. Ride out of band in the payload,
    /// never in the ArgTree — so out of the cache key. Recomputed per dispatch,
    /// so a warm runner's follow-up jobs each carry their own.
    secrets: Vec<(String, String)>,
    /// Current rendezvous nonce; refreshed on requeue (first post per nonce wins).
    nonce: String,
    phase: Phase,
    enqueued: Instant,
    events: mpsc::Sender<DispatchEvent>,
    /// `(base, affinity)` for a keyed job (see [`job_key`]).
    key: Option<String>,
    /// The tenure whose owner this job was handed to, once it has been.
    tenure: Option<String>,
}

/// The runner that owns one key: the only one that is ever handed a job for it.
struct Owner {
    key: String,
    /// The owner is gone if its lease is not renewed by then.
    lease_until: Instant,
    /// Keyed jobs waiting for the owner, in arrival order.
    queue: VecDeque<u64>,
    /// The job it is running, if any. A job and its owner are never both idle
    /// while the queue holds work.
    current: Option<u64>,
    /// The id of its parked poll, when it is between jobs.
    parked: Option<u64>,
}

/// The rendezvous state: parked polls and dispatched jobs, one lock.
#[derive(Default)]
struct State {
    parked: Vec<ParkedPoll>,
    /// Jobs by dispatch id (stable across requeues, unlike the nonce).
    jobs: HashMap<u64, Job>,
    /// Nonce → dispatch id, for result posts.
    by_nonce: HashMap<String, u64>,
    next_id: u64,
    /// Owners by TENURE: an unguessable id the server mints when it makes a
    /// runner the owner of a key, and that the runner presents to renew its
    /// lease and to poll. It names one ownership, not one job, so it survives
    /// from job to job while the job nonce does not.
    owners: HashMap<String, Owner>,
    /// Key → tenure. A key has at most one owner.
    by_key: HashMap<String, String>,
}

/// The key of a keyed job: its image and its `affinity`, by oid. `None` for
/// every job that names no instance.
fn job_key(arg_entries: &ArgTree) -> Option<String> {
    let affinity = arg_entries.get(caos_world::AFFINITY_ARG)?;
    let base = arg_entries.get("base").map(String::as_str).unwrap_or("");
    Some(format!("{base}/{affinity}"))
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|p| p.into_inner())
}

/// The configured runner token, if any.
fn token() -> Option<String> {
    std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty())
}

/// Require the shared bearer token when one is configured.
fn check_auth(authorization: Option<&str>) -> Result<(), HttpError> {
    let Some(expected) = token() else {
        return Ok(());
    };
    match authorization.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(got) if got == expected => Ok(()),
        _ => Err(HttpError::new(401, "missing or bad runner token")),
    }
}

/// A fresh nonce: unpredictable enough to be unguessable rendezvous state, and
/// unique across requeues and restarts.
fn new_nonce(id: u64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let seed = format!("{id}:{}:{}", std::process::id(), now.as_nanos());
    let digest = Sha256::digest(seed.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The git blob oid of a pool NAME — what a `required-pool` arg's entry is, and
/// so what the rendezvous compares. Memoized per name: the names are a fixed
/// tiny set and this is read under the dispatch path.
pub(crate) fn pool_oid(name: &str) -> String {
    static MEMO: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(known) = memo.lock().expect("pool oid memo").get(name) {
        return known.clone();
    }
    let oid = gix::objs::compute_hash(
        gix::hash::Kind::Sha1,
        gix::objs::Kind::Blob,
        name.as_bytes(),
    )
    .expect("hashing a pool name")
    .to_string();
    memo.lock()
        .expect("pool oid memo")
        .insert(name.to_string(), oid.clone());
    oid
}

/// Does `required` match a job with `arg_entries`? The rendezvous is SYMMETRIC,
/// and both halves are pure oid equality:
///
/// * every (name, oid) the RUNNER requires must equal the job's entry of that
///   name — the runner saying which jobs it will accept;
/// * every job entry named with [`caos_world::REQUIRED_ARG_PREFIX`] must equal
///   the runner's required entry of that name — the JOB saying which runners it
///   will accept.
///
/// The second half is what makes a dedicated pool possible. The generic pool
/// requires nothing, so the first half alone matches it against everything,
/// including work that wanted a specific pool; a `required*` arg excludes it,
/// because it advertises no entry of that name. Nothing here knows what any
/// particular pool is FOR — `required-pool=seeded` is just a name both sides
/// happen to agree on.
///
/// A job whose pool is absent therefore matches nothing and fails at its
/// pending deadline. That is the intended failure: leaking onto the general
/// pool is precisely what it asked not to do.
fn matches(required: &ArgTree, arg_entries: &ArgTree) -> bool {
    required
        .iter()
        .all(|(name, oid)| arg_entries.get(name) == Some(oid))
        && arg_entries
            .iter()
            .filter(|(name, _)| name.starts_with(caos_world::REQUIRED_ARG_PREFIX))
            .all(|(name, oid)| required.get(name) == Some(oid))
}

/// Why a seeded job is unanswerable, if that is PROVABLE right now.
///
/// A `docker://seeded…` job's `image` arg is the blob of the sentinel string
/// itself, and the seed record `build-builtins.sh` publishes for that sentinel
/// carries the same blob in its `required` — so a parked poll whose
/// `required["base"]` equals the job's `base` IS this sentinel's seeder, and
/// nobody else's. If that poll is parked and does not match the job, the two
/// sides disagree about the rest of the key and no amount of waiting fixes it:
/// the seeder answers one arg tree and the caller formed another.
///
/// That is the whole failure this exists for. It is not hypothetical — merging
/// main changed the seeded-core contract (`strip_caos_expr`) while two
/// hand-rolled `run-then` call sites still passed the directory whole, so
/// build.sh's `docker://seeded` job asked for `in=1fa8ec14` against a seeder
/// offering `in=229fc9ba`. The symptom was a TEN MINUTE hang on an idle
/// machine and then `no runner for arg_tree …`, which points at capacity —
/// the one thing that was not wrong. The information to say so exactly was in
/// this table the entire time.
///
/// `None` means "not provable": no seeder for this sentinel is parked (it may
/// not have registered yet, or its poll is between TTLs), so keep waiting.
fn seeded_verdict(st: &State, id: u64) -> Option<String> {
    let now = Instant::now();
    let entries = &st.jobs.get(&id)?.arg_entries;
    let live = st
        .parked
        .iter()
        .filter(|p| now < p.matchable_until)
        .map(|p| &p.required);
    disagreeing_seeder(entries, live)
}

/// [`seeded_verdict`] over plain values: the job's arg entries, and the
/// required set of every currently-matchable poll. The closest disagreement
/// wins, so the message names the one that shares the most of the key.
fn disagreeing_seeder<'a>(
    entries: &ArgTree,
    polls: impl Iterator<Item = &'a ArgTree>,
) -> Option<String> {
    let image = entries.get("base")?;
    let mut closest: Option<Vec<String>> = None;
    for required in polls {
        if required.get("base") != Some(image) {
            continue;
        }
        // A matching poll would have been claimed by `offer_job`; if one is
        // somehow here the job is about to run, so diagnose nothing.
        if matches(required, entries) {
            return None;
        }
        let diffs: Vec<String> = required
            .iter()
            .filter(|(name, oid)| entries.get(*name) != Some(*oid))
            .map(|(name, oid)| match entries.get(name) {
                Some(got) => format!("{name}: seeder answers {oid}, the job asks {got}"),
                None => format!("{name}: seeder answers {oid}, the job has no such arg"),
            })
            .collect();
        if closest.as_ref().is_none_or(|best| diffs.len() < best.len()) {
            closest = Some(diffs);
        }
    }
    closest.map(|diffs| {
        format!(
            "its seeder IS registered and requires a different arg tree ({}). \
             The seed record and the caller disagree about the key: one of them \
             is stale, so republish the seed (caosd up) or fix the caller.",
            diffs.join("; ")
        )
    })
}

/// The job payload a matched poll is answered with.
fn payload(job: &Job) -> String {
    let mut body = serde_json::json!({
        // `req` is the wire field name; its value is the ArgTree hash.
        "req": job.arg_tree,
        "nonce": job.nonce,
        "image_ref": job.image_ref,
        // No execution deadline (see Phase::Inflight); 0 kept for payload
        // shape compatibility.
        "deadline_ms": 0,
    });
    if let Some(token) = token() {
        body["token"] = serde_json::Value::String(token);
    }
    if let Some(tenure) = &job.tenure {
        // Present on a keyed job only. A runner that sees one renews the lease
        // on it for as long as it holds it, and a resident worker's `caos next`
        // polls on it.
        body["tenure"] = serde_json::Value::String(tenure.clone());
    }
    if !job.secrets.is_empty() {
        // Out-of-band injection channel: the values reach only this worker, for
        // this job, and are never part of the ArgTree/cache key.
        body["secrets"] = serde_json::Value::Object(
            job.secrets
                .iter()
                .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone())))
                .collect(),
        );
    }
    body.to_string()
}

/// Run ArgTree `arg_tree` (its top level `arg_entries`, resolved image
/// `image_ref`) through the runner rendezvous, blocking until a runner posts its
/// result.
pub(crate) fn dispatch(
    arg_tree: &str,
    arg_entries: ArgTree,
    image_ref: &str,
    seeded: bool,
    secrets: Vec<(String, String)>,
    mut start_sub_run: impl FnMut(&str) -> Result<(), HttpError>,
    mut on_note: impl FnMut(Note),
) -> Result<String, HttpError> {
    let (event_tx, event_rx) = mpsc::channel();
    let id = {
        let mut st = lock();
        let id = st.next_id;
        st.next_id += 1;
        let nonce = new_nonce(id);
        st.by_nonce.insert(nonce.clone(), id);
        let deadline = Instant::now() + pending_timeout();
        // A SEEDED SENTINEL IS FOR A SEEDER, AND FOR NOBODY ELSE. `docker://seeded…`
        // names no registry image; it is a key a core-seeder-runner answers with a
        // pre-built result (design/caos-expr.md, Phase 3). A generic runner that
        // claims one cannot do anything but `docker run seeded-deep-deps` and die
        // — and `offer_job` prefers the most specific poll, so the ONLY way that
        // happens is a seeder that has not parked its polls yet. That window is
        // real: a stack whose seed ref is published after boot answers nothing
        // until the seeder's next rescan, and the caller sees a docker error
        // pointing nowhere near the cause (observed, from caos-tools/build/worker.sh
        // publishing std and resolving it moments later).
        //
        // Deferring generic polls for the WHOLE pending window makes the sentinel
        // contract what it always claimed to be: it waits for an answerer, and if
        // none ever comes it fails loudly on the pending timeout.
        let defer_generic_until = if seeded { Some(deadline) } else { None };
        let key = job_key(&arg_entries);
        st.jobs.insert(
            id,
            Job {
                arg_tree: arg_tree.to_string(),
                image_ref: image_ref.to_string(),
                arg_entries,
                secrets,
                nonce,
                phase: Phase::Pending {
                    deadline,
                    defer_generic_until,
                },
                enqueued: Instant::now(),
                events: event_tx,
                key,
                tenure: None,
            },
        );
        offer_job(&mut st, id);
        id
    };

    // A seeded sentinel defers generic runners for its whole pending window, so
    // when it goes unanswered NOTHING happens — no container starts, no log
    // line appears, and the machine sits idle for 900s before a 503 that blames
    // capacity. Wake once at the grace point to say what is actually true.
    let mut seeded_check = if seeded {
        Some(Instant::now() + seeded_grace())
    } else {
        None
    };

    loop {
        // Sleep until the job's current phase deadline (the result sender wakes
        // us early through the channel), or the seeded grace point if sooner.
        let wait = {
            let st = lock();
            match st.jobs.get(&id).map(|j| &j.phase) {
                Some(Phase::Pending { deadline, .. }) => {
                    deadline.saturating_duration_since(Instant::now())
                }
                // Claimed: no deadline — wait on the channel in long chunks.
                // A claimed job runs until its result arrives, however long
                // (an execution deadline + requeue spawns a RACER against the
                // still-running worker — duplicate 20-core bakes ground this
                // machine to a halt; dead-worker detection is future work,
                // likely leases).
                Some(Phase::Inflight | Phase::Queued) => Duration::from_secs(3600),
                // Job already resolved and removed: the outcome is in the channel.
                None => Duration::ZERO,
            }
        };
        let wait = match seeded_check {
            Some(at) => wait.min(at.saturating_duration_since(Instant::now())),
            None => wait,
        };
        match event_rx.recv_timeout(wait.max(Duration::from_millis(10))) {
            Ok(DispatchEvent::Note(note)) => on_note(note),
            Ok(DispatchEvent::Outcome(Outcome::Done(result))) => return Ok(result),
            Ok(DispatchEvent::Outcome(Outcome::Failed(message))) => {
                return Err(HttpError::new(500, message))
            }
            Ok(DispatchEvent::SubRun { arg_tree, reply }) => {
                let outcome = start_sub_run(&arg_tree)
                    .map_err(|error| (error.status(), error.message().to_string()));
                let _ = reply.send(outcome);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut st = lock();
                // A bool, not a borrow of the job: the grace block below needs
                // `st` mutably, and `arg_tree` is the dispatch parameter anyway
                // (a Job's copy of it never changes).
                let Some(pending) = st
                    .jobs
                    .get(&id)
                    .map(|j| matches!(j.phase, Phase::Pending { .. }))
                else {
                    // Resolved between the timeout and the lock; loop to drain
                    // the channel (the sender removed the job before sending).
                    continue;
                };
                let now = Instant::now();
                // The grace point: still pending, and long enough past enqueue
                // that a same-sentinel disagreement can no longer be a seeder
                // mid-turnover. Fires at most once.
                if pending && seeded_check.is_some_and(|at| now >= at) {
                    seeded_check = None;
                    match seeded_verdict(&st, id) {
                        Some(why) => {
                            remove_job(&mut st, id);
                            drop(st);
                            return Err(HttpError::new(
                                503,
                                // `docker://` back on the front: `image_ref` is
                                // post-resolution and the scheme is stripped by
                                // then, so the bare name reads as an image.
                                format!(
                                    "seeded sentinel docker://{image_ref} \
                                     (arg_tree {arg_tree}) cannot be answered: {why}"
                                ),
                            ));
                        }
                        // Not provable — no seeder for this sentinel is parked
                        // at all. Still worth SAYING so, because the alternative
                        // is an idle machine and no output until the timeout.
                        None => eprintln!(
                            "caos-server: docker://{image_ref} (arg_tree {arg_tree}) has waited \
                             {:?} with no seeder registered for it; waiting up to {:?}",
                            seeded_grace(),
                            pending_timeout()
                        ),
                    }
                    continue;
                }
                let Some(job) = st.jobs.get(&id) else {
                    continue;
                };
                match job.phase {
                    Phase::Pending { deadline, .. } if now >= deadline => {
                        // ONLY for a seeded job. A `required["base"]` match is
                        // evidence of a seeder only when the image is a
                        // sentinel; on an ordinary job it is just a warm runner
                        // holding the same image, and calling that "the seeder"
                        // would be a new wrong answer in place of the old one.
                        let detail = if !seeded {
                            String::new()
                        } else {
                            match seeded_verdict(&st, id) {
                                Some(why) => format!(", the docker://{image_ref} sentinel: {why}"),
                                // A seeded sentinel never reaches a generic
                                // runner, so "no runner" is a misleading way to
                                // say "nobody published a record under this key".
                                None => format!(
                                    ": no seeder ever registered the \
                                     docker://{image_ref} sentinel"
                                ),
                            }
                        };
                        let job = remove_job(&mut st, id);
                        drop(st);
                        return Err(HttpError::new(
                            503,
                            format!(
                                "no runner for arg_tree {} (waited {:?}){detail}",
                                job.arg_tree,
                                pending_timeout()
                            ),
                        ));
                    }
                    // Deadline moved (claimed or requeued meanwhile): re-wait.
                    _ => {}
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(HttpError::new(500, "runner rendezvous lost the job"));
            }
        }
    }
}

/// `POST /sub-run` — ask the compute thread that owns an in-flight job to
/// start one exact child request. The nonce is the entire authority: it is
/// unpredictable, scoped to one claimed job, and removed with that job.
pub(crate) fn sub_run(body: &str) -> Result<Vec<u8>, HttpError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| HttpError::new(400, format!("invalid sub-run json: {error}")))?;
    let arg_tree = value["req"].as_str().unwrap_or_default();
    let nonce = value["nonce"].as_str().unwrap_or_default();
    if !valid_hex(arg_tree, 40) {
        return Err(HttpError::new(
            400,
            "sub-run needs a lowercase request hash",
        ));
    }
    if !valid_hex(nonce, 32) {
        return Err(HttpError::new(400, "sub-run needs a lowercase job nonce"));
    }

    let events = {
        let st = lock();
        let Some(id) = st.by_nonce.get(nonce) else {
            return Err(HttpError::new(410, "unknown or consumed job nonce"));
        };
        let job = &st.jobs[id];
        if !matches!(job.phase, Phase::Inflight) {
            return Err(HttpError::new(409, "job is not in flight"));
        }
        job.events.clone()
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    events
        .send(DispatchEvent::SubRun {
            arg_tree: arg_tree.to_string(),
            reply: reply_tx,
        })
        .map_err(|_| HttpError::new(410, "job finished before sub-run admission"))?;
    match reply_rx.recv() {
        Ok(Ok(())) => Ok(b"{}".to_vec()),
        Ok(Err((status, message))) => Err(HttpError::new(status, message)),
        Err(_) => Err(HttpError::new(410, "job finished before sub-run admission")),
    }
}

/// `POST /trace/child` — record that the CALLING job dispatched `req` under
/// `name`, so a reader following the caller's record descends into it.
///
/// This exists for work a job starts on ANOTHER STACK. The dev stack a test run
/// brings up writes its trace records to the same redis (`stack/serve` points
/// it at the host's, and `caos:trace:<argtree>` carries no cache namespace), so
/// the records are already side by side — what was missing was the one edge
/// joining them. With it, `status` on the outer job renders the whole suite.
///
/// The nonce is the entire authority, exactly as for `/sub-run`: it names one
/// claimed job, it is unpredictable, and it dies with the job. A caller can
/// only ever add an edge under ITSELF.
pub(crate) fn trace_child(config: &crate::Config, body: &str) -> Result<Vec<u8>, HttpError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| HttpError::new(400, format!("invalid trace-child json: {error}")))?;
    let child = value["req"].as_str().unwrap_or_default();
    let nonce = value["nonce"].as_str().unwrap_or_default();
    let name = value["name"].as_str().unwrap_or_default();
    if !valid_hex(child, 40) {
        return Err(HttpError::new(
            400,
            "trace-child needs a lowercase request hash",
        ));
    }
    if !valid_hex(nonce, 32) {
        return Err(HttpError::new(
            400,
            "trace-child needs a lowercase job nonce",
        ));
    }
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_graphic()) {
        return Err(HttpError::new(
            400,
            "trace-child needs a short printable name",
        ));
    }

    let parent = {
        let st = lock();
        let Some(id) = st.by_nonce.get(nonce) else {
            return Err(HttpError::new(410, "unknown or consumed job nonce"));
        };
        let job = &st.jobs[id];
        if !matches!(job.phase, Phase::Inflight) {
            return Err(HttpError::new(409, "job is not in flight"));
        }
        job.arg_tree.clone()
    };
    crate::status::child(config, &parent, "stack", name, child);
    Ok(b"{}".to_vec())
}

fn valid_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Remove job `id` (and its nonce mapping), returning it.
fn remove_job(st: &mut State, id: u64) -> Job {
    let job = st.jobs.remove(&id).expect("job present under lock");
    st.by_nonce.remove(&job.nonce);
    job
}

/// Put job `id` back in Pending under a fresh nonce (result-deadline miss, or
/// an explicit requeue verb), then offer it to the parked polls again.
fn requeue(st: &mut State, id: u64, defer_generic: Option<Duration>) {
    let nonce = new_nonce(id);
    let old = {
        let job = st.jobs.get_mut(&id).expect("job present under lock");
        let old = std::mem::replace(&mut job.nonce, nonce.clone());
        job.phase = Phase::Pending {
            deadline: Instant::now() + pending_timeout(),
            defer_generic_until: defer_generic.map(|d| Instant::now() + d),
        };
        old
    };
    st.by_nonce.remove(&old);
    st.by_nonce.insert(nonce, id);
    offer_job(st, id);
}

/// Try to hand pending job `id` to a parked poll: the most specific match wins,
/// ties go LIFO. If nothing matches, kick the deepest parked poll whose lineage
/// could serve the job (its exit lets an ancestor poll — the anti-starvation
/// cascade).
fn offer_job(st: &mut State, id: u64) {
    // A keyed job whose key has an owner is the owner's, and goes to nobody
    // else: not to a warm runner, not to the generic pool, and not into the
    // eviction cascade below. Whether the owner is parked or busy is its own
    // business; the queue holds the job until it asks.
    if let Some(tenure) = st
        .jobs
        .get(&id)
        .and_then(|job| job.key.as_ref())
        .and_then(|key| st.by_key.get(key))
        .cloned()
    {
        enqueue_for_owner(st, id, &tenure);
        return;
    }
    let now = Instant::now();
    let (arg_entries, defer_generic) = {
        let job = &st.jobs[&id];
        let defer = match job.phase {
            Phase::Pending {
                defer_generic_until: Some(until),
                ..
            } => until > now,
            _ => false,
        };
        (job.arg_entries.clone(), defer)
    };
    let live = |p: &ParkedPoll| now < p.matchable_until;
    let best = st
        .parked
        .iter()
        .enumerate()
        .filter(|(_, p)| live(p) && matches(&p.required, &arg_entries))
        .filter(|(_, p)| !(defer_generic && p.required.is_empty()))
        .max_by_key(|(_, p)| (p.required.len(), p.id))
        .map(|(i, _)| i);
    if let Some(i) = best {
        let poll = st.parked.remove(i);
        claim(st, id, &poll.reply);
        return;
    }
    // No match: kick the deepest poll whose lineage covers the job. One kick
    // per offer — the freed parent's poll either matches or is kicked in turn.
    let kick = st
        .parked
        .iter()
        .enumerate()
        .filter(|(_, p)| live(p) && p.lineage.iter().any(|l| matches(l, &arg_entries)))
        .max_by_key(|(_, p)| (p.required.len(), p.id))
        .map(|(i, _)| i);
    if let Some(i) = kick {
        let poll = st.parked.remove(i);
        // An evicted owner is released. Its queue is empty: a job for its key
        // is handed to a parked owner at once, so a parked owner never has one.
        if let Some(tenure) = &poll.tenure {
            release_owner(st, tenure);
        }
        let _ = poll.reply.send(PollReply::Exit);
    }
}

/// Hand job `id` to a poll: mark it inflight and answer the poll. If the job is
/// keyed and its key has no owner yet, the poll's runner becomes the owner in
/// this same step, under the one lock that every offer takes — so two first
/// messages cannot both claim, and the second finds an owner and queues.
fn claim(st: &mut State, id: u64, reply: &mpsc::Sender<PollReply>) {
    let key = st.jobs.get(&id).and_then(|job| job.key.clone());
    if let Some(key) = key {
        if !st.by_key.contains_key(&key) {
            create_owner(st, id, key);
        }
    }
    let job = st.jobs.get_mut(&id).expect("job present under lock");
    job.phase = Phase::Inflight;
    let _ = job.events.send(DispatchEvent::Note(Note::Started));
    let body = payload(job);
    let _ = reply.send(PollReply::Job(body));
}

/// Make the runner that is taking job `id` the owner of `key`, and pull every
/// other pending job for the key into its queue. Jobs for the key that arrive
/// from now on queue through [`offer_job`].
fn create_owner(st: &mut State, id: u64, key: String) {
    let tenure = new_nonce(st.next_id);
    st.next_id += 1;
    let mut waiting: Vec<(Instant, u64)> = st
        .jobs
        .iter()
        .filter(|(&other, job)| {
            other != id
                && job.key.as_deref() == Some(key.as_str())
                && matches!(job.phase, Phase::Pending { .. })
        })
        .map(|(&other, job)| (job.enqueued, other))
        .collect();
    waiting.sort();
    for (_, other) in &waiting {
        st.jobs
            .get_mut(other)
            .expect("job present under lock")
            .phase = Phase::Queued;
    }
    st.owners.insert(
        tenure.clone(),
        Owner {
            key: key.clone(),
            lease_until: Instant::now() + lease_start_ttl(),
            queue: waiting.into_iter().map(|(_, other)| other).collect(),
            current: Some(id),
            parked: None,
        },
    );
    st.by_key.insert(key, tenure.clone());
    st.jobs.get_mut(&id).expect("job present under lock").tenure = Some(tenure);
    spawn_sweeper();
}

/// Put keyed job `id` in `tenure`'s mailbox, and hand it over now if the owner
/// is waiting for work.
fn enqueue_for_owner(st: &mut State, id: u64, tenure: &str) {
    st.jobs.get_mut(&id).expect("job present under lock").phase = Phase::Queued;
    st.owners
        .get_mut(tenure)
        .expect("owner present under lock")
        .queue
        .push_back(id);
    drain_owner(st, tenure);
}

/// If `tenure`'s owner is parked and has work waiting, hand it the head of its
/// queue. A parked poll whose TTL is about to run out is left alone: it answers
/// `idle` and the owner's next poll takes the job, so the job is never handed to
/// a connection the runner is abandoning.
fn drain_owner(st: &mut State, tenure: &str) {
    let now = Instant::now();
    let Some(owner) = st.owners.get(tenure) else {
        return;
    };
    let (Some(poll_id), true) = (owner.parked, !owner.queue.is_empty()) else {
        return;
    };
    let Some(position) = st
        .parked
        .iter()
        .position(|p| p.id == poll_id && now < p.matchable_until)
    else {
        return;
    };
    let poll = st.parked.remove(position);
    let owner = st.owners.get_mut(tenure).expect("owner present under lock");
    owner.parked = None;
    let id = owner.queue.pop_front().expect("queue checked non-empty");
    owner.current = Some(id);
    st.jobs.get_mut(&id).expect("job present under lock").tenure = Some(tenure.to_string());
    claim(st, id, &poll.reply);
}

/// The owner's job has finished. With `keep` the runner stays the owner and is
/// about to poll for its queue; otherwise it is done with the key.
fn settle_owner(st: &mut State, tenure: &str, keep: bool) {
    if keep {
        if let Some(owner) = st.owners.get_mut(tenure) {
            owner.current = None;
        }
    } else {
        release_owner(st, tenure);
    }
}

/// End an ownership without failing anything: the key is free, and every job
/// still queued for it goes back to the pending table (fresh deadline, unowned)
/// to be offered again. The job in flight, if there is one, is left to post its
/// own result.
fn release_owner(st: &mut State, tenure: &str) {
    let Some(owner) = st.owners.remove(tenure) else {
        return;
    };
    st.by_key.remove(&owner.key);
    if let Some(poll_id) = owner.parked {
        st.parked.retain(|p| p.id != poll_id);
    }
    // Offered in arrival order, so the first becomes the next owner and the
    // rest queue behind it.
    for id in owner.queue {
        if let Some(job) = st.jobs.get_mut(&id) {
            job.phase = Phase::Pending {
                deadline: Instant::now() + pending_timeout(),
                defer_generic_until: None,
            };
            job.tenure = None;
        }
        offer_job(st, id);
    }
}

/// An owner whose lease lapsed is presumed dead. Its job in flight FAILS — a
/// failure is never cached, so the caller can retry — and its queue returns to
/// the pool as for [`release_owner`].
fn lapse_owner(st: &mut State, tenure: &str) {
    let Some(current) = st.owners.get(tenure).and_then(|owner| owner.current) else {
        release_owner(st, tenure);
        return;
    };
    release_owner(st, tenure);
    if st.jobs.contains_key(&current) {
        let job = remove_job(st, current);
        let _ = job
            .events
            .send(DispatchEvent::Outcome(Outcome::Failed(format!(
                "the runner owning this job's instance stopped renewing its lease \
                 (job {}); the instance is released, so a retry starts a new one",
                job.arg_tree
            ))));
    }
}

/// Sweep lapsed leases. One thread per process, started with the first owner;
/// it only ever takes the rendezvous lock, so an owner's death is noticed within
/// a second of its lease running out.
fn spawn_sweeper() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            let mut st = lock();
            let now = Instant::now();
            let lapsed: Vec<String> = st
                .owners
                .iter()
                .filter(|(_, owner)| owner.lease_until < now)
                .map(|(tenure, _)| tenure.clone())
                .collect();
            for tenure in lapsed {
                eprintln!("caos-server: owner {tenure}'s lease lapsed; releasing its key");
                lapse_owner(&mut st, &tenure);
            }
        });
    });
}

/// `POST /runner/lease` — an owner's runner renewing its lease (or, with
/// `release`, giving the key up). 410 if the tenure is not live, which is how a
/// runner learns it was presumed dead and must stop.
pub(crate) fn lease(authorization: Option<&str>, body: &str) -> Result<Vec<u8>, HttpError> {
    check_auth(authorization)?;
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| HttpError::new(400, format!("invalid lease json: {e}")))?;
    let tenure = v["tenure"].as_str().unwrap_or_default();
    let mut st = lock();
    if v["release"].as_bool() == Some(true) {
        release_owner(&mut st, tenure);
        return Ok(b"{}".to_vec());
    }
    match st.owners.get_mut(tenure) {
        Some(owner) => {
            owner.lease_until = Instant::now() + lease_ttl();
            Ok(b"{}".to_vec())
        }
        None => Err(HttpError::new(410, "unknown or lapsed tenure")),
    }
}

/// `POST /runner/poll` — hang until a matching job, eviction, or TTL.
pub(crate) fn poll(authorization: Option<&str>, body: &str) -> Result<Vec<u8>, HttpError> {
    check_auth(authorization)?;
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| HttpError::new(400, format!("invalid poll json: {e}")))?;
    let required = arg_tree(&v["required"])?;
    let lineage = match &v["lineage"] {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(sets) => {
            sets.iter().map(arg_tree).collect::<Result<Vec<_>, _>>()?
        }
        _ => return Err(HttpError::new(400, "lineage must be an array")),
    };
    let ttl = Duration::from_millis(v["ttl_ms"].as_u64().unwrap_or(10_000))
        .clamp(MIN_POLL_TTL, MAX_POLL_TTL);
    // Short polls get a proportional margin; long ones cap out.
    let margin = (ttl / 5).min(MAX_POLL_MARGIN);

    let tenure = v["tenure"].as_str().map(str::to_string);

    let (reply_tx, reply_rx) = mpsc::channel();
    let poll_id = {
        let mut st = lock();
        if let Some(tenure) = &tenure {
            // An owner polls for ITS queue and nothing else: never the pending
            // table, which holds only jobs nobody owns.
            let id = st.next_id;
            st.next_id += 1;
            let poll = ParkedPoll {
                id,
                required,
                lineage,
                matchable_until: Instant::now() + ttl - margin,
                reply: reply_tx,
                tenure: Some(tenure.clone()),
            };
            if !owner_poll(&mut st, tenure, poll)? {
                // Answered from the queue at once.
                return match reply_rx.recv() {
                    Ok(PollReply::Job(payload)) => reply_job(&payload),
                    _ => Err(HttpError::new(500, "poll reply lost")),
                };
            }
            id
        } else {
            // A pending job may already be waiting for exactly this runner.
            if let Some(id) = best_pending(&st, &required) {
                claim(&mut st, id, &reply_tx);
                match reply_rx.recv() {
                    Ok(PollReply::Job(payload)) => return reply_job(&payload),
                    _ => return Err(HttpError::new(500, "poll reply lost")),
                }
            }
            let id = st.next_id;
            st.next_id += 1;
            st.parked.push(ParkedPoll {
                id,
                required,
                lineage,
                matchable_until: Instant::now() + ttl - margin,
                reply: reply_tx,
                tenure: None,
            });
            id
        }
    };

    match reply_rx.recv_timeout(ttl) {
        Ok(PollReply::Job(payload)) => reply_job(&payload),
        Ok(PollReply::Exit) => Ok(br#"{"exit":true}"#.to_vec()),
        Err(_) => {
            // TTL expired — but a matcher may have claimed us in the race window:
            // if we're no longer parked, a reply is (about to be) in the channel.
            let mut st = lock();
            if let Some(i) = st.parked.iter().position(|p| p.id == poll_id) {
                let poll = st.parked.remove(i);
                // An idle owner KEEPS its key: it polls again, and a job that
                // arrived in between waited in its queue. Only eviction,
                // a release or a lapse ends an ownership.
                if let Some(owner) = poll.tenure.as_ref().and_then(|t| st.owners.get_mut(t)) {
                    owner.parked = None;
                }
                Ok(br#"{"idle":true}"#.to_vec())
            } else {
                drop(st);
                match reply_rx.recv() {
                    Ok(PollReply::Job(payload)) => reply_job(&payload),
                    Ok(PollReply::Exit) => Ok(br#"{"exit":true}"#.to_vec()),
                    Err(_) => Err(HttpError::new(500, "poll reply lost")),
                }
            }
        }
    }
}

/// An owner's poll: answer it from the owner's queue now, or park it. Returns
/// whether it was parked (`false` means the reply is already in its channel).
///
/// Polling renews the lease, and a poll for a tenure that is not live is an
/// error rather than a wait: its owner lapsed or was evicted, the key may
/// already belong to someone else, and the runner must stop.
fn owner_poll(st: &mut State, tenure: &str, poll: ParkedPoll) -> Result<bool, HttpError> {
    let Some(owner) = st.owners.get_mut(tenure) else {
        return Err(HttpError::new(410, "unknown or lapsed tenure"));
    };
    if owner.current.is_some() {
        // A runner polls only after it has posted its result, and a result
        // that keeps the tenure clears `current`.
        return Err(HttpError::new(409, "tenure has a job in flight"));
    }
    owner.lease_until = Instant::now() + lease_ttl();
    if let Some(id) = owner.queue.pop_front() {
        owner.current = Some(id);
        st.jobs.get_mut(&id).expect("job present under lock").tenure = Some(tenure.to_string());
        claim(st, id, &poll.reply);
        return Ok(false);
    }
    owner.parked = Some(poll.id);
    st.parked.push(poll);
    Ok(true)
}

/// The oldest pending job this poll's required set matches (respecting a
/// requeue's defer-generic window), if any.
fn best_pending(st: &State, required: &ArgTree) -> Option<u64> {
    let now = Instant::now();
    st.jobs
        .iter()
        .filter(|(_, job)| match job.phase {
            Phase::Pending {
                defer_generic_until,
                ..
            } => !(required.is_empty() && defer_generic_until.is_some_and(|until| until > now)),
            Phase::Inflight | Phase::Queued => false,
        })
        .filter(|(_, job)| matches(required, &job.arg_entries))
        .min_by_key(|(_, job)| job.enqueued)
        .map(|(&id, _)| id)
}

/// Wrap a job payload as the poll response `{"job": {...}}`.
fn reply_job(payload: &str) -> Result<Vec<u8>, HttpError> {
    Ok(format!(r#"{{"job":{payload}}}"#).into_bytes())
}

/// Parse a JSON object of string → string into an [`ArgTree`].
fn arg_tree(v: &serde_json::Value) -> Result<ArgTree, HttpError> {
    match v {
        serde_json::Value::Null => Ok(ArgTree::new()),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_string()))
                    .ok_or_else(|| HttpError::new(400, format!("arg {k:?} is not a string")))
            })
            .collect(),
        _ => Err(HttpError::new(400, "required args must be an object")),
    }
}

/// `POST /runner/result` — a runner reporting on a job it was handed: a result,
/// a failure, or a requeue (it can't run the job; put it back for someone who
/// can). First post per nonce wins; a consumed or unknown nonce gets 410.
pub(crate) fn result(authorization: Option<&str>, body: &str) -> Result<Vec<u8>, HttpError> {
    check_auth(authorization)?;
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| HttpError::new(400, format!("invalid result json: {e}")))?;
    // `req` is the wire field name; its value is the ArgTree hash.
    let arg_tree = v["req"].as_str().unwrap_or_default();
    let nonce = v["nonce"].as_str().unwrap_or_default();
    if arg_tree.is_empty() || nonce.is_empty() {
        return Err(HttpError::new(400, "result missing req/nonce"));
    }

    let mut st = lock();
    let Some(&id) = st.by_nonce.get(nonce) else {
        return Err(HttpError::new(410, "unknown or consumed nonce"));
    };
    if st.jobs[&id].arg_tree != arg_tree {
        return Err(HttpError::new(410, "nonce does not belong to this req"));
    }

    if v["requeue"].as_bool() == Some(true) {
        let defer = Duration::from_millis(
            v["defer_generic_ms"]
                .as_u64()
                .unwrap_or(DEFAULT_DEFER_GENERIC.as_millis() as u64),
        );
        // A runner that cannot run a keyed job does not own its key either.
        if let Some(tenure) = st.jobs.get_mut(&id).and_then(|job| job.tenure.take()) {
            release_owner(&mut st, &tenure);
        }
        requeue(&mut st, id, Some(defer));
        return Ok(b"{}".to_vec());
    }

    let job = remove_job(&mut st, id);
    // Whether its runner stays the owner is the runner's word, not a guess: a
    // resident worker's `caos next` posts with `keep` and then polls for its
    // queue, and anything else is a runner that is done with the key, so the
    // key is released and whatever queued behind this job is offered afresh.
    if let Some(tenure) = &job.tenure {
        settle_owner(&mut st, tenure, v["keep"].as_bool() == Some(true));
    }
    drop(st);
    // Sent before the outcome, and on the failing path too: perf data from a
    // run that died is the case you most want it for. The outcome ends the
    // dispatch loop, so a note after it would never be read.
    if let Some(oid) = v["out_trace"].as_str().filter(|oid| is_object_hash(oid)) {
        let _ = job
            .events
            .send(DispatchEvent::Note(Note::OutTrace(oid.to_string())));
    }
    let outcome = if v["ok"].as_bool() == Some(true) {
        match v["result"].as_str() {
            Some(result) if !result.trim().is_empty() => Outcome::Done(result.trim().to_string()),
            _ => Outcome::Failed("runner posted ok without a result".to_string()),
        }
    } else {
        let error = v["error"].as_str().unwrap_or("unspecified failure");
        let log = v["log"].as_str().unwrap_or_default();
        let message = if log.is_empty() {
            format!("worker failed: {error}")
        } else {
            format!("worker failed: {error}\n{log}")
        };
        Outcome::Failed(message)
    };
    let _ = job.events.send(DispatchEvent::Outcome(outcome));
    Ok(b"{}".to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> ArgTree {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The merge failure, in miniature: the seeder for `docker://seeded` (image
    /// blob `sent`) answers `in=aaa`, and the caller formed `in=bbb`.
    #[test]
    fn a_parked_seeder_that_disagrees_is_a_verdict() {
        let job = args(&[("base", "sent"), ("in", "bbb"), ("std", "s")]);
        let seeder = args(&[("base", "sent"), ("in", "aaa")]);
        let why = disagreeing_seeder(&job, [&seeder].into_iter()).expect("a verdict");
        assert!(
            why.contains("in: seeder answers aaa, the job asks bbb"),
            "{why}"
        );
    }

    /// No seeder for THIS sentinel is parked — another sentinel's seeder and a
    /// generic runnerd poll are not evidence about this key, so keep waiting.
    #[test]
    fn only_the_same_sentinel_counts() {
        let job = args(&[("base", "sent"), ("in", "bbb")]);
        let other = args(&[("base", "other-sent"), ("in", "aaa")]);
        let generic = args(&[]);
        assert!(disagreeing_seeder(&job, [&other, &generic].into_iter()).is_none());
        // …and with nothing parked at all.
        assert!(disagreeing_seeder(&job, [].into_iter()).is_none());
    }

    /// `matches` is a SUBSET match, so a seeder pinning only what it cares
    /// about (no `std`, no `salt` — build-builtins.sh omits them) matches, and
    /// a matching poll is never a verdict.
    #[test]
    fn a_matching_seeder_is_not_a_verdict() {
        let job = args(&[("base", "sent"), ("in", "aaa"), ("std", "s"), ("salt", "x")]);
        let seeder = args(&[("base", "sent"), ("in", "aaa")]);
        assert!(disagreeing_seeder(&job, [&seeder].into_iter()).is_none());
    }

    /// Two seeders disagree; the message names the one sharing more of the key.
    #[test]
    fn the_closest_disagreement_is_reported() {
        let job = args(&[("base", "sent"), ("in", "bbb"), ("worker1", "w")]);
        let far = args(&[("base", "sent"), ("in", "aaa"), ("worker1", "zzz")]);
        let near = args(&[("base", "sent"), ("in", "aaa"), ("worker1", "w")]);
        let why = disagreeing_seeder(&job, [&far, &near].into_iter()).expect("a verdict");
        assert!(!why.contains("worker1"), "{why}");
    }

    /// A job that names a pool is invisible to the generic runner, which is the
    /// whole mechanism: without it, capping the general pool deadlocks the
    /// moment every slot holds a test waiting on a child.
    #[test]
    fn a_pool_arg_excludes_the_generic_runner_and_admits_only_that_pool() {
        let oid = |s: &str| s.repeat(40 / s.len());
        let generic = ArgTree::new();
        let test_pool = ArgTree::from([("required-pool".to_string(), oid("a"))]);

        let ordinary = ArgTree::from([("base".to_string(), oid("b"))]);
        let pooled = ArgTree::from([
            ("base".to_string(), oid("b")),
            ("required-pool".to_string(), oid("a")),
        ]);

        // The generic runner takes ordinary work and nothing that named a pool.
        assert!(matches(&generic, &ordinary));
        assert!(!matches(&generic, &pooled));
        // The test pool takes its own work and nothing else — so it never
        // steals capacity from the general pool either.
        assert!(matches(&test_pool, &pooled));
        assert!(!matches(&test_pool, &ordinary));
        // A pool arg whose VALUE differs is a different pool, not a wildcard.
        let other = ArgTree::from([
            ("base".to_string(), oid("b")),
            ("required-pool".to_string(), oid("c")),
        ]);
        assert!(!matches(&test_pool, &other));
    }

    // ---- keyed dispatch (design/daemons.md) ----

    fn keyed(instance: &str) -> ArgTree {
        args(&[("base", "image"), ("affinity", instance)])
    }

    /// Dispatch-side: a pending keyed job, offered. Returns its id and the
    /// receiver its outcome would arrive on.
    fn submit(st: &mut State, entries: ArgTree, tag: &str) -> (u64, mpsc::Receiver<DispatchEvent>) {
        let (events, rx) = mpsc::channel();
        let id = st.next_id;
        st.next_id += 1;
        st.jobs.insert(
            id,
            Job {
                arg_tree: tag.to_string(),
                image_ref: "image".to_string(),
                key: job_key(&entries),
                arg_entries: entries,
                secrets: Vec::new(),
                nonce: format!("nonce-{id}"),
                phase: Phase::Pending {
                    deadline: Instant::now() + Duration::from_secs(60),
                    defer_generic_until: None,
                },
                enqueued: Instant::now(),
                events,
                tenure: None,
            },
        );
        offer_job(st, id);
        (id, rx)
    }

    /// A parked generic poll, the way runnerd's is.
    fn park_generic(st: &mut State) -> mpsc::Receiver<PollReply> {
        let (reply, rx) = mpsc::channel();
        let id = st.next_id;
        st.next_id += 1;
        st.parked.push(ParkedPoll {
            id,
            required: ArgTree::new(),
            lineage: Vec::new(),
            matchable_until: Instant::now() + Duration::from_secs(60),
            reply,
            tenure: None,
        });
        rx
    }

    /// An owner's poll, the way `caos next` makes it. `true` if it parked.
    fn owner_polls(st: &mut State, tenure: &str) -> (bool, mpsc::Receiver<PollReply>) {
        let (reply, rx) = mpsc::channel();
        let id = st.next_id;
        st.next_id += 1;
        let poll = ParkedPoll {
            id,
            required: keyed("a"),
            lineage: vec![ArgTree::new()],
            matchable_until: Instant::now() + Duration::from_secs(60),
            reply,
            tenure: Some(tenure.to_string()),
        };
        let Ok(parked) = owner_poll(st, tenure, poll) else {
            panic!("the tenure is not live");
        };
        (parked, rx)
    }

    fn answered_job(rx: &mpsc::Receiver<PollReply>) -> serde_json::Value {
        match rx.try_recv() {
            Ok(PollReply::Job(payload)) => serde_json::from_str(&payload).expect("payload json"),
            _ => panic!("the poll was not answered with a job"),
        }
    }

    fn tenure_of(st: &State, key: &str) -> String {
        st.by_key.get(key).cloned().expect("the key has an owner")
    }

    /// Two first messages for one instance, and two generic runners waiting:
    /// exactly one runner claims, and the second message queues instead of
    /// starting a second container.
    #[test]
    fn concurrent_first_messages_claim_once() {
        let mut st = State::default();
        let first = park_generic(&mut st);
        let second = park_generic(&mut st);
        let (j1, _r1) = submit(&mut st, keyed("a"), "one");
        let (j2, _r2) = submit(&mut st, keyed("a"), "two");
        let answered: Vec<_> = [&first, &second]
            .iter()
            .filter_map(|rx| rx.try_recv().ok())
            .collect();
        assert_eq!(answered.len(), 1, "exactly one runner was handed a job");
        assert!(matches!(st.jobs[&j1].phase, Phase::Inflight));
        assert!(matches!(st.jobs[&j2].phase, Phase::Queued));
        assert_eq!(st.owners.len(), 1);
    }

    /// While the owner is busy, a message for its key waits in the owner's queue
    /// and a generic runner that is parked and idle does NOT get it.
    #[test]
    fn a_busy_owner_queues_rather_than_spills() {
        let mut st = State::default();
        let owner = park_generic(&mut st);
        let (j1, _r1) = submit(&mut st, keyed("a"), "one");
        answered_job(&owner);
        let spare = park_generic(&mut st);
        let (j2, _r2) = submit(&mut st, keyed("a"), "two");
        assert!(matches!(st.jobs[&j2].phase, Phase::Queued));
        assert!(
            spare.try_recv().is_err(),
            "the spare runner was offered a keyed job"
        );
        assert_eq!(
            st.owners[&tenure_of(&st, &job_key(&keyed("a")).unwrap())].queue,
            [j2]
        );
        assert!(matches!(st.jobs[&j1].phase, Phase::Inflight));
    }

    /// The owner is handed its queue one job at a time, in arrival order, and
    /// is never answered idle while work waits.
    #[test]
    fn the_queue_is_served_in_arrival_order() {
        let mut st = State::default();
        let first = park_generic(&mut st);
        let (j1, _r1) = submit(&mut st, keyed("a"), "one");
        let tenure = answered_job(&first)["tenure"].as_str().unwrap().to_string();
        let (_j2, _r2) = submit(&mut st, keyed("a"), "two");
        let (_j3, _r3) = submit(&mut st, keyed("a"), "three");

        // `caos next`: post the result and keep the tenure, then poll.
        st.jobs.remove(&j1);
        settle_owner(&mut st, &tenure, true);
        let (parked, rx) = owner_polls(&mut st, &tenure);
        assert!(!parked, "work was waiting, so the poll is answered at once");
        let two = answered_job(&rx);
        assert_eq!(two["req"], "two");
        assert_eq!(
            two["tenure"],
            tenure.as_str(),
            "the same ownership, a new job"
        );

        let (j2, _) = st
            .jobs
            .iter()
            .find(|(_, j)| j.arg_tree == "two")
            .map(|(i, j)| (*i, j.nonce.clone()))
            .unwrap();
        st.jobs.remove(&j2);
        settle_owner(&mut st, &tenure, true);
        let (_, rx) = owner_polls(&mut st, &tenure);
        assert_eq!(answered_job(&rx)["req"], "three");
    }

    /// A parked owner is handed a message the moment it arrives.
    #[test]
    fn a_parked_owner_gets_a_new_message_at_once() {
        let mut st = State::default();
        let first = park_generic(&mut st);
        let (j1, _r1) = submit(&mut st, keyed("a"), "one");
        let tenure = answered_job(&first)["tenure"].as_str().unwrap().to_string();
        st.jobs.remove(&j1);
        settle_owner(&mut st, &tenure, true);
        let (parked, rx) = owner_polls(&mut st, &tenure);
        assert!(parked);
        let (_j2, _r2) = submit(&mut st, keyed("a"), "two");
        assert_eq!(answered_job(&rx)["req"], "two");
    }

    /// A result that does not keep the tenure ends the ownership, and what
    /// queued behind it is offered to the pool again, unowned.
    #[test]
    fn a_result_without_keep_releases_the_key() {
        let mut st = State::default();
        let first = park_generic(&mut st);
        let (j1, _r1) = submit(&mut st, keyed("a"), "one");
        let tenure = answered_job(&first)["tenure"].as_str().unwrap().to_string();
        let (j2, _r2) = submit(&mut st, keyed("a"), "two");
        st.jobs.remove(&j1);
        settle_owner(&mut st, &tenure, false);
        assert!(st.owners.is_empty() && st.by_key.is_empty());
        assert!(
            matches!(st.jobs[&j2].phase, Phase::Pending { .. }),
            "unowned again"
        );
        // The next generic runner to arrive takes it and owns the key afresh.
        let later = park_generic(&mut st);
        offer_job(&mut st, j2);
        assert_eq!(answered_job(&later)["req"], "two");
        assert_eq!(st.owners.len(), 1);
    }

    /// A lapsed lease fails the job in flight (uncacheably, as an outcome the
    /// caller sees) and re-dispatches the queue.
    #[test]
    fn a_lapse_fails_the_job_in_flight_and_frees_the_queue() {
        let mut st = State::default();
        let first = park_generic(&mut st);
        let (_j1, r1) = submit(&mut st, keyed("a"), "one");
        let tenure = answered_job(&first)["tenure"].as_str().unwrap().to_string();
        let (j2, _r2) = submit(&mut st, keyed("a"), "two");
        lapse_owner(&mut st, &tenure);
        let outcome = std::iter::from_fn(|| r1.try_recv().ok())
            .find_map(|event| match event {
                DispatchEvent::Outcome(Outcome::Failed(message)) => Some(message),
                _ => None,
            })
            .expect("the in-flight job failed");
        assert!(outcome.contains("lease"), "{outcome}");
        assert!(st.owners.is_empty() && st.by_key.is_empty());
        assert!(matches!(st.jobs[&j2].phase, Phase::Pending { .. }));
        // The owner's own late poll is refused rather than parked.
        let (reply, _rx) = mpsc::channel();
        let poll = ParkedPoll {
            id: 99,
            required: keyed("a"),
            lineage: Vec::new(),
            matchable_until: Instant::now() + Duration::from_secs(60),
            reply,
            tenure: Some(tenure.clone()),
        };
        assert!(owner_poll(&mut st, &tenure, poll).is_err());
    }

    /// Eviction ends a parked owner's ownership, and it is only ever chosen
    /// while its queue is empty. A job nothing can serve kicks it, as it would
    /// any warm runner.
    #[test]
    fn eviction_releases_a_parked_owner() {
        let mut st = State::default();
        let first = park_generic(&mut st);
        let (j1, _r1) = submit(&mut st, keyed("a"), "one");
        let tenure = answered_job(&first)["tenure"].as_str().unwrap().to_string();
        st.jobs.remove(&j1);
        settle_owner(&mut st, &tenure, true);
        let (parked, rx) = owner_polls(&mut st, &tenure);
        assert!(parked);
        // A job for another image: no poll matches, the owner's lineage does.
        let (_other, _r) = submit(&mut st, args(&[("base", "other")]), "other");
        assert!(matches!(rx.try_recv(), Ok(PollReply::Exit)));
        assert!(st.owners.is_empty() && st.by_key.is_empty());
    }

    /// Two instances are two keys with two owners, never one's queue for the
    /// other's.
    #[test]
    fn keys_are_independent() {
        let mut st = State::default();
        let first = park_generic(&mut st);
        let second = park_generic(&mut st);
        let (_j1, _r1) = submit(&mut st, keyed("a"), "a1");
        let (_j2, _r2) = submit(&mut st, keyed("b"), "b1");
        let a = answered_job(&second);
        let b = answered_job(&first);
        assert_ne!(a["tenure"], b["tenure"]);
        assert_eq!(st.owners.len(), 2);
    }
}
