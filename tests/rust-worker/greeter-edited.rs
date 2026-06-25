//! The edited counterpart of greeter.rs — same worker, different greeting. The
//! test builds this after greeter.rs to prove that changed source rebuilds to a
//! new, distinct worker (and doesn't leak into the first one's result).
use std::fs;
use std::process::ExitCode;
use worker_common::{caos, path, run_worker, scratch};

fn main() -> ExitCode {
    run_worker("greeter", run)
}

fn run() -> Result<(), String> {
    let out = scratch("out")?;
    fs::write(out.join("greeting"), "a different greeting entirely\n")
        .map_err(|e| format!("write: {e}"))?;
    caos(["put", path(&out), "/cas/out"])
}
