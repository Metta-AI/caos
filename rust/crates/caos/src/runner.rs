//! The container runner: PID 1 of every worker container (`caos runner`).
//!
//! It takes the job it was started with, runs a fresh `/worker` as an
//! unprivileged user against a root-owned `/cas`, posts what the worker left at
//! `/cas/out`, then long-polls for more work for the same image until a poll
//! comes back `idle` or `exit` (see `design/runner-protocol.md`).
//!
//! A worker in an image that declares `CAOS_RESIDENT=1` may instead call
//! `caos next` when it finishes a job (see `design/daemons.md`). The runner then
//! posts the result and polls for the NEXT job of the same instance, on the
//! worker's behalf, and hands it back at `/cas/args` — the same process handles
//! it. The runner stays the only poller and the only poster, so the worker needs
//! no HTTP of its own and no knowledge of the runner protocol.
//!
//! ```text
//! worker:  job 1 ... caos next ──────────────────────────────▶ job 2 ... caos next
//! runner:  prepare ─ spawn ─ post(keep) ─ narrow reset ─ poll ─ prepare ─ answer
//! ```
//!
//! One thread waits on the worker, one accepts connections on the `caos next`
//! socket, and one renews the server-side lease; all of them report to the main
//! thread over a single channel, which is the only place a job's state changes.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{HttpTransport, Transport};

/// The program a job always runs. Images that build off the
/// `caos-worker-base` image supply this binary.
const DEFAULT_WORKER: &str = "/worker";

/// The unprivileged user a job runs `/worker` as. The container starts as
/// root so the runner can set up — and later tear down — the root-owned
/// `/cas`; it drops to this uid/gid only for the `/worker` child. The worker
/// therefore can't tamper with `/cas` directly: it must go through `caos`, which
/// is setuid-root. Override (e.g. for a different image user) with the env vars.
const WORKER_UID_ENV: &str = "CAOS_WORKER_UID";
const WORKER_GID_ENV: &str = "CAOS_WORKER_GID";
const DEFAULT_WORKER_UID: u32 = 1000;
const DEFAULT_WORKER_GID: u32 = 1000;

/// The runner's idle budget, in milliseconds: how long one follow-up poll
/// hangs before the runner exits. Ski-rental: set it near the cost of
/// restarting a container for this image. Override with `CAOS_RUNNER_TTL_MS`.
const RUNNER_TTL_ENV: &str = "CAOS_RUNNER_TTL_MS";
const DEFAULT_RUNNER_TTL_MS: u32 = 2000;

/// What an image declares about being resident, read from the runner's own
/// environment. A caller cannot set any of these: they come from the image.
const RESIDENT_ENV: &str = "CAOS_RESIDENT";
const RESIDENT_MAX_SECS_ENV: &str = "CAOS_RESIDENT_MAX_SECS";
const RESIDENT_GRACE_SECS_ENV: &str = "CAOS_RESIDENT_GRACE_SECS";
/// How long one resident poll hangs before it is repeated. Short, because it is
/// also how quickly a worker that died between jobs is noticed.
const RESIDENT_POLL_MS_ENV: &str = "CAOS_RESIDENT_POLL_MS";
const DEFAULT_RESIDENT_POLL_MS: u32 = 10_000;
const DEFAULT_RESIDENT_GRACE_SECS: u32 = 10;
/// How often the lease is renewed. A third of the server's default lease, so
/// three missed renewals are a lapse.
const LEASE_RENEW_MS_ENV: &str = "CAOS_LEASE_RENEW_MS";
const DEFAULT_LEASE_RENEW_MS: u32 = 5000;

/// The unix socket `caos next` reaches the runner on. Root-owned and mode 0600:
/// the unprivileged worker gets to it only through the setuid `caos`.
pub const NEXT_SOCKET_ENV: &str = "CAOS_NEXT_SOCKET";
const DEFAULT_NEXT_SOCKET: &str = "/run/caos/next.sock";

/// `caos next`'s exit status when the worker should leave: it was evicted, its
/// lifetime ended, or its lease lapsed. Anything else nonzero is an error.
pub const LEAVE_STATUS: u8 = 10;

/// How much of a worker's recent output is kept to explain a failure.
const TAIL_BYTES: usize = 64 * 1024;

/// In-container directory the runner drops granted secrets into, one file per
/// secret, for the worker to read (design/secrets.md, `/secret/<name>`).
const SECRET_DIR: &str = crate::SECRET_DIR;

/// A job handed to this runner: the rendezvous ids (the ArgTree is fetched and
/// unpacked from `arg_tree` itself), plus the bearer token children present back
/// to the server. Everything else about the job is derived from the ArgTree.
/// (`req` is the wire field name; its value is the ArgTree hash.)
struct RunnerJob {
    arg_tree: String,
    nonce: String,
    token: Option<String>,
    /// Present on a keyed job (design/daemons.md): the id of the ownership this
    /// runner holds for the job's instance. It names the lease to renew and the
    /// queue a resident worker's `caos next` polls.
    tenure: Option<String>,
    /// Secrets the server granted this job (design/secrets.md): name → value.
    /// Dropped at `/secret/<name>` for the worker, out of band from its args.
    secrets: Vec<(String, String)>,
}

impl RunnerJob {
    fn parse(json: &str) -> Result<RunnerJob, String> {
        let v: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("invalid job json: {e}"))?;
        RunnerJob::from_value(&v)
    }

    fn from_value(v: &serde_json::Value) -> Result<RunnerJob, String> {
        let field = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
        let arg_tree = field("req").unwrap_or_default().to_string();
        let nonce = field("nonce").unwrap_or_default().to_string();
        if arg_tree.is_empty() || nonce.is_empty() {
            return Err("job missing req/nonce".to_string());
        }
        let secrets = match v.get("secrets") {
            Some(serde_json::Value::Object(map)) => map
                .iter()
                .map(|(name, value)| {
                    let value = value
                        .as_str()
                        .ok_or_else(|| format!("secret {name:?} value is not a string"))?;
                    Ok((name.clone(), value.to_string()))
                })
                .collect::<Result<Vec<_>, String>>()?,
            _ => Vec::new(),
        };
        Ok(RunnerJob {
            arg_tree,
            nonce,
            token: field("token").map(str::to_string),
            tenure: field("tenure").map(str::to_string),
            secrets,
        })
    }
}

