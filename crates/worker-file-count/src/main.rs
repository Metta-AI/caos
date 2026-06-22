//! caos-worker-file-count: a leaf algebra meant to be driven by the fold worker
//! as its `post`. It receives the node being combined as `--in` and the folded
//! child results as `--children`. A file (a leaf — `--in` is a blob) counts as 1;
//! anything else returns the sum of its child counts (each `--children` entry
//! holds a number). The result, a blob holding the count, is left at `/cas/out`.
//! So `fold --post=file-count` over a tree totals its leaf files.
//!
//! It only touches the object server (no `caos run`); the compute server injects
//! that URL at runtime.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{arg, caos, entries, path, scratch};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("file-count: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    // A file leaf counts as 1; otherwise sum the folded child counts. `--in` and
    // `--children` arrive as placeholders, so the type is readable without
    // fetching content.
    let total: u64 = if Path::new(&arg("in")).is_file() {
        eprintln!("file-count: a file counts as 1");
        1
    } else {
        eprintln!("file-count: summing child counts");
        let children = arg("children");
        caos(["get", &children])?; // expand the directory one level: a placeholder per child

        let mut total = 0u64;
        for child in entries(&children)? {
            caos(["get", path(&child)])?; // expand the placeholder to its bytes
            let text = fs::read_to_string(&child)
                .map_err(|e| format!("reading {}: {e}", child.display()))?;
            total += text
                .trim()
                .parse::<u64>()
                .map_err(|e| format!("parsing count in {}: {e}", child.display()))?;
        }
        total
    };

    let out = scratch("file-count")?.join("count");
    fs::write(&out, format!("{total}\n")).map_err(|e| format!("writing count: {e}"))?;
    caos(["put", path(&out), "/cas/out"])
}
