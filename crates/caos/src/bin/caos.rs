//! caos: the worker-side client, baked setuid-root into worker images as
//! `/bin/caos`.
//!
//! It speaks HTTP to the server (`/object`, for storage) via
//! [`caos::HttpTransport`], and provides the container `entrypoint` — which sets
//! up the root-owned `/cas`, runs `/worker` as an unprivileged user, and prints
//! the kind + hash recorded at `/cas/out`. It never triggers compute: its `map-then`
//! records a map-then continuation the server resolves after the worker exits.
//! The shared command logic lives in the `caos` library; this binary is the
//! worker's CLI surface plus the privileged entrypoint.
//!
//! Subcommands: `get-hash`, `get`, `put`, `map-then`, `curry`, and `entrypoint`.
//! (Image import and ref resolution are user-facing only — see `caos-cli`.)

use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;
use std::sync::Mutex;

use caos::{prog_name, HttpTransport};
use tiny_http::{Header, Request, Response, Server};

/// The program `entrypoint` always runs. Images that build off the
/// `caos-worker-base` image supply this binary.
const DEFAULT_WORKER: &str = "/worker";

/// The unprivileged user `entrypoint` runs `/worker` as. The container starts as
/// root so `entrypoint` can set up — and later tear down — the root-owned
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
        // `map-then <in> -- [--map=<image>] [--then=<image>]` — record a map-then
        // continuation over the CAS path `<in>` as this worker's result at
        // /cas/out (a tail call; the server resolves it after the worker exits).
        Some("map-then") => match &args[2..] {
            [input, sep, kvs @ ..] if sep == "--" => caos::caos_map_then(&http()?, input, kvs),
            _ => Err(usage(args)),
        },
        // `curry <image> -- [--name=value | --name:@=path ...]` — bind args to an image, printing
        // a ref to the resulting curried image (run/curry it like any image).
        Some("curry") => match &args[2..] {
            [image, sep, kvs @ ..] if sep == "--" => caos::caos_curry(&http()?, image, kvs),
            _ => Err(usage(args)),
        },
        // `entrypoint [--args=<hash>]` — takes no command; it always runs /worker.
        Some("entrypoint") => match &args[2..] {
            [] => entrypoint(None),
            [flag] => match flag.strip_prefix("--args=") {
                Some(hash) => entrypoint(Some(hash)),
                None => Err(usage(args)),
            },
            _ => Err(usage(args)),
        },
        // `serve` — long-lived HTTP worker (warm-pool mode); see `serve()`.
        Some("serve") => serve(),
        _ => Err(usage(args)),
    }
}

/// The worker talks to the server over HTTP.
fn http() -> Result<HttpTransport, String> {
    HttpTransport::from_env()
}

/// One job at a time. Held across a job *and* its cleanup, so the slot never
/// frees until the VM is clean for the next job.
static SLOT: Mutex<()> = Mutex::new(());

/// A dispatched job: which args tree to run, plus the std/salt the server used
/// to thread through the container env. The worker is reused across jobs, so
/// these arrive per-request rather than as process env.
struct Job {
    args: String,
    std: String,
    salt: String,
}

/// `serve` — run as a long-lived HTTP worker instead of a one-shot container.
/// Per `POST /run` it runs the same staged job lifecycle as `entrypoint`,
/// in-process and one at a time, so each job's `/cas` lifecycle is identical to
/// the container model. A busy worker bounces the request to another instance
/// (once, via `fly-replay`) or blocks.
fn serve() -> Result<(), String> {
    let server = Server::http("[::]:8080").map_err(|e| format!("binding :8080: {e}"))?;
    eprintln!("caos serve: listening on :8080");
    for request in server.incoming_requests() {
        // One thread per connection only so a busy worker can answer "replay"
        // (or hold a blocked conn) without stalling accept(). Job concurrency is
        // still 1 — enforced by SLOT, not by the thread count.
        std::thread::spawn(move || handle(request));
    }
    Ok(())
}

fn handle(mut request: Request) {
    if request.url().split('?').next() != Some("/run") {
        return respond(request, 404, "not found");
    }
    // The proxy stamps replayed requests with `fly-replay-src`; we bounce at most
    // once, then block, so a saturated pool can't loop.
    let already_replayed = request
        .headers()
        .iter()
        .any(|h| h.field.to_string().eq_ignore_ascii_case("fly-replay-src"));

    let mut body = String::new();
    if request.as_reader().read_to_string(&mut body).is_err() {
        return respond(request, 400, "unreadable body");
    }
    let job = match parse_job(&body) {
        Ok(job) => job,
        Err(e) => return respond(request, 400, &e),
    };

    match SLOT.try_lock() {
        Ok(guard) => run_and_reply(request, &job, &guard),
        // Busy, first touch: ask the proxy to replay on a different instance.
        Err(_) if !already_replayed => {
            let hdr = Header::from_bytes(&b"fly-replay"[..], &b"elsewhere=true"[..])
                .expect("static header");
            let _ = request.respond(Response::empty(503).with_header(hdr));
        }
        // Busy, already bounced once: wait our turn here instead of bouncing again.
        Err(_) => {
            let guard = SLOT.lock().unwrap_or_else(|p| p.into_inner());
            run_and_reply(request, &job, &guard);
        }
    }
}