/// What the image declares about residency, and the runner's own timing knobs.
struct Config {
    resident: bool,
    /// Hard lifetime cap, enforced here rather than trusted to the daemon.
    max_life: Option<Duration>,
    /// SIGTERM to SIGKILL.
    grace: Duration,
    poll_ms: u32,
    renew: Duration,
    uid: u32,
    gid: u32,
}

impl Config {
    fn from_env() -> Config {
        Config {
            resident: std::env::var(RESIDENT_ENV).is_ok_and(|v| v == "1"),
            max_life: crate::env_u32(RESIDENT_MAX_SECS_ENV)
                .map(|secs| Duration::from_secs(u64::from(secs))),
            grace: Duration::from_secs(u64::from(
                crate::env_u32(RESIDENT_GRACE_SECS_ENV).unwrap_or(DEFAULT_RESIDENT_GRACE_SECS),
            )),
            poll_ms: crate::env_u32(RESIDENT_POLL_MS_ENV).unwrap_or(DEFAULT_RESIDENT_POLL_MS),
            renew: Duration::from_millis(u64::from(
                crate::env_u32(LEASE_RENEW_MS_ENV).unwrap_or(DEFAULT_LEASE_RENEW_MS),
            )),
            uid: crate::env_u32(WORKER_UID_ENV).unwrap_or(DEFAULT_WORKER_UID),
            gid: crate::env_u32(WORKER_GID_ENV).unwrap_or(DEFAULT_WORKER_GID),
        }
    }
}

/// Everything the main thread hears about.
enum Event {
    /// The worker process ended.
    Exit(Result<ExitStatus, String>),
    /// The worker is done with its job and wants another (`caos next`).
    Next {
        error: Option<String>,
        reply: mpsc::Sender<Answer>,
    },
}

/// What `caos next` is told.
enum Answer {
    /// The next job's args are at `/cas/args`.
    Job,
    /// Stop: the worker has until the grace period ends.
    Leave,
    Refused(String),
}

/// A worker's recent output, and the secret values to keep out of it.
#[derive(Default)]
struct Output {
    tail: String,
    masks: Vec<String>,
}

type SharedOutput = Arc<Mutex<Output>>;

/// Why a leave happened; only for the message a failed job carries.
#[derive(Clone, Copy)]
enum Leave {
    Evicted,
    LifetimeCap,
    LeaseLost,
}

impl Leave {
    fn describe(self) -> &'static str {
        match self {
            Leave::Evicted => "the server evicted this runner",
            Leave::LifetimeCap => "the image's CAOS_RESIDENT_MAX_SECS ran out",
            Leave::LeaseLost => "the server no longer recognizes this runner's lease",
        }
    }
}

/// Whether the runner goes back to polling for any job of its image, or is done.
enum After {
    Poll,
    Done,
}

struct Runner {
    t: HttpTransport,
    cfg: Config,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    output: SharedOutput,
    started: Instant,
    /// Whether the worker has called `caos next`. Only such a worker is a daemon:
    /// one that has not behaves exactly as on any image, including that its runner
    /// goes back to polling for work when it exits.
    used_next: bool,
    /// Our image's CAS-level name, and the instance's, for the follow-up polls'
    /// required args — read off the placeholders `/cas/args` was materialized
    /// with (every entry is tagged with its hash).
    image_oid: Option<String>,
    affinity_oid: Option<String>,
}

/// `runner --job=<json>` — run the handed-in job through the staged lifecycle,
/// post its result to the server, then long-poll for more work for this image
/// (required args `{base: <oid>}`, learned from our own materialization of the
/// first job's args) until a poll comes back empty (`idle`), the server evicts us
/// (`exit`), or we never learned the oid.
pub fn run(job_json: &str) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let mut runner = Runner {
        t: HttpTransport::from_env()?,
        cfg: Config::from_env(),
        tx,
        rx,
        output: Arc::default(),
        started: Instant::now(),
        used_next: false,
        image_oid: None,
        affinity_oid: None,
    };
    if runner.cfg.resident {
        serve_next_socket(runner.tx.clone())?;
    }
    let mut job = RunnerJob::parse(job_json)?;
    loop {
        let token = job.token.clone();
        let (after, failure) = runner.life(job)?;
        // A failed job doesn't kill a warm runner — but never having learned
        // our image's oid (setup failed before /cas/args existed) means we have
        // nothing to advertise, so don't linger.
        let Some(oid) = runner.image_oid.clone() else {
            return failure.map_or(Ok(()), Err);
        };
        if matches!(after, After::Done) {
            return Ok(());
        }
        match next_job(&runner.t, &oid, &token)? {
            Some(next) => job = next,
            None => return Ok(()),
        }
    }
}

