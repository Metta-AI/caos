//! caos: the worker-side client, baked setuid-root into worker images as
//! `/bin/caos`.
//!
//! It speaks HTTP to the server (`/object`, for storage) via
//! [`caos::HttpTransport`], and provides the container `runner` — which runs a
//! job (set up the root-owned `/cas`, run `/worker` as an unprivileged user,
//! post the kind + hash recorded at `/cas/out` back to the server), then
//! long-polls for more work for its image until an idle TTL passes (see
//! `design/runner-protocol.md`). It never triggers compute: its `map-then`
//! records a map-then continuation the server resolves after the worker's job
//! finishes. The shared command logic lives in the `caos` library; this binary
//! is the worker's CLI surface plus the privileged runner.
//!
//! Subcommands: `get-hash`, `get`, `put`, `put-commit`, `hash`, `map-then`,
//! `run-then`, `curry`, and `runner`.
//! (Image import and ref resolution are user-facing only — see `caos-cli`.)

use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use caos::{prog_name, HttpTransport, Transport};

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}: {err}", prog_name(&args));
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("get-hash") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(hash), Some(path), None) => caos::get_hash(&http()?, hash, path),
            _ => Err(usage(args)),
        },
        Some("get") => {
            let (path, depth) = caos::parse_get(&args[2..])?;
            caos::get(&http()?, path, depth)
        }
        Some("put") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(src), Some(dst), None) => caos::put(&http()?, src, dst),
            _ => Err(usage(args)),
        },
        // `put-commit <src-file> <cas-path>` — store the file's bytes as a git
        // *commit* object, record it (kind-tagged) at the CAS path, and print
        // its hash. How a worker mints a turn/step commit.
        Some("put-commit") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(src), Some(dst), None) => caos::put_commit(&http()?, src, dst),
            _ => Err(usage(args)),
        },
        // `hash <cas-path>` — print the git hash recorded on a CAS path (e.g. a
        // commit-valued arg whose hash becomes the next commit's parent).
        Some("hash") => match (args.get(2), args.get(3)) {
            (Some(path), None) => caos::cas_hash(path),
            _ => Err(usage(args)),
        },
        // `map-then <in> -- [--map=<image>] [--then=<image>]` — record a map-then
        // continuation over the CAS path `<in>` as this worker's result at
        // /cas/out (a tail call; the server resolves it after the worker exits).
        Some("map-then") => match &args[2..] {
            [input, sep, kvs @ ..] if sep == "--" => caos::caos_map_then(&http()?, input, kvs),
            _ => Err(usage(args)),
        },
        // `run-then <in> -- --run=<image> [--then=<image>]` — the single-valued
        // map-then: the server runs `run(--in=<in>)` once, then (optionally)
        // `then(--in=<in>, --result=<R>)`. The same tail-call contract.
        Some("run-then") => match &args[2..] {
            [input, sep, kvs @ ..] if sep == "--" => caos::caos_run_then(&http()?, input, kvs),
            _ => Err(usage(args)),
        },
        // `curry <image> -- [--name=value | --name:@=path ...]` — bind args to an image, printing
        // a ref to the resulting curried image (run/curry it like any image).
        Some("curry") => match &args[2..] {
            [image, sep, kvs @ ..] if sep == "--" => caos::caos_curry(&http()?, image, kvs),
            _ => Err(usage(args)),
        },
        // `runner --job=<json>` — run the handed-in job, then poll for more; see
        // `runner()`.
        Some("runner") => match &args[2..] {
            [flag] => match flag.strip_prefix("--job=") {
                Some(json) => runner(json),
                None => Err(usage(args)),
            },
            _ => Err(usage(args)),
        },
        _ => Err(usage(args)),
    }
}

/// The worker talks to the server over HTTP.
fn http() -> Result<HttpTransport, String> {
    HttpTransport::from_env()
}

/// The runner's idle budget, in milliseconds: how long one follow-up poll
/// hangs before the runner exits. Ski-rental: set it near the cost of
/// restarting a container for this image. Override with `CAOS_RUNNER_TTL_MS`.
const RUNNER_TTL_ENV: &str = "CAOS_RUNNER_TTL_MS";
const DEFAULT_RUNNER_TTL_MS: u32 = 2000;

/// A job handed to this runner: the rendezvous ids (the request is fetched and
/// unpacked from `req` itself), plus the bearer token children present back to
/// the server. Everything else about the job is derived from `req`.
struct RunnerJob {
    req: String,
    nonce: String,
    token: Option<String>,
}

impl RunnerJob {
    fn parse(json: &str) -> Result<RunnerJob, String> {
        let v: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("invalid job json: {e}"))?;
        RunnerJob::from_value(&v)
    }

    fn from_value(v: &serde_json::Value) -> Result<RunnerJob, String> {
        let field = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
        let req = field("req").unwrap_or_default().to_string();
        let nonce = field("nonce").unwrap_or_default().to_string();
        if req.is_empty() || nonce.is_empty() {
            return Err("job missing req/nonce".to_string());
        }
        Ok(RunnerJob {
            req,
            nonce,
            token: field("token").map(str::to_string),
        })
    }
}

