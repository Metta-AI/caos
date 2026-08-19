//! std/hello — mirror the run's arguments back as text.
//!
//! The smallest thing that proves an installation works end to end: a client
//! formed an ArgTree, the server scheduled it, a runner started a container,
//! the worker read `/cas/args`, and a result came back. One command, and the
//! answer is on stdout:
//!
//! ```text
//! $ caos-cli run --base:@=DEEP-DEPS/hello --greeting=hi --who=world
//! hello: 2 arguments
//!   greeting = hi
//!   who = world
//! ```
//!
//! The result is a BLOB rather than a tree so that `run` with no output path
//! streams it straight to the terminal — a tree would force the caller to pick
//! a checkout directory and then go looking in it, which is a worse first
//! experience for the one command whose whole job is to say "this works".
//!
//! Two kinds of entry are skipped, both SELF-REFERENCE rather than input.
//! `base`/`salt`/`secret-hash` are caos' own reserved entries. And `workerN` is
//! how a compiled worker rides the shared runner pool — a rustc-built entry
//! resolves to `curry(runner, worker1=<binary>)`, so without this hello reports
//! its own 5 MB binary as one of "its arguments", having fetched it to do so.
//! What is left is exactly what the caller wrote.

use std::path::Path;
use std::process::ExitCode;

use worker_common::{cas_hash, caos, entries, file_name, path, run_worker, scratch, ARGS};

fn main() -> ExitCode {
    run_worker("hello", run)
}

/// ArgTree entries that are caos' own, not the caller's (SPEC, "ArgTree").
/// Listing them here rather than importing: a std tool gets `worker-common`,
/// not the crate these names are defined in.
const RESERVED: [&str; 3] = ["base", "salt", "secret-hash"];

/// Is `name` this worker's own plumbing rather than something the caller passed?
///
/// `workerN` is the documented naming for an executable in the chain (see
/// `std/bash`'s worker, `std/runner`), and a compiled std entry always carries
/// `worker1` — its own binary, bound by the curry that makes it runnable.
fn is_self_reference(name: &str) -> bool {
    RESERVED.contains(&name)
        || name
            .strip_prefix("worker")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// How much of a text argument to echo before summarizing it instead. Long
/// enough for the values a person types, short enough that mirroring a file
/// someone passed by mistake does not bury the answer.
const MAX_ECHO: usize = 200;

fn run() -> Result<(), String> {
    let args: Vec<_> = entries(ARGS)?
        .into_iter()
        .filter(|e| !is_self_reference(&file_name(e)))
        .collect();

    let mut report = format!("hello: {} argument{}\n", args.len(), plural(args.len()));
    for entry in &args {
        let name = file_name(entry);
        report.push_str(&format!("  {name} = {}\n", describe(entry)?));
    }

    let out = scratch("hello")?.join("report");
    std::fs::write(&out, report).map_err(|e| format!("writing the report: {e}"))?;
    caos(["put", path(&out), "/cas/out"])
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// One argument, as a line a person can read.
///
/// A directory argument is reported by KIND AND HASH, never walked: an arg can
/// be a whole source tree, and fetching one to print it would turn a smoke test
/// into a download. Its hash is the useful fact anyway — it is what the cache
/// keyed on.
fn describe(entry: &Path) -> Result<String, String> {
    let hash = cas_hash(path(entry))?;
    if entry.is_dir() {
        return Ok(format!("tree {hash}"));
    }
    // A file is worth reading: it is almost always a `--name=value` literal,
    // which is the thing a caller most wants echoed.
    caos(["get", path(entry)])?;
    let bytes = std::fs::read(entry).map_err(|e| format!("reading {}: {e}", entry.display()))?;
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) if text.len() <= MAX_ECHO && !text.contains('\n') => text.to_string(),
        // Multi-line, oversized or not UTF-8: say what it is instead of
        // wrecking the layout with it.
        _ => return Ok(format!("blob {hash} ({} bytes)", bytes.len())),
    };
    Ok(text)
}