impl Runner {
    /// One worker's life: set up for `first`, run `/worker`, and for a resident
    /// worker hand it each following job of the instance until it leaves.
    /// Returns what to do next, and the failure of the last job if there was one
    /// and nothing has said it (see `run`).
    fn life(&mut self, first: RunnerJob) -> Result<(After, Option<String>), String> {
        let mut job = first;
        let (cas, salt) = match self.prepare(&job, true) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.post(&job, &Err(error.clone()), None, false)?;
                reset_after_job(&self.cfg);
                return Ok((After::Poll, Some(error)));
            }
        };
        // The lifetime cap measures a DAEMON's life, which starts with its keyed job:
        // a warm runner that has been idle for hours is not already out of time.
        if job.tenure.is_some() {
            self.started = Instant::now();
        }
        let heartbeat = job
            .tenure
            .clone()
            .map(|tenure| Heartbeat::start(&self.t, tenure, job.token.clone(), self.cfg.renew));
        // The first job's context is in the environment as it always was, so a
        // worker that never calls `caos next` is unchanged. It is STALE from a
        // resident worker's second job on, which is why `caos` reads the files
        // the runner writes before it reads these (see `job_nonce`).
        let envs = vec![
            (crate::SALT_ENV, salt),
            (crate::JOB_NONCE_ENV, job.nonce.clone()),
        ];
        let worker = self.spawn_worker(&envs)?;
        let outcome = self.supervise(&mut job, &cas, &worker, heartbeat.as_ref());
        if let Some(heartbeat) = &heartbeat {
            heartbeat.stop();
        }
        outcome
    }

    /// The main loop of one worker's life: the worker ended, asked for its next
    /// job, or ran out of time.
    fn supervise(
        &mut self,
        job: &mut RunnerJob,
        cas: &Path,
        worker: &Worker,
        heartbeat: Option<&Heartbeat>,
    ) -> Result<(After, Option<String>), String> {
        let cas = cas.to_path_buf();
        loop {
            if let Some(leave) = self.must_leave(heartbeat) {
                // The job is running and its time is up. The server already
                // failed it if the lease is what ran out, and 410s our post.
                let error = format!("{}: the job was stopped", leave.describe());
                self.terminate(worker);
                let tail = self.tail();
                self.post(job, &Err(format!("{error}\n{tail}")), None, false)?;
                if let Some(heartbeat) = heartbeat {
                    heartbeat.release(leave);
                }
                reset_after_job(&self.cfg);
                return Ok((After::Done, Some(error)));
            }
            let event = match self.rx.recv_timeout(Duration::from_millis(500)) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("runner event channel closed".to_string())
                }
            };
            match event {
                Event::Exit(status) => {
                    return self.after_exit(job, &cas, worker, status);
                }
                Event::Next { error, reply } => {
                    if let Some(done) = self.next(job, &cas, worker, heartbeat, error, &reply)? {
                        return Ok(done);
                    }
                }
            }
        }
    }

    /// The worker exited without asking for another job: its result is whatever
    /// it left at `/cas/out`, exactly as for any worker. A resident worker that
    /// stops this way (an explicit stop op) takes the runner with it.
    fn after_exit(
        &mut self,
        job: &RunnerJob,
        cas: &Path,
        worker: &Worker,
        status: Result<ExitStatus, String>,
    ) -> Result<(After, Option<String>), String> {
        worker.join_readers();
        let tail = self.tail();
        let ran: Result<String, String> = match status {
            Ok(status) if status.success() => Ok(String::new()),
            Ok(status) => Err(format!("{DEFAULT_WORKER} exited with {status}:\n{tail}")),
            Err(error) => Err(format!("waiting for {DEFAULT_WORKER}: {error}")),
        };
        // Perf data the worker chose to leave behind, read BEFORE the failure
        // check so a run that died still reports it, and before `remove_cas`
        // takes the directory away. Optional by construction: `read_hash` on a
        // path no worker wrote is an error, and no out-trace is the normal case.
        let out_trace = crate::read_hash(&cas.join("out-trace")).ok();
        let result = match ran {
            Ok(_) => read_result(cas),
            Err(error) => Err(error),
        };
        remove_secrets();
        let _ = remove_cas(cas);
        // Not `keep`: the worker is gone, so this runner owns nothing any more.
        self.post(job, &result, out_trace.as_deref(), false)?;
        reset_after_job(&self.cfg);
        Ok((
            if self.used_next {
                After::Done
            } else {
                After::Poll
            },
            result.err(),
        ))
    }

    /// `caos next`: post this job's result, keeping the instance, then wait for
    /// the next job and set it up. `None` means the worker has its next job and
    /// the loop goes on; `Some` means the worker's life is over.
    fn next(
        &mut self,
        job: &mut RunnerJob,
        cas: &Path,
        worker: &Worker,
        heartbeat: Option<&Heartbeat>,
        error: Option<String>,
        reply: &mpsc::Sender<Answer>,
    ) -> Result<Option<(After, Option<String>)>, String> {
        if !self.cfg.resident {
            let _ = reply.send(Answer::Refused(format!(
                "this image is not resident: declare {RESIDENT_ENV}=1 in its environment"
            )));
            return Ok(None);
        }
        let Some(tenure) = job.tenure.clone() else {
            let _ = reply.send(Answer::Refused(
                "caos next needs a job that carries an `affinity` arg: only a keyed job \
                 has an instance to stay resident for"
                    .to_string(),
            ));
            return Ok(None);
        };
        let ran = match error {
            Some(error) => Err(error),
            None => read_result(cas),
        };
        let out_trace = crate::read_hash(&cas.join("out-trace")).ok();
        self.post(job, &ran, out_trace.as_deref(), true)?;
        self.used_next = true;
        remove_secrets();
        narrow_reset();
        self.output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .tail
            .clear();

        loop {
            let leave = self.must_leave(heartbeat);
            // A worker that died between jobs is noticed within one poll.
            if let Ok(Event::Exit(_)) = self.rx.try_recv() {
                worker.join_readers();
                if let Some(heartbeat) = heartbeat {
                    heartbeat.release(Leave::Evicted);
                }
                reset_after_job(&self.cfg);
                return Ok(Some((After::Done, None)));
            }
            let outcome = match leave {
                Some(leave) => Polled::Leave(leave),
                None => self.poll_owner(&tenure, job.token.clone())?,
            };
            match outcome {
                Polled::Idle => {}
                Polled::Leave(leave) => {
                    let _ = reply.send(Answer::Leave);
                    self.terminate(worker);
                    if let Some(heartbeat) = heartbeat {
                        heartbeat.release(leave);
                    }
                    reset_after_job(&self.cfg);
                    return Ok(Some((After::Done, None)));
                }
                Polled::Job(next) => match self.prepare(&next, false) {
                    Ok(_) => {
                        *job = next;
                        let _ = reply.send(Answer::Job);
                        return Ok(None);
                    }
                    Err(error) => {
                        // This job could not be set up, and the daemon never
                        // saw it: fail it, keep the instance, keep polling.
                        self.post(&next, &Err(error), None, true)?;
                        remove_secrets();
                        narrow_reset();
                    }
                },
            }
        }
    }

    /// Whether the runner must stop now: the server no longer recognizes its
    /// lease, or its lifetime is up. The cap is for a KEYED life only — an
    /// instance's daemon. A job nothing keyed holds a slot for as long as the job
    /// takes, and an image that declares a cap for its daemons must not kill the
    /// ordinary jobs it also runs.
    fn must_leave(&self, heartbeat: Option<&Heartbeat>) -> Option<Leave> {
        let heartbeat = heartbeat?;
        if heartbeat.lost() {
            return Some(Leave::LeaseLost);
        }
        match self.cfg.max_life {
            Some(max) if self.started.elapsed() >= max => Some(Leave::LifetimeCap),
            _ => None,
        }
    }

    /// One long-poll for the instance's next job, on the tenure.
    fn poll_owner(&self, tenure: &str, token: Option<String>) -> Result<Polled, String> {
        let (Some(image), Some(affinity)) = (&self.image_oid, &self.affinity_oid) else {
            return Err("a resident job's args name no base or affinity".to_string());
        };
        let ttl_ms = self.cfg.poll_ms;
        let body = serde_json::json!({
            "required": { "base": image, "affinity": affinity },
            "tenure": tenure,
            // Our parent is a generic runner (runnerd) — it polls with no required
            // args once we die, so a job we can't serve can evict us toward it.
            "lineage": [ {} ],
            "ttl_ms": ttl_ms,
        });
        let url = runner_url(&self.t, "poll")?;
        let resp = runner_post(
            &url,
            &body.to_string(),
            &token,
            u64::from(ttl_ms) / 1000 + 15,
        )?;
        match resp.status_code {
            200 => {}
            // The server no longer has this ownership: it lapsed, or the key is
            // someone else's. Nothing to renew and nothing to poll for.
            410 => return Ok(Polled::Leave(Leave::LeaseLost)),
            code => {
                return Err(format!(
                    "poll failed ({code}): {}",
                    resp.as_str().unwrap_or("")
                ))
            }
        }
        let v: serde_json::Value = serde_json::from_str(resp.as_str().unwrap_or(""))
            .map_err(|e| format!("invalid poll reply: {e}"))?;
        if v.get("exit").and_then(serde_json::Value::as_bool) == Some(true) {
            return Ok(Polled::Leave(Leave::Evicted));
        }
        match v.get("job") {
            Some(job) if job.is_object() => Ok(Polled::Job(RunnerJob::from_value(job)?)),
            _ => Ok(Polled::Idle),
        }
    }

    /// Set up for one job: its ArgTree at `/cas/args`, its secrets at
    /// `/secret`, and — for a resident worker, whose environment cannot change
    /// from job to job — the job's nonce and salt as root-owned files `caos`
    /// reads in place of `CAOS_JOB_NONCE` and `CAOS_SALT`. `fresh` wipes `/cas`
    /// first; a later job of a resident worker keeps what earlier ones fetched,
    /// which is checked against its oid and so still correct.
    fn prepare(&mut self, job: &RunnerJob, fresh: bool) -> Result<(PathBuf, String), String> {
        let (arg_tree, salt) = read_arg_tree(&self.t, &job.arg_tree)?;
        let cas = if fresh {
            cas_setup(&self.t, Some(&arg_tree))?
        } else {
            let cas = crate::cas_dir();
            crate::fetch_and_materialize(&self.t, &cas.join("args"), &arg_tree)?;
            cas
        };
        if self.image_oid.is_none() {
            self.image_oid = crate::read_hash(&cas.join("args").join("base")).ok();
        }
        if self.affinity_oid.is_none() {
            self.affinity_oid = crate::read_hash(&cas.join("args").join("affinity")).ok();
        }
        // Drop the granted secrets at `/secret/<name>` just before the worker
        // runs (design/secrets.md). `write_secrets` wipes any prior job's
        // `/secret` first, so a warm runner never leaks a secret into a later job
        // that wasn't granted it.
        write_secrets(&job.secrets)?;
        {
            let mut output = self.output.lock().unwrap_or_else(|p| p.into_inner());
            for (_, value) in &job.secrets {
                if !value.is_empty() && !output.masks.contains(value) {
                    output.masks.push(value.clone());
                }
            }
        }
        if self.cfg.resident {
            write_root_file(&cas.join(NONCE_FILE), &job.nonce)?;
            write_root_file(&cas.join(SALT_FILE), &salt)?;
        }
        Ok((cas, salt))
    }

    /// Run `/worker` with `envs` added to its environment. We stay root (to tear
    /// down `/cas` after), but drop the worker to an unprivileged user so it can't
    /// tamper with the root-owned `/cas` — only the setuid-root `caos` it invokes
    /// can. It leads its own process group so it can be stopped as a whole.
    ///
    /// Its output is relayed to our stderr (the container log) AS IT IS WRITTEN,
    /// masked, and a bounded tail is kept to explain a failure. A worker that
    /// handles many jobs does not exit, so waiting for exit to relay it would
    /// hold the log back indefinitely. Masking here is the one chokepoint that
    /// covers the whole chain: every downstream log (this container's stderr,
    /// runnerd's relay, the server's failure message) derives from these lines.
    /// Best-effort and transform-blind — a value the worker base64'd or split
    /// slips through; this catches an accidental echo, not a determined
    /// exfiltrator.
    fn spawn_worker(&self, envs: &[(&'static str, String)]) -> Result<Worker, String> {
        let (uid, gid) = (self.cfg.uid, self.cfg.gid);
        let mut command = Command::new(DEFAULT_WORKER);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }
        // SAFETY: the closure runs in the forked child before exec and only makes
        // async-signal-safe syscalls. We drop privileges by hand (rather than
        // `Command::uid`/`gid`) so we can also clear supplementary groups — `groups`
        // is still unstable — and in the right order: groups, then gid, then uid,
        // each while we're still root.
        unsafe {
            command.pre_exec(move || {
                extern "C" {
                    fn setsid() -> i32;
                }
                setsid();
                if drop_privileges(uid, gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("running {DEFAULT_WORKER}: {e}"))?;
        let pid = child.id() as i32;
        let readers = vec![
            relay(child.stdout.take().expect("piped"), self.output.clone()),
            relay(child.stderr.take().expect("piped"), self.output.clone()),
        ];
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let status = child.wait().map_err(|e| e.to_string());
            let _ = tx.send(Event::Exit(status));
        });
        Ok(Worker {
            pid,
            readers: Mutex::new(readers),
        })
    }

    /// Stop the worker: SIGTERM to its process group, the grace period to finish
    /// (a daemon exports its state here), then SIGKILL.
    fn terminate(&self, worker: &Worker) {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe { kill(-worker.pid, 15) };
        let deadline = Instant::now() + self.cfg.grace;
        let mut exited = false;
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Exit(_)) => {
                    exited = true;
                    break;
                }
                // A worker asking for another job while it is being told to
                // leave is told so again.
                Ok(Event::Next { reply, .. }) => {
                    let _ = reply.send(Answer::Leave);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if !exited {
            unsafe { kill(-worker.pid, 9) };
            let _ = self.rx.recv_timeout(Duration::from_secs(5));
        }
        worker.join_readers();
    }

    fn tail(&self) -> String {
        self.output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .tail
            .clone()
    }

    /// POST the job's outcome to `/runner/result`. A 410 means the nonce was
    /// already consumed (someone else reported) — fine, the job is settled.
    /// `keep` says this runner stays the owner of the job's instance and is
    /// about to poll for the next job of it. (`req` is the wire field name; its
    /// value is the ArgTree hash.)
    fn post(
        &self,
        job: &RunnerJob,
        ran: &Result<String, String>,
        out_trace: Option<&str>,
        keep: bool,
    ) -> Result<(), String> {
        // `out_trace` rides alongside the result, never inside it: a result is
        // content-addressed and keys every consumer of it, so folding timings or
        // cache statistics into one would make all of them miss whenever the
        // numbers moved. That separation is the whole reason `/cas/out-trace`
        // exists next to `/cas/out`.
        let mut body = match ran {
            Ok(result) => serde_json::json!({
                "req": job.arg_tree, "nonce": job.nonce, "ok": true, "result": result,
            }),
            Err(error) => serde_json::json!({
                "req": job.arg_tree, "nonce": job.nonce, "ok": false, "error": error,
            }),
        };
        if let Some(oid) = out_trace {
            body["out_trace"] = serde_json::Value::String(oid.to_string());
        }
        if keep {
            body["keep"] = serde_json::Value::Bool(true);
        }
        let url = runner_url(&self.t, "result")?;
        let resp = runner_post(&url, &body.to_string(), &job.token, 30)?;
        match resp.status_code {
            200 | 410 => Ok(()),
            code => Err(format!(
                "posting result ({code}): {}",
                resp.as_str().unwrap_or("")
            )),
        }
    }
}

enum Polled {
    Job(RunnerJob),
    Idle,
    Leave(Leave),
}

/// The running `/worker`: its process group, and the threads relaying its output.
struct Worker {
    pid: i32,
    readers: Mutex<Vec<JoinHandle<()>>>,
}

impl Worker {
    /// Wait for the output relays to drain, so everything the worker wrote is in
    /// the tail before it is read.
    fn join_readers(&self) {
        let readers = std::mem::take(&mut *self.readers.lock().unwrap_or_else(|p| p.into_inner()));
        for reader in readers {
            let _ = reader.join();
        }
    }
}

/// Relay a worker's output line by line: masked, to our stderr, and into the
/// bounded tail.
fn relay(stream: impl Read + Send + 'static, output: SharedOutput) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let mut output = output.lock().unwrap_or_else(|p| p.into_inner());
            let line = mask_secrets(String::from_utf8_lossy(&buf).into_owned(), &output.masks);
            eprint!("{line}");
            output.tail.push_str(&line);
            if output.tail.len() > TAIL_BYTES {
                let mut cut = output.tail.len() - TAIL_BYTES;
                while !output.tail.is_char_boundary(cut) {
                    cut += 1;
                }
                output.tail.drain(..cut);
            }
        }
    })
}

