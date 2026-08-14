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

use std::collections::{BTreeMap, HashMap};
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
}

/// A dispatched job's lifecycle phase.
enum Phase {
    /// Waiting for a matching poll.
    Pending {
        deadline: Instant,
        /// While set (and in the future), only polls with ≥1 required key match.
        defer_generic_until: Option<Instant>,
    },
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
    outcome: mpsc::Sender<Outcome>,
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

/// Does `required` match a job with `arg_entries`? Every required (name, oid)
/// must equal the job's entry of that name — pure oid equality.
fn matches(required: &ArgTree, arg_entries: &ArgTree) -> bool {
    required
        .iter()
        .all(|(name, oid)| arg_entries.get(name) == Some(oid))
}

/// Why a seeded job is unanswerable, if that is PROVABLE right now.
///
/// A `docker://seeded…` job's `image` arg is the blob of the sentinel string
/// itself, and the seed record `build-builtins.sh` publishes for that sentinel
/// carries the same blob in its `required` — so a parked poll whose
/// `required["image"]` equals the job's `image` IS this sentinel's seeder, and
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
) -> Result<String, HttpError> {
    let (outcome_tx, outcome_rx) = mpsc::channel();
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
        // pointing nowhere near the cause (observed, from caos-tools/build.sh
        // publishing std and resolving it moments later).
        //
        // Deferring generic polls for the WHOLE pending window makes the sentinel
        // contract what it always claimed to be: it waits for an answerer, and if
        // none ever comes it fails loudly on the pending timeout.
        let defer_generic_until = if seeded { Some(deadline) } else { None };
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
                outcome: outcome_tx,
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
                Some(Phase::Inflight) => Duration::from_secs(3600),
                // Job already resolved and removed: the outcome is in the channel.
                None => Duration::ZERO,
            }
        };
        let wait = match seeded_check {
            Some(at) => wait.min(at.saturating_duration_since(Instant::now())),
            None => wait,
        };
        match outcome_rx.recv_timeout(wait.max(Duration::from_millis(10))) {
            Ok(Outcome::Done(result)) => return Ok(result),
            Ok(Outcome::Failed(message)) => return Err(HttpError::new(500, message)),
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
                        // ONLY for a seeded job. A `required["image"]` match is
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
        let _ = poll.reply.send(PollReply::Exit);
    }
}

/// Hand job `id` to a poll: mark it inflight and answer the poll.
fn claim(st: &mut State, id: u64, reply: &mpsc::Sender<PollReply>) {
    let job = st.jobs.get_mut(&id).expect("job present under lock");
    job.phase = Phase::Inflight;
    let body = payload(job);
    let _ = reply.send(PollReply::Job(body));
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

    let (reply_tx, reply_rx) = mpsc::channel();
    let poll_id = {
        let mut st = lock();
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
        });
        id
    };

    match reply_rx.recv_timeout(ttl) {
        Ok(PollReply::Job(payload)) => reply_job(&payload),
        Ok(PollReply::Exit) => Ok(br#"{"exit":true}"#.to_vec()),
        Err(_) => {
            // TTL expired — but a matcher may have claimed us in the race window:
            // if we're no longer parked, a reply is (about to be) in the channel.
            let mut st = lock();
            if let Some(i) = st.parked.iter().position(|p| p.id == poll_id) {
                st.parked.remove(i);
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
            Phase::Inflight => false,
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
        requeue(&mut st, id, Some(defer));
        return Ok(b"{}".to_vec());
    }

    let job = remove_job(&mut st, id);
    drop(st);
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
    let _ = job.outcome.send(outcome);
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
        let job = args(&[
            ("base", "sent"),
            ("in", "aaa"),
            ("std", "s"),
            ("salt", "x"),
        ]);
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
}
