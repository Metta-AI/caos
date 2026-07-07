//! caos-runnerd: the generic runner — the host agent that mints fresh worker
//! containers (see `design/runner-protocol.md`).
//!
//! Each of its slots long-polls the server's `POST /runner/poll` with *no*
//! required args, so it matches any job. On a job it runs
//!
//! ```text
//! docker run --rm --network <net> -e CAOS_SERVER_URL=<url> \
//!     --entrypoint /bin/caos <image_ref> runner --job=<json>
//! ```
//!
//! and waits for the container to exit. The container owns the job from there:
//! it posts the result itself, then polls for more work for its image (that's
//! what makes it a warm runner) — this slot doesn't poll again until the
//! container dies, so each slot is exactly one machine's worth of capacity.
//! runnerd is only the crash backstop: a container that exits nonzero may have
//! died before posting, so runnerd posts a failure result with the captured
//! log (harmlessly answered 410 if the container already reported).
//!
//! Forcing `--entrypoint /bin/caos` means any image carrying the `caos` binary
//! and a `/worker` works as a compute image, regardless of its own configured
//! entrypoint/command.
//!
//! Configuration (env): `CAOS_SERVER_URL` (default `http://caos-server`; also
//! injected into the containers), `CAOS_RUNNER_TOKEN` (bearer token, if the
//! server requires one), `CAOS_RUNNER_SLOTS` (default 8), `CAOS_DOCKER_NETWORK`
//! (default `caos-net`), `CAOS_DOCKER_BIN` (default `docker`).

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// The caos binary inside every compute image, forced as the entrypoint.
const CAOS_BIN: &str = "/bin/caos";

/// How long each generic poll hangs. Purely a reconnect cadence — a generic
/// runner never idles out, it just polls again.
const POLL_TTL: Duration = Duration::from_secs(60);

/// Backoff after a failed poll (server down or restarting).
const POLL_RETRY: Duration = Duration::from_secs(2);