/// Renews the lease on an instance while this runner holds it. A lease is how
/// the server tells a dead owner from a slow one, so this runs on a thread of
/// its own and never waits on the worker.
struct Heartbeat {
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    url: Option<String>,
    token: Option<String>,
    tenure: String,
}

impl Heartbeat {
    fn start(t: &HttpTransport, tenure: String, token: Option<String>, renew: Duration) -> Self {
        let url = runner_url(t, "lease").ok();
        let heartbeat = Heartbeat {
            stop: Arc::new(AtomicBool::new(false)),
            lost: Arc::new(AtomicBool::new(false)),
            url,
            token,
            tenure,
        };
        if let Some(url) = heartbeat.url.clone() {
            let (stop, lost) = (heartbeat.stop.clone(), heartbeat.lost.clone());
            let (token, tenure) = (heartbeat.token.clone(), heartbeat.tenure.clone());
            std::thread::spawn(move || {
                let body = serde_json::json!({ "tenure": tenure }).to_string();
                while !stop.load(Ordering::Relaxed) {
                    // A failed renewal is retried at the next tick. Only the
                    // server saying the tenure is gone ends it: anything else is
                    // a network blip, and the lease has two renewals of slack.
                    if let Ok(resp) = runner_post(&url, &body, &token, 10) {
                        if resp.status_code == 410 {
                            lost.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                    let until = Instant::now() + renew;
                    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            });
        }
        heartbeat
    }

    fn lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Give the instance back, so what queued behind it is offered to the pool
    /// now rather than when the lease runs out. Nothing to give back if the
    /// server already took it.
    fn release(&self, why: Leave) {
        self.stop();
        if matches!(why, Leave::LeaseLost) {
            return;
        }
        if let Some(url) = &self.url {
            let body = serde_json::json!({ "tenure": self.tenure, "release": true });
            let _ = runner_post(url, &body.to_string(), &self.token, 10);
        }
    }
}

/// The root-owned files holding a resident worker's current job context.
const NONCE_FILE: &str = "nonce";
const SALT_FILE: &str = "salt";

/// Write `contents` root-owned and readable by root only: it is `caos`, which is
/// setuid-root, that reads these, never the worker.
fn write_root_file(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|e| format!("writing {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

/// The job nonce of the job a worker is running: the file a resident runner
/// writes for each job, else the environment, which any runner exports for the
/// FIRST job. The file wins because the environment of a worker that outlives
/// its first job is stale; only a resident runner writes one, so a worker that
/// is not resident reads the environment exactly as before.
pub fn job_nonce() -> Option<String> {
    read_context_file(NONCE_FILE).or_else(|| std::env::var(crate::JOB_NONCE_ENV).ok())
}

/// The salt of the job a worker is running, by the same rule as [`job_nonce`].
pub fn job_salt() -> Option<String> {
    read_context_file(SALT_FILE).or_else(|| std::env::var(crate::SALT_ENV).ok())
}

fn read_context_file(name: &str) -> Option<String> {
    if !JOB_CONTEXT_FILES.load(Ordering::Relaxed) {
        return None;
    }
    std::fs::read_to_string(crate::cas_dir().join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Whether this process may trust `/cas/nonce` and `/cas/salt`. Only `/bin/caos`,
/// the worker-side binary, turns it on. A client that merely shares a container
/// with a runner — the test stack's tested `caos-cli`, run by an interpreter whose
/// `/cas` belongs to the OUTER job — must not pick up the outer job's context.
static JOB_CONTEXT_FILES: AtomicBool = AtomicBool::new(false);

pub fn trust_job_context_files() {
    JOB_CONTEXT_FILES.store(true, Ordering::Relaxed);
}

/// Listen on the `caos next` socket for the life of the runner.
fn serve_next_socket(tx: mpsc::Sender<Event>) -> Result<(), String> {
    let path = PathBuf::from(
        std::env::var(NEXT_SOCKET_ENV).unwrap_or_else(|_| DEFAULT_NEXT_SOCKET.to_string()),
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod {}: {e}", dir.display()))?;
    }
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).map_err(|e| format!("binding {}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = serve_next_connection(stream, &tx);
            });
        }
    });
    Ok(())
}

/// One `caos next` conversation. The request is one JSON line:
/// `{"op":"next","error":<text|absent>}` — answered with one line when the next
/// job is ready — or `{"op":"stream"}`, which stays open for the worker's life.
/// Answers are `job`, `leave` or `refused <why>`.
fn serve_next_connection(stream: UnixStream, tx: &mpsc::Sender<Event>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_default();
    let ask = |error: Option<String>| -> Answer {
        let (reply, answer) = mpsc::channel();
        if tx.send(Event::Next { error, reply }).is_err() {
            return Answer::Refused("the runner is gone".to_string());
        }
        answer
            .recv()
            .unwrap_or_else(|_| Answer::Refused("the runner is gone".to_string()))
    };
    let write_answer = |writer: &mut UnixStream, answer: &Answer| -> std::io::Result<bool> {
        match answer {
            Answer::Job => writeln!(writer, "job")?,
            Answer::Leave => writeln!(writer, "leave")?,
            Answer::Refused(why) => writeln!(writer, "refused {}", why.replace('\n', " "))?,
        }
        writer.flush()?;
        Ok(matches!(answer, Answer::Job))
    };
    match request["op"].as_str() {
        Some("next") => {
            let answer = ask(request["error"].as_str().map(str::to_string));
            write_answer(&mut writer, &answer)?;
        }
        // The worker is already running its first job, so the first line is for
        // it: every job gets exactly one `job` line, and a worker handles one
        // per line and answers `done` (or `error <text>`) to ask for the next.
        Some("stream") => {
            writeln!(writer, "job")?;
            writer.flush()?;
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 {
                    break;
                }
                let line = line.trim();
                let error = match line.split_once(' ') {
                    Some(("error", text)) => Some(text.to_string()),
                    None if line == "error" => Some("the worker reported an error".to_string()),
                    _ => None,
                };
                let answer = ask(error);
                if !write_answer(&mut writer, &answer)? {
                    break;
                }
            }
        }
        _ => {
            writeln!(writer, "refused unknown request")?;
        }
    }
    Ok(())
}

/// `caos next [--error <text>]` and `caos next --stream`: the worker's side of
/// the conversation above. Returns the process's exit status: 0 with the next
/// job's args at `/cas/args`, [`LEAVE_STATUS`] when the worker should stop.
pub fn next_client(args: &[String]) -> Result<u8, String> {
    let (request, stream) = match args {
        [] => (serde_json::json!({"op": "next"}), false),
        [flag, text] if flag == "--error" => {
            (serde_json::json!({"op": "next", "error": text}), false)
        }
        [flag] if flag == "--stream" => (serde_json::json!({"op": "stream"}), true),
        _ => return Err("usage: caos next [--error <text> | --stream]".to_string()),
    };
    let path = std::env::var(NEXT_SOCKET_ENV).unwrap_or_else(|_| DEFAULT_NEXT_SOCKET.to_string());
    let mut socket = UnixStream::connect(&path).map_err(|e| {
        format!(
            "connecting to the runner at {path}: {e} \
             (caos next works only in a worker whose image declares {RESIDENT_ENV}=1)"
        )
    })?;
    writeln!(socket, "{request}").map_err(|e| format!("writing to the runner: {e}"))?;
    if stream {
        return stream_client(socket);
    }
    let mut answer = String::new();
    BufReader::new(socket)
        .read_line(&mut answer)
        .map_err(|e| format!("reading the runner's answer: {e}"))?;
    answer_status(answer.trim())
}

fn answer_status(answer: &str) -> Result<u8, String> {
    match answer.split_once(' ').unwrap_or((answer, "")) {
        ("job", _) => Ok(0),
        ("leave", _) => Ok(LEAVE_STATUS),
        ("refused", why) => Err(why.to_string()),
        _ => Err(format!("the runner hung up ({answer:?})")),
    }
}

/// `--stream`: stdin lines go to the runner and its lines come back on stdout,
/// so a daemon that does not want a process per job reads `job` lines and
/// writes `done`. EOF on stdin closes the conversation; `leave` ends it with
/// [`LEAVE_STATUS`].
fn stream_client(socket: UnixStream) -> Result<u8, String> {
    let mut upstream = socket.try_clone().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let mut line = String::new();
        let stdin = std::io::stdin();
        while stdin.read_line(&mut line).is_ok_and(|n| n > 0) {
            if upstream.write_all(line.as_bytes()).is_err() {
                return;
            }
            line.clear();
        }
        let _ = upstream.shutdown(std::net::Shutdown::Write);
    });
    let mut stdout = std::io::stdout();
    for line in BufReader::new(socket).lines() {
        let line = line.map_err(|e| e.to_string())?;
        writeln!(stdout, "{line}").map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())?;
        match line.split_once(' ').unwrap_or((&line, "")) {
            ("leave", _) => return Ok(LEAVE_STATUS),
            ("refused", why) => return Err(why.to_string()),
            _ => {}
        }
    }
    Ok(0)
}

