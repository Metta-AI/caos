//! caos-worker-hello: a minimal demonstration worker. It reads every argument
//! `caos run` passed (one entry per `--name=value` under `/cas/args`), assembles
//! a result tree holding each argument's content under its name plus a small
//! `receipt`, and stores that at `/cas/out`.
//!
//! Each argument is staged by symlinking the already-fetched `/cas/args/<name>`
//! into the result tree; `caos put` resolves those to the content's recorded
//! hash, so nothing is re-read. Only `receipt` is a real file.

use std::fs;
use std::process::ExitCode;

use worker_common::{caos, entries, file_name, link, path, run_worker, scratch, ARGS};

fn main() -> ExitCode {
    run_worker("hello", run)
}

fn run() -> Result<(), String> {
    eprintln!("hello-worker: reading {ARGS}");
    let out = scratch("out")?;
    let mut receipt = String::from("worker ran\n");
    for entry in entries(ARGS)? {
        let name = file_name(&entry);
        caos(["get", path(&entry)])?; // expand the placeholder to real content
        link(&entry, out.join(&name))?;
        eprintln!("  saw {name}");
        receipt.push_str(&format!("saw {name}\n"));
    }
    fs::write(out.join("receipt"), receipt).map_err(|e| format!("writing receipt: {e}"))?;
    caos(["put", path(&out), "/cas/out"])
}
