//! caos-worker-runner: a generic worker runner.
//!
//! It receives a compiled worker binary as the `bin` argument and execs it — so
//! any worker built as a single self-contained binary runs in this one warm,
//! pooled image instead of being baked into (and provisioned as) its own image.
//! The exec'd binary *is* the worker: it reads its remaining arguments from
//! `/cas/args` and writes its result to `/cas/out`, exactly as a baked-in
//! `/worker` would.
//!
//! We `exec` (replace this process) rather than spawn: the `caos` runner forked
//! us and waits, then reads `/cas/out` — so the binary inherits our place as the
//! child, and *its* `/cas/out` is what the run returns. The binary inherits our
//! environment too (so `PATH` still finds the setuid `caos`).

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

use worker_common::{arg, caos, run_worker, scratch};

fn main() -> ExitCode {
    run_worker("runner", run)
}

fn run() -> Result<(), String> {
    // Fetch the worker binary (the `bin` arg). `/cas` is root-owned, so stage a
    // writable, executable copy under a scratch dir before running it.
    let bin = arg("bin");
    caos(["get", bin.as_str()])?;
    let dir = scratch("run")?;
    let exe = dir.join("worker");
    std::fs::copy(&bin, &exe).map_err(|e| format!("staging worker binary: {e}"))?;
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod worker binary: {e}"))?;

    // Replace this process with the worker; `exec` only returns on failure.
    let err = Command::new(&exe).exec();
    Err(format!("exec {}: {err}", exe.display()))
}