/// Write `secrets` (name → value) into `/secret/<name>`, wiping any prior
/// contents first. Written root-owned but world-readable so the unprivileged
/// worker can read them; a name with a path separator is rejected (it must be a
/// single file component).
fn write_secrets(secrets: &[(String, String)]) -> Result<(), String> {
    remove_secrets();
    if secrets.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(SECRET_DIR).map_err(|e| format!("creating {SECRET_DIR}: {e}"))?;
    std::fs::set_permissions(SECRET_DIR, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {SECRET_DIR}: {e}"))?;
    for (name, value) in secrets {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(format!(
                "secret name {name:?} is not a single path component"
            ));
        }
        let path = format!("{SECRET_DIR}/{name}");
        std::fs::write(&path, value).map_err(|e| format!("writing secret {name}: {e}"))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))
            .map_err(|e| format!("chmod secret {name}: {e}"))?;
    }
    Ok(())
}

/// Remove `/secret` and everything in it. Succeeds if it's already gone.
fn remove_secrets() {
    let _ = std::fs::remove_dir_all(SECRET_DIR);
}

/// Unpack an ArgTree: its hash (returned back for `/cas/args`) and the salt
/// (its reserved `salt` entry, empty if absent). `base`/`salt` are entries of
/// this one tree, per SPEC's ArgTree.
fn read_arg_tree(t: &dyn Transport, arg_tree: &str) -> Result<(String, String), String> {
    let (kind, content) = t.get_object(arg_tree)?;
    if kind != "tree" {
        return Err(format!("arg tree {arg_tree} is a {kind}, not a tree"));
    }
    let tree = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
        .map_err(|e| format!("malformed arg tree {arg_tree}: {e}"))?;
    let blob = |oid: gix::ObjectId| -> Result<String, String> {
        let (_, content) = t.get_object(&oid.to_string())?;
        Ok(String::from_utf8_lossy(&content).trim().to_string())
    };
    let mut salt = String::new();
    for entry in tree.entries {
        if entry.filename.to_vec().as_slice() == b"salt" {
            salt = blob(entry.oid.into())?;
        }
    }
    Ok((arg_tree.to_string(), salt))
}