/// Run the job (the same staged lifecycle `entrypoint` composes, in-process),
/// reply, then reset the VM — all while holding the slot, so the next job starts
/// clean regardless of which branch above we took.
fn run_and_reply(request: Request, job: &Job, _slot: &std::sync::MutexGuard<'_, ()>) {
    match run_job(job) {
        Ok(result) => respond(request, 200, &result),
        Err(e) => respond(request, 500, &format!("worker failed: {e}")),
    }
    reset_after_job();
}

/// One job through the staged lifecycle. The process is reused across jobs, so
/// std/salt arrive with the job rather than in our env: `/cas/std` is
/// materialized from the job's value, and the worker child gets both as env
/// vars — `caos map-then`/`curry` running under it read them from there.
fn run_job(job: &Job) -> Result<String, String> {
    let std = (!job.std.is_empty()).then_some(job.std.as_str());
    let cas = cas_setup(Some(&job.args), std)?;
    let envs = [
        (caos::STD_ENV, job.std.as_str()),
        (caos::SALT_ENV, job.salt.as_str()),
    ];
    run_worker(&envs, WorkerOutput::Capture)?;
    let result = read_result(&cas)?;
    remove_cas(&cas)?;
    Ok(result)
}

fn respond(request: Request, status: u16, body: &str) {
    let resp = Response::from_string(body).with_status_code(status);
    let _ = request.respond(resp);
}

fn parse_job(body: &str) -> Result<Job, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid job json: {e}"))?;
    let field = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let args = field("args");
    if args.is_empty() {
        return Err("job missing 'args'".to_string());
    }
    Ok(Job {
        args,
        std: field("std"),
        salt: field("salt"),
    })
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
         {prog} map-then <in-cas-path> -- [--map=<image>] [--then=<image>]\n  \
         {prog} curry <image> -- [--name=value | --name:@=path ...]\n  \
         {prog} entrypoint [--args=<hash>]\n  \
         {prog} serve"
    )
}

/// `entrypoint [--args=<hash>]` — the container entrypoint: the staged job
/// lifecycle (`cas_setup` → `run_worker` → `read_result` → `remove_cas`) run
/// back to back, with `/cas/std` taken from `$CAOS_STD` and the result printed
/// on stdout. `serve` runs the same stages in-process per job.
fn entrypoint(args_hash: Option<&str>) -> Result<(), String> {
    let std = std::env::var(caos::STD_ENV).ok().filter(|s| !s.is_empty());
    let cas = cas_setup(args_hash, std.as_deref())?;
    run_worker(&[], WorkerOutput::Stream)?;
    let result = read_result(&cas)?;
    remove_cas(&cas)?;
    println!("{result}");
    Ok(())
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

/// Where a job's worker output goes.
enum WorkerOutput {
    /// Stream to this process's stderr (the one-shot container: our stdout must
    /// carry only the result line, and the container's stderr is the job log).
    Stream,
    /// Capture, surfacing it in the error on failure (serve: the process's own
    /// streams outlive the job, so the caller wants the log with the failure).
    Capture,
}

/// Run `/worker` with `envs` added to its environment. We stay root (to tear
/// down `/cas` after), but drop the worker to an unprivileged user so it can't
/// tamper with the root-owned `/cas` — only the setuid-root `caos` it invokes
/// can.
fn run_worker(envs: &[(&str, &str)], output: WorkerOutput) -> Result<(), String> {
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
    match output {
        WorkerOutput::Stream => {
            let stdout = std::io::stderr()
                .as_fd()
                .try_clone_to_owned()
                .map_err(|e| format!("duplicating stderr: {e}"))?;
            command.stdout(std::process::Stdio::from(stdout));
            let status = command
                .status()
                .map_err(|e| format!("running {DEFAULT_WORKER}: {e}"))?;
            if !status.success() {
                return Err(format!("{DEFAULT_WORKER} exited with {status}"));
            }
        }
        WorkerOutput::Capture => {
            let out = command
                .output()
                .map_err(|e| format!("running {DEFAULT_WORKER}: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "{DEFAULT_WORKER} exited with {}:\n{}{}",
                    out.status,
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
        }
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
