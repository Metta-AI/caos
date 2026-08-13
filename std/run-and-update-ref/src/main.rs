//! Run one already-complete CAOS request, then append its task status to a
//! conversation ref without making the caller wait in a worker container.
//!
//! `Q = run-and-update-ref { subreq: R, target-ref: F }` has two positions:
//! start emits `run-request-then R` with this same Q (minus the call result) as
//! its callback; finish appends `.caos/async/<Q>/status` to F and returns R's
//! result unchanged. A caught R failure is represented as a small result tree
//! after F is marked failed because the current promise protocol can deliver a
//! failure to a callback but cannot ask that callback to rethrow it.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, forward, own_args_tree, read_arg, run_request_then_catching, run_worker,
    scratch, Arg,
};

mod refs;

fn main() -> ExitCode {
    run_worker("run-and-update-ref", run)
}

fn run() -> Result<(), String> {
    let has_result = Path::new(&arg("result")).exists();
    let has_error = Path::new(&arg("error")).exists();
    match (has_result, has_error) {
        (false, false) => start(),
        (true, false) => finish_success(),
        (false, true) => finish_failure(),
        (true, true) => Err("callback received both result and error".to_string()),
    }
}

fn start() -> Result<(), String> {
    let request = read_arg("subreq")?;
    validate_hash(&request, "subreq")?;
    let target_ref = read_arg("target-ref")?;
    refs::validate_target_ref(&target_ref)?;

    // Curry the exact Q forward as the finish callback. `result`/`error` are
    // call-time args supplied later by the promise interpreter.
    let q = own_args_tree()?;
    let callback = caos_curry(&q, &[("task", Arg::Lit(&q))])?;
    run_request_then_catching(&request, &callback)
}

fn finish_success() -> Result<(), String> {
    finish("complete")?;
    // Exact pass-through, including tree/commit result kinds and without
    // loading potentially large output bytes.
    forward(&arg("result"), "/cas/out")
}

fn finish_failure() -> Result<(), String> {
    finish("failed")?;
    caos(["get", &arg("error")])?;
    let error =
        fs::read_to_string(arg("error")).map_err(|e| format!("reading subrequest failure: {e}"))?;
    let out = scratch("run-and-update-ref-failure")?;
    fs::write(out.join("status"), "failed\n")
        .map_err(|e| format!("writing failure status: {e}"))?;
    fs::write(out.join("error"), error).map_err(|e| format!("writing failure detail: {e}"))?;
    caos(["put", worker_common::path(&out), "/cas/out"])
}

fn finish(status: &str) -> Result<(), String> {
    let target_ref = read_arg("target-ref")?;
    refs::validate_target_ref(&target_ref)?;
    let task = read_arg("task")?;
    validate_hash(&task, "task")?;
    refs::append_status(&target_ref, &task, status)
}

fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid {what} hash {hash:?}"));
    }
    Ok(())
}