/// `runner --job=<json>` — the container runner (see
/// `design/runner-protocol.md`): run the handed-in job through the staged
/// lifecycle, post its result to the server, then long-poll for more work for
/// this image — required args `{image: <oid>}`, learned from our own
/// materialization of the first job's args — until a poll comes back empty
/// (`idle`), the server evicts us (`exit`), or we never learned the oid.
fn runner(job_json: &str) -> Result<(), String> {
    let t = http()?;
    let mut job = RunnerJob::parse(job_json)?;
    let mut image_oid: Option<String> = None;
    loop {
        let ran = run_runner_job(&t, &job, &mut image_oid);
        post_result(&t, &job, &ran)?;
        reset_after_job();
        // A failed job doesn't kill a warm runner — but never having learned
        // our image's oid (setup failed before /cas/args existed) means we have
        // nothing to advertise, so don't linger.
        let Some(oid) = image_oid.clone() else {
            return ran.map(|_| ());
        };
        match next_job(&t, &oid, &job.token)? {
            Some(next) => job = next,
            None => return Ok(()),
        }
    }
}

/// One job through the staged lifecycle: unpack the request, set up `/cas`,
/// run `/worker`, read back `/cas/out`, tear down. The process is reused across
/// jobs, so std/salt come from the request rather than our env: `/cas/std` is
/// materialized from the request's value, and the worker child gets both as
/// env vars — `caos map-then`/`curry` running under it read them from there.
struct RanJob {
    result: String,
}

fn run_runner_job(
    t: &HttpTransport,
    job: &RunnerJob,
    image_oid: &mut Option<String>,
) -> Result<RanJob, String> {
    let (args, std, salt) = read_req_tree(t, &job.req)?;
    let cas = cas_setup(Some(&args), std.as_deref())?;
    // Our image's CAS-level name, for the follow-up poll's required args — read
    // off the placeholder cas_setup just materialized (every entry is tagged
    // with its hash).
    if image_oid.is_none() {
        *image_oid = caos::read_hash(&cas.join("args").join("image")).ok();
    }
    let envs = [
        (caos::STD_ENV, std.as_deref().unwrap_or("")),
        (caos::SALT_ENV, salt.as_str()),
    ];
    run_worker(&envs)?;
    let result = read_result(&cas)?;
    remove_cas(&cas)?;
    Ok(RanJob { result })
}

/// Unpack a request tree `{args, std, salt}`: the args-tree hash, the std tree
/// hash (its entry is a blob *naming* the tree; `None` if empty or absent), and
/// the salt (empty if absent).
fn read_req_tree(t: &dyn Transport, req: &str) -> Result<(String, Option<String>, String), String> {
    let (kind, content) = t.get_object(req)?;
    if kind != "tree" {
        return Err(format!("request {req} is a {kind}, not a tree"));
    }
    let tree = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
        .map_err(|e| format!("malformed request tree {req}: {e}"))?;
    let blob = |oid: gix::ObjectId| -> Result<String, String> {
        let (_, content) = t.get_object(&oid.to_string())?;
        Ok(String::from_utf8_lossy(&content).trim().to_string())
    };
    let (mut args, mut std, mut salt) = (None, None, String::new());
    for entry in tree.entries {
        match entry.filename.to_vec().as_slice() {
            b"args" => args = Some(entry.oid.to_string()),
            b"std" => std = Some(blob(entry.oid.into())?).filter(|s| !s.is_empty()),
            b"salt" => salt = blob(entry.oid.into())?,
            _ => {}
        }
    }
    let args = args.ok_or_else(|| format!("request {req} missing 'args'"))?;
    Ok((args, std, salt))
}

/// POST the job's outcome to `/runner/result`. A 410 means the nonce was
/// already consumed (someone else reported) — fine, the job is settled.
fn post_result(
    t: &HttpTransport,
    job: &RunnerJob,
    ran: &Result<RanJob, String>,
) -> Result<(), String> {
    let body = match ran {
        Ok(ran) => serde_json::json!({
            "req": job.req, "nonce": job.nonce, "ok": true,
            "result": &ran.result,
        }),
        Err(error) => serde_json::json!({
            "req": job.req, "nonce": job.nonce, "ok": false, "error": error,
        }),
    };
    let url = runner_url(t, "result")?;
    let resp = runner_post(&url, &body.to_string(), &job.token, 30)?;
    match resp.status_code {
        200 | 410 => Ok(()),
        code => Err(format!(
            "posting result ({code}): {}",
            resp.as_str().unwrap_or("")
        )),
    }
}

