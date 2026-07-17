//! The runner protocol: pull-based dispatch (see `design/runner-protocol.md`).
//!
//! Anything that can run work long-polls `POST /runner/poll` with its
//! *required args* — name → oid pairs a job's args-tree top level must match
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
//! claims fails 503 after [`PENDING_TIMEOUT`]; a claimed job whose result
//! never arrives is requeued under a fresh nonce after [`JOB_DEADLINE`]
//! (results are memoized and workers deterministic, so a spurious re-run is
//! wasted work, never a wrong answer).

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::HttpError;

/// A runner's required args / a job's args-tree top level: name → git oid.
type ArgSet = BTreeMap<String, String>;

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

/// How long a claimed job may run before it's requeued under a fresh nonce.
/// Generous: long compiles are real, and determinism makes a spurious re-run
/// wasteful rather than wrong.
const JOB_DEADLINE: Duration = Duration::from_secs(600);

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
    /// The worker's result.
    Done(DispatchResult),
    /// The runner reported failure.
    Failed(String),
}

pub(crate) struct DispatchResult {
    pub(crate) result: String,
}

/// A hanging `POST /runner/poll`, parked until matched, kicked, or expired.
struct ParkedPoll {
    /// Monotone arrival id — ties between equally specific polls go to the
    /// largest (LIFO: concentrate work on a hot runner, let the tail idle out).
    id: u64,
    required: ArgSet,
    /// The required sets of the runner's ancestors (outermost first). A pending
    /// job nothing matches kicks the deepest poll whose lineage could serve it.
    lineage: Vec<ArgSet>,
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
    /// Handed to a runner; a result must arrive by `deadline` or it's requeued.
    Inflight { deadline: Instant },
}

/// One dispatched job, from enqueue to result.
struct Job {
    req: String,
    /// Docker-pullable image reference (always sent; warm runners ignore it).
    image_ref: String,
    /// The args tree's top-level name → oid map, what `required` matches against.
    arg_entries: ArgSet,
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
fn matches(required: &ArgSet, arg_entries: &ArgSet) -> bool {
    required
        .iter()
        .all(|(name, oid)| arg_entries.get(name) == Some(oid))
}

/// The job payload a matched poll is answered with.
fn payload(job: &Job) -> String {
    let mut body = serde_json::json!({
        "req": job.req,
        "nonce": job.nonce,
        "image_ref": job.image_ref,
        "deadline_ms": JOB_DEADLINE.as_millis() as u64,
    });
    if let Some(token) = token() {
        body["token"] = serde_json::Value::String(token);
    }
    body.to_string()
}

/// Run `req` (args-tree top level `arg_entries`, resolved image `image_ref`)
/// through the runner rendezvous, blocking until a runner posts its result.
pub(crate) fn dispatch(
    req: &str,
    arg_entries: ArgSet,
    image_ref: &str,
) -> Result<DispatchResult, HttpError> {
    let (outcome_tx, outcome_rx) = mpsc::channel();
    let id = {
        let mut st = lock();
        let id = st.next_id;
        st.next_id += 1;
        let nonce = new_nonce(id);
        st.by_nonce.insert(nonce.clone(), id);
        st.jobs.insert(
            id,
            Job {
                req: req.to_string(),
                image_ref: image_ref.to_string(),
                arg_entries,
                nonce,
                phase: Phase::Pending {
                    deadline: Instant::now() + pending_timeout(),
                    defer_generic_until: None,
                },
                enqueued: Instant::now(),
                outcome: outcome_tx,
            },
        );
        offer_job(&mut st, id);
        id
    };

    loop {
        // Sleep until the job's current phase deadline (the result sender wakes
        // us early through the channel).
        let wait = {
            let st = lock();
            match st.jobs.get(&id).map(|j| &j.phase) {
                Some(Phase::Pending { deadline, .. }) | Some(Phase::Inflight { deadline }) => {
                    deadline.saturating_duration_since(Instant::now())
                }
                // Job already resolved and removed: the outcome is in the channel.
                None => Duration::ZERO,
            }
        };
        match outcome_rx.recv_timeout(wait.max(Duration::from_millis(10))) {
            Ok(Outcome::Done(result)) => return Ok(result),
            Ok(Outcome::Failed(message)) => return Err(HttpError::new(500, message)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut st = lock();
                let Some(job) = st.jobs.get(&id) else {
                    // Resolved between the timeout and the lock; loop to drain
                    // the channel (the sender removed the job before sending).
                    continue;
                };
                let now = Instant::now();
                match job.phase {
                    Phase::Pending { deadline, .. } if now >= deadline => {
                        let job = remove_job(&mut st, id);
                        drop(st);
                        return Err(HttpError::new(
                            503,
                            format!(
                                "no runner for req {} (waited {:?})",
                                job.req,
                                pending_timeout()
                            ),
                        ));
                    }
                    Phase::Inflight { deadline } if now >= deadline => {
                        eprintln!(
                            "runner: job {} (req {}) missed its deadline; requeueing",
                            id, job.req
                        );
                        requeue(&mut st, id, None);
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
    job.phase = Phase::Inflight {
        deadline: Instant::now() + JOB_DEADLINE,
    };
    let body = payload(job);
    let _ = reply.send(PollReply::Job(body));
}

/// `POST /runner/poll` — hang until a matching job, eviction, or TTL.
pub(crate) fn poll(authorization: Option<&str>, body: &str) -> Result<Vec<u8>, HttpError> {
    check_auth(authorization)?;
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| HttpError::new(400, format!("invalid poll json: {e}")))?;
    let required = arg_set(&v["required"])?;
    let lineage = match &v["lineage"] {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(sets) => {
            sets.iter().map(arg_set).collect::<Result<Vec<_>, _>>()?
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
fn best_pending(st: &State, required: &ArgSet) -> Option<u64> {
    let now = Instant::now();
    st.jobs
        .iter()
        .filter(|(_, job)| match job.phase {
            Phase::Pending {
                defer_generic_until,
                ..
            } => !(required.is_empty() && defer_generic_until.is_some_and(|until| until > now)),
            Phase::Inflight { .. } => false,
        })
        .filter(|(_, job)| matches(required, &job.arg_entries))
        .min_by_key(|(_, job)| job.enqueued)
        .map(|(&id, _)| id)
}

/// Wrap a job payload as the poll response `{"job": {...}}`.
fn reply_job(payload: &str) -> Result<Vec<u8>, HttpError> {
    Ok(format!(r#"{{"job":{payload}}}"#).into_bytes())
}

/// Parse a JSON object of string → string into an [`ArgSet`].
fn arg_set(v: &serde_json::Value) -> Result<ArgSet, HttpError> {
    match v {
        serde_json::Value::Null => Ok(ArgSet::new()),
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
    let req = v["req"].as_str().unwrap_or_default();
    let nonce = v["nonce"].as_str().unwrap_or_default();
    if req.is_empty() || nonce.is_empty() {
        return Err(HttpError::new(400, "result missing req/nonce"));
    }

    let mut st = lock();
    let Some(&id) = st.by_nonce.get(nonce) else {
        return Err(HttpError::new(410, "unknown or consumed nonce"));
    };
    if st.jobs[&id].req != req {
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
            Some(result) if !result.trim().is_empty() => Outcome::Done(DispatchResult {
                result: result.trim().to_string(),
            }),
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
