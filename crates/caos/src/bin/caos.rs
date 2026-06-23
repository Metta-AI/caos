//! caos: the worker-side client, baked setuid-root into worker images as
//! `/bin/caos`.
//!
//! It speaks HTTP to the server (`/object` for storage, `/run` for compute) via
//! [`caos::HttpTransport`], and provides the container `entrypoint` — which sets
//! up the root-owned `/cas`, runs `/worker` as an unprivileged user, and prints
//! the hash recorded at `/cas/out`. The shared command logic lives in the `caos`
//! library; this binary is the worker's CLI surface plus the privileged
//! entrypoint.
//!
//! Subcommands: `get-hash`, `get`, `put`, `run`, `curry`, `build-args`, and
//! `entrypoint`. (Image import and ref resolution are user-facing only — see
//! `caos-cli`.)

use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use caos::{prog_name, HttpTransport};

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
        // `run <image> <output> -- [--name=value ...]`. The `--` separates the
        // fixed arguments from the (possibly empty) list of key/value args.
        Some("run") => match &args[2..] {
            [image, output, sep, kvs @ ..] if sep == "--" => {
                caos::caos_run(&http()?, image, output, kvs)
            }
            _ => Err(usage(args)),
        },
        // `curry <image> -- [--name=value ...]` — bind args to an image, printing
        // a ref to the resulting curried image (run/curry it like any image).
        Some("curry") => match &args[2..] {
            [image, sep, kvs @ ..] if sep == "--" => caos::caos_curry(&http()?, image, kvs),
            _ => Err(usage(args)),
        },
        // `build-args [--name=value ...]` — print the hash of the assembled args
        // tree (path values stored from disk, everything else a literal blob).
        Some("build-args") => caos::build_args(&http()?, &args[2..]),
        // `entrypoint [--args=<hash>]` — takes no command; it always runs /worker.
        Some("entrypoint") => match &args[2..] {
            [] => entrypoint(None),
            [flag] => match flag.strip_prefix("--args=") {
                Some(hash) => entrypoint(Some(hash)),
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

fn usage(args: &[String]) -> String {
    let prog = prog_name(args);
    format!(
        "usage:\n  {prog} get-hash <hash> <path>\n  \
         {prog} get [-r | --recursive[=<depth>]] <path>\n  \
         {prog} put <src-path> <cas-path>\n  \
         {prog} run <image> <output-cas-path> -- [--name=value ...]\n  \
         {prog} curry <image> -- [--name=value ...]\n  \
         {prog} build-args [--name=value ...]\n  \
         {prog} entrypoint [--args=<hash>]"
    )
}

/// `entrypoint [--args=<hash>]` — the container entrypoint. Wipes the CAS
/// directory, optionally populates `/cas/args` from `--args=<hash>` and `/cas/std`
/// from `$CAOS_STD`, runs `/worker`, prints the hash recorded at `/cas/out`, then
/// removes the CAS directory.
fn entrypoint(args_hash: Option<&str>) -> Result<(), String> {
    let cas = caos::cas_dir();

    // Start clean: delete the CAS directory and recreate it empty (fail if we
    // can't), then verify it supports the xattrs we rely on.
    remove_cas(&cas)?;
    std::fs::create_dir_all(&cas).map_err(|e| format!("creating {}: {e}", cas.display()))?;
    // Root-owned and only root-writable: the worker reaches `/cas` solely through
    // this setuid-root binary, never by writing here directly.
    caos::set_mode(&cas, caos::MODE_FETCHED_DIR)?;
    caos::probe_xattr(&cas)?;

    // Populate /cas/args from the given hash, like `get-hash <hash> /cas/args`,
    // so the worker can read its inputs there.
    if let Some(hash) = args_hash {
        caos::fetch_and_materialize(&http()?, &cas.join("args"), hash)?;
    }

    // Populate /cas/std (one level) from the built-in tree the server threaded in,
    // so the worker can reach builtins as `/cas/std/<name>`.
    if let Ok(std) = std::env::var(caos::STD_ENV) {
        if !std.is_empty() {
            caos::fetch_and_materialize(&http()?, &cas.join("std"), &std)?;
        }
    }

    // Run the worker, sending its stdout to our stderr so that our own stdout
    // carries only the resulting hash. We stay root (to tear down `/cas` after),
    // but drop the *worker* to an unprivileged user so it can't tamper with the
    // root-owned `/cas` — only the setuid-root `caos` it invokes can.
    let stdout = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("duplicating stderr: {e}"))?;
    let uid = caos::env_u32(WORKER_UID_ENV).unwrap_or(DEFAULT_WORKER_UID);
    let gid = caos::env_u32(WORKER_GID_ENV).unwrap_or(DEFAULT_WORKER_GID);
    let mut command = std::process::Command::new(DEFAULT_WORKER);
    command.stdout(std::process::Stdio::from(stdout));
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
    let status = command
        .status()
        .map_err(|e| format!("running {DEFAULT_WORKER}: {e}"))?;
    if !status.success() {
        return Err(format!("{DEFAULT_WORKER} exited with {status}"));
    }

    // Everything under /cas got there via get/put/run, which tag each path with
    // its hash, so /cas/out already knows its hash — read it (and its type) back
    // before teardown. The server returns this `"<type> <hash>"` to the caller so
    // it can record a correctly-typed result placeholder without fetching.
    let out = cas.join("out");
    let hash = caos::read_hash(&out)?;
    let kind = if out.is_dir() { "tree" } else { "blob" };

    // Tear down.
    remove_cas(&cas)?;

    println!("{kind} {hash}");
    Ok(())
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