/// One follow-up long-poll for more work for our image. `Some(job)` to run it;
/// `None` on `idle` (our TTL passed) or `exit` (evicted) — either way, quit.
fn next_job(
    t: &HttpTransport,
    image_oid: &str,
    token: &Option<String>,
) -> Result<Option<RunnerJob>, String> {
    let ttl_ms = caos::env_u32(RUNNER_TTL_ENV).unwrap_or(DEFAULT_RUNNER_TTL_MS);
    let body = serde_json::json!({
        "required": { "image": image_oid },
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

/// Reset the worker-writable surface between jobs — the guarantees the disposable
/// container used to give for free. `entrypoint` already wipes `/cas` on each run;
/// here we reap any strays and clear the scratch dirs (`scratch()` writes /tmp).
fn reset_after_job() {
    let uid = caos::env_u32(WORKER_UID_ENV).unwrap_or(DEFAULT_WORKER_UID);
    reap_uid(uid);
    for dir in ["/tmp", "/var/tmp", "/dev/shm"] {
        wipe_dir_contents(dir);
    }
}

/// SIGKILL every process owned by `uid`. The slot means one job at a time and the
/// worker uid is dedicated, so this only reaps strays the just-finished worker
/// left behind (the container teardown used to kill these implicitly).
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

fn usage(args: &[String]) -> String {
    let prog = prog_name(args);
    format!(
        "usage:\n  {prog} get-hash <hash> <path>\n  \
         {prog} get [-r | --recursive[=<depth>]] <path>\n  \
         {prog} put <src-path> <cas-path>\n  \
         {prog} put-commit <src-file> <cas-path>\n  \
         {prog} hash <cas-path>\n  \
         {prog} map-then <in-cas-path> -- [--map=<image>] [--then=<image>]\n  \
         {prog} run-then <in-cas-path> -- --run=<image> [--then=<image>]\n  \
         {prog} curry <image> -- [--name=value | --name:@=path ...]\n  \
         {prog} runner --job=<json>"
    )
}

/// Set up a fresh `/cas` for one job: wipe whatever a prior job left, recreate
/// it empty (fail if we can't), verify it supports the xattrs we rely on, then
/// populate `/cas/args` from `args_hash` and `/cas/std` from `std_hash` (each
/// one level, like `get-hash`), so the worker can read its inputs there.
fn cas_setup(
    args_hash: Option<&str>,
    std_hash: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    let cas = caos::cas_dir();
    remove_cas(&cas)?;
    std::fs::create_dir_all(&cas).map_err(|e| format!("creating {}: {e}", cas.display()))?;
    // Root-owned and only root-writable: the worker reaches `/cas` solely through
    // this setuid-root binary, never by writing here directly.
    caos::set_mode(&cas, caos::MODE_FETCHED_DIR)?;
    caos::probe_xattr(&cas)?;
    if let Some(hash) = args_hash {
        caos::fetch_and_materialize(&http()?, &cas.join("args"), hash)?;
    }
    if let Some(std) = std_hash {
        caos::fetch_and_materialize(&http()?, &cas.join("std"), std)?;
    }
    Ok(cas)
}

/// Run `/worker` with `envs` added to its environment. We stay root (to tear
/// down `/cas` after), but drop the worker to an unprivileged user so it can't
/// tamper with the root-owned `/cas` — only the setuid-root `caos` it invokes
/// can. Its output is captured, relayed to our stderr (the container log), and
/// included in the error on failure so the failure post carries the log.
fn run_worker(envs: &[(&str, &str)]) -> Result<(), String> {
    let uid = caos::env_u32(WORKER_UID_ENV).unwrap_or(DEFAULT_WORKER_UID);
    let gid = caos::env_u32(WORKER_GID_ENV).unwrap_or(DEFAULT_WORKER_GID);
    let mut command = std::process::Command::new(DEFAULT_WORKER);
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
            if drop_privileges(uid, gid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = command
        .output()
        .map_err(|e| format!("running {DEFAULT_WORKER}: {e}"))?;
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprint!("{log}");
    if !out.status.success() {
        return Err(format!(
            "{DEFAULT_WORKER} exited with {}:\n{log}",
            out.status
        ));
    }
    Ok(())
}

/// Read back the result the worker recorded at `/cas/out`, as `"<type> <hash>"`.
/// Everything under /cas got there via get/put, which tag each path with its
/// hash, so no re-hashing — the caller can record a correctly-typed result
/// placeholder without fetching, or resolve a `promise` (a map-then continuation
/// `caos map-then` recorded) once this job's slot is free.
fn read_result(cas: &std::path::Path) -> Result<String, String> {
    let out = cas.join("out");
    let hash = caos::read_hash(&out)?;
    let kind = caos::result_kind(&out)?;
    Ok(format!("{kind} {hash}"))
}

/// Delete the CAS directory and everything in it. Succeeds if it's already gone.
fn remove_cas(cas: &std::path::Path) -> Result<(), String> {
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
/// the others would be denied. Only used from `entrypoint`'s `pre_exec`, so it
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