struct Config {
    server_url: String,
    token: Option<String>,
    slots: u32,
    network: String,
    docker_bin: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Install handlers so the process terminates on `SIGINT`/`SIGTERM` — as PID 1
/// in a container the kernel applies no default disposition, so without these
/// `docker stop` would hang until the 10s `SIGKILL`.
fn install_termination_handlers() {
    extern "C" fn terminate(_signum: std::ffi::c_int) {
        unsafe { exit_now(0) }
    }
    extern "C" {
        fn signal(signum: std::ffi::c_int, handler: extern "C" fn(std::ffi::c_int)) -> usize;
        #[link_name = "_exit"]
        fn exit_now(code: std::ffi::c_int) -> !;
    }
    const SIGINT: std::ffi::c_int = 2;
    const SIGTERM: std::ffi::c_int = 15;
    unsafe {
        signal(SIGINT, terminate);
        signal(SIGTERM, terminate);
    }
}

fn main() {
    install_termination_handlers();
    let config = Arc::new(Config {
        server_url: env_or("CAOS_SERVER_URL", "http://caos-server"),
        token: std::env::var("CAOS_RUNNER_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        slots: env_or("CAOS_RUNNER_SLOTS", "8").parse().unwrap_or(8),
        network: env_or("CAOS_DOCKER_NETWORK", "caos-net"),
        docker_bin: env_or("CAOS_DOCKER_BIN", "docker"),
    });
    eprintln!(
        "caos-runnerd: {} slots, server {}, network {}",
        config.slots, config.server_url, config.network
    );
    let mut threads = Vec::new();
    for slot in 1..config.slots {
        let config = Arc::clone(&config);
        threads.push(std::thread::spawn(move || slot_loop(&config, slot)));
    }
    slot_loop(&config, 0);
}

/// One slot: poll for a job, run its container, wait for the container to die,
/// poll again. The container (a warm runner) may serve many jobs before dying;
/// this slot stays parked on `wait` the whole time — one poll per slot lineage.
fn slot_loop(config: &Config, slot: u32) {
    loop {
        match poll(config) {
            Ok(Some(job)) => run_container(config, slot, &job),
            Ok(None) => {} // idle (or evicted, which we ignore): poll again
            Err(e) => {
                eprintln!("runnerd slot {slot}: poll failed: {e}");
                std::thread::sleep(POLL_RETRY);
            }
        }
    }
}

/// A claimed job: the fields runnerd itself needs, plus the payload verbatim to
/// hand the container.
struct Job {
    req: String,
    nonce: String,
    image_ref: String,
    payload: String,
}

/// One generic long-poll. `Some(job)` to run; `None` on idle/evicted.
fn poll(config: &Config) -> Result<Option<Job>, String> {
    let body = serde_json::json!({
        "required": {},
        "lineage": [],
        "ttl_ms": POLL_TTL.as_millis() as u64,
    });
    let url = format!("{}/runner/poll", config.server_url.trim_end_matches('/'));
    let mut req = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_timeout(POLL_TTL.as_secs() + 15)
        .with_body(body.to_string());
    if let Some(token) = &config.token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().map_err(|e| format!("POST {url}: {e}"))?;
    if resp.status_code != 200 {
        return Err(format!(
            "poll failed ({}): {}",
            resp.status_code,
            resp.as_str().unwrap_or("")
        ));
    }
    let v: serde_json::Value = serde_json::from_str(resp.as_str().unwrap_or(""))
        .map_err(|e| format!("invalid poll reply: {e}"))?;
    let Some(job) = v.get("job").filter(|j| j.is_object()) else {
        return Ok(None);
    };
    let field = |k: &str| job[k].as_str().unwrap_or_default().to_string();
    let parsed = Job {
        req: field("req"),
        nonce: field("nonce"),
        image_ref: field("image_ref"),
        payload: job.to_string(),
    };
    if parsed.req.is_empty() || parsed.nonce.is_empty() || parsed.image_ref.is_empty() {
        return Err(format!(
            "job missing req/nonce/image_ref: {}",
            parsed.payload
        ));
    }
    Ok(Some(parsed))
}

/// Run the job's container and wait it out. The container posts its own
/// results; we only backstop a crash — nonzero exit means it may never have
/// reported, so post a failure with the captured log (410 if it did report).
fn run_container(config: &Config, slot: u32, job: &Job) {
    eprintln!(
        "runnerd slot {slot}: req {} -> container ({})",
        job.req, job.image_ref
    );
    let out = Command::new(&config.docker_bin)
        .arg("run")
        .arg("--rm")
        .args(["--network", &config.network])
        .args(["-e", &format!("CAOS_SERVER_URL={}", config.server_url)])
        .args(["--entrypoint", CAOS_BIN])
        .arg(&job.image_ref)
        .arg("runner")
        .arg(format!("--job={}", job.payload))
        .output();
    let failure = match out {
        Ok(out) => {
            // Relay the container's log (the runner relays its workers' output
            // to its stderr) so it survives the container's `--rm`.
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            if out.status.success() {
                None
            } else {
                Some((
                    format!("worker container exited with {}", out.status),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                ))
            }
        }
        Err(e) => Some((format!("running {}: {e}", config.docker_bin), String::new())),
    };
    if let Some((error, log)) = failure {
        eprintln!("runnerd slot {slot}: req {}: {error}", job.req);
        if let Err(e) = post_failure(config, job, &error, &log) {
            eprintln!("runnerd slot {slot}: reporting failure: {e}");
        }
    }
}

/// The crash backstop: report a container that died without (necessarily)
/// posting. A 410 means it did post before dying — the job is settled.
fn post_failure(config: &Config, job: &Job, error: &str, log: &str) -> Result<(), String> {
    // Keep only the tail of a big log: the failure is usually at the end, and
    // the message lands in an error string a client will read.
    let tail: String = log
        .lines()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let body = serde_json::json!({
        "req": job.req, "nonce": job.nonce, "ok": false, "error": error, "log": tail,
    });
    let url = format!("{}/runner/result", config.server_url.trim_end_matches('/'));
    let mut req = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_timeout(30)
        .with_body(body.to_string());
    if let Some(token) = &config.token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().map_err(|e| format!("POST {url}: {e}"))?;
    match resp.status_code {
        200 | 410 => Ok(()),
        code => Err(format!(
            "result post failed ({code}): {}",
            resp.as_str().unwrap_or("")
        )),
    }
}
