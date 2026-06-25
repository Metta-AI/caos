//! A trivial worker, built from source by the rustc builder in this test: it
//! writes a fixed greeting to /cas/out. Editing the greeting (see
//! greeter-edited.rs) must yield a different, independently-built worker.
use std::fs;
use std::process::ExitCode;
use worker_common::{caos, path, run_worker, scratch};

fn main() -> ExitCode {
    run_worker("greeter", run)
}

fn run() -> Result<(), String> {
    let out = scratch("out")?;
    fs::write(out.join("greeting"), "hello from a source-built worker\n")
        .map_err(|e| format!("write: {e}"))?;
    caos(["put", path(&out), "/cas/out"])
}