/// One follow-up long-poll for more work for our image. `Some(job)` to run it;
/// `None` on `idle` (our TTL passed) or `exit` (evicted) — either way, quit.
fn next_job(
    t: &HttpTransport,
    image_oid: &str,
    token: &Option<String>,
) -> Result<Option<RunnerJob>, String> {
    let ttl_ms = crate::env_u32(RUNNER_TTL_ENV).unwrap_or(DEFAULT_RUNNER_TTL_MS);
    let body = serde_json::json!({
        "required": { "base": image_oid },
        // Our parent is a generic runner (runnerd) — it polls with no required
        // args once we die, so a job we can't serve can evict us toward it.
        "lineage": [ {} ],
        "ttl_ms": ttl_ms,
    });
    let url = runner_url(t, "poll")?;
    // The HTTP timeout only backstops a dead server; the poll itself hangs for
    // the TTL server-side, so pad well past it (seconds granularity).
    let resp = runner_post(
        &url,
        &body.to_string(),
        token,
        u64::from(ttl_ms) / 1000 + 15,
    )?;
    if resp.status_code != 200 {
        return Err(format!(
            "poll failed ({}): {}",
            resp.status_code,
            resp.as_str().unwrap_or("")
        ));
    }
    let v: serde_json::Value = serde_json::from_str(resp.as_str().unwrap_or(""))
        .map_err(|e| format!("invalid poll reply: {e}"))?;
    match v.get("job") {
        Some(job) if job.is_object() => Ok(Some(RunnerJob::from_value(job)?)),
        _ => Ok(None),
    }
}

/// The server's runner endpoint `/runner/<leaf>`.
fn runner_url(t: &HttpTransport, leaf: &str) -> Result<String, String> {
    Ok(format!(
        "{}/runner/{leaf}",
        t.server_url()?.trim_end_matches('/')
    ))
}

/// POST a runner-protocol request, presenting the job's bearer token if any.
fn runner_post(
    url: &str,
    body: &str,
    token: &Option<String>,
    timeout_secs: u64,
) -> Result<minreq::Response, String> {
    let mut req = minreq::post(url)
        .with_header("content-type", "application/json")
        .with_timeout(timeout_secs)
        .with_body(body.to_string());
    if let Some(token) = token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    req.send().map_err(|e| format!("POST {url}: {e}"))
}

/// Reset the worker-writable surface between jobs. A pooled runner keeps the
/// container across jobs, so nothing is disposed for us: `entrypoint` wipes
/// `/cas` on each run, and here we reap strays and clear the scratch dirs
/// (`scratch()` writes /tmp).
fn reset_after_job(cfg: &Config) {
    reap_uid(cfg.uid);
    for dir in ["/tmp", "/var/tmp", "/dev/shm"] {
        wipe_dir_contents(dir);
    }
}

/// What a resident worker loses between jobs: everything that belonged to the
/// job that just ended — its args, its result, its trace, its context and its
/// secrets. What it keeps is the rest of `/cas` (content fetched there is
/// checked against its oid, so a stale entry is still correct for its oid),
/// the scratch directories, and every process. That is the point of staying.
fn narrow_reset() {
    let cas = crate::cas_dir();
    for name in ["args", "out", "out-trace", NONCE_FILE, SALT_FILE] {
        let path = cas.join(name);
        let _ = if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
    remove_secrets();
}

/// SIGKILL every process owned by `uid`. The slot means one job at a time and the
/// worker uid is dedicated, so this only reaps strays the just-finished worker
/// left behind — nothing else tears them down in a pooled container.
fn reap_uid(uid: u32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let me = std::process::id() as i32;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        let owned = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|u| u.parse::<u32>().ok())
            == Some(uid);
        if owned {
            unsafe { kill(pid, 9) };
        }
    }
}

/// Remove the children of `dir` (keeping it as a mount point). On tmpfs this is
/// fast and complete.
fn wipe_dir_contents(dir: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let removed = if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        let _ = removed;
    }
}

/// Set up a fresh `/cas` for one job: wipe whatever a prior job left, recreate
/// it empty (fail if we can't), verify it supports the xattrs we rely on, then
/// populate `/cas/args` from `args_hash` (one level, like `get-hash`), so the
/// worker can read its inputs there.
fn cas_setup(t: &HttpTransport, args_hash: Option<&str>) -> Result<PathBuf, String> {
    let cas = crate::cas_dir();
    remove_cas(&cas)?;
    std::fs::create_dir_all(&cas).map_err(|e| format!("creating {}: {e}", cas.display()))?;
    // Root-owned and only root-writable: the worker reaches `/cas` solely through
    // this setuid-root binary, never by writing here directly.
    crate::set_mode(&cas, crate::MODE_FETCHED_DIR)?;
    crate::probe_xattr(&cas)?;
    if let Some(hash) = args_hash {
        crate::fetch_and_materialize(t, &cas.join("args"), hash)?;
    }
    Ok(cas)
}

/// Replace every injected secret value in `log` with a fixed marker. Longest
/// values first, so a secret that contains another is masked whole. Empty
/// values are skipped (they'd match everywhere). The marker names no secret.
fn mask_secrets(mut log: String, secrets: &[String]) -> String {
    let mut values: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for value in values {
        if log.contains(value) {
            log = log.replace(value, "[redacted secret]");
        }
    }
    log
}

/// Read back the result the worker recorded at `/cas/out`, as `"<type> <hash>"`.
/// Everything under /cas got there via get/put, which tag each path with its
/// hash, so no re-hashing — the caller can record a correctly-typed result
/// placeholder without fetching, or resolve a `promise` (a map-then continuation
/// `caos map-then` recorded) once this job's slot is free.
fn read_result(cas: &Path) -> Result<String, String> {
    let out = cas.join("out");
    let hash = crate::read_hash(&out)?;
    let kind = crate::result_kind(&out)?;
    Ok(format!("{kind} {hash}"))
}

/// Delete the CAS directory and everything in it. Succeeds if it's already gone.
fn remove_cas(cas: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(cas) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("removing {}: {e}", cas.display())),
    }
}

/// Drop to `uid`/`gid`, clearing supplementary groups first. Returns 0 on
/// success, or a non-zero return from the first failing syscall (the caller then
/// reads `errno`). Must be called while still privileged, in this order:
/// supplementary groups, then the group, then the user — once the uid is dropped
/// the others would be denied. Only used from the worker's `pre_exec`, so it
/// must stay async-signal-safe: these three raw syscalls are.
fn drop_privileges(uid: u32, gid: u32) -> i32 {
    // Resolved against the libc std already links (musl in the image).
    extern "C" {
        fn setgroups(size: usize, list: *const u32) -> i32;
        fn setgid(gid: u32) -> i32;
        fn setuid(uid: u32) -> i32;
    }
    unsafe {
        let rc = setgroups(0, std::ptr::null());
        if rc != 0 {
            return rc;
        }
        let rc = setgid(gid);
        if rc != 0 {
            return rc;
        }
        setuid(uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_longest_secret_first_and_ignores_empty() {
        let masked = mask_secrets(
            "token=abcdef and abc".to_string(),
            &["abc".to_string(), "abcdef".to_string(), String::new()],
        );
        assert_eq!(masked, "token=[redacted secret] and [redacted secret]");
    }

    #[test]
    fn answers_map_to_statuses() {
        assert_eq!(answer_status("job"), Ok(0));
        assert_eq!(answer_status("leave"), Ok(LEAVE_STATUS));
        assert_eq!(answer_status("refused no way"), Err("no way".to_string()));
        assert!(answer_status("").is_err());
    }
}
