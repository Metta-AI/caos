//! caos-worker-file-count: a leaf algebra meant to be driven by the fold worker.
//! Its single input arrives as `--in`. A file counts as 1; a directory (assumed
//! to hold only files, each containing a number — e.g. the per-child counts fold
//! assembles) returns their sum. The result, a blob holding the count, is left at
//! `/cas/out`. So folding a tree with this image totals the leaf files.
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
    let in_path = arg("in");

    let total: u64 = if Path::new(&in_path).is_dir() {
        eprintln!("file-count: summing child counts");
        caos(["get", &in_path])?; // expand the directory one level: a placeholder per child

        let mut total = 0u64;
        for child in entries(&in_path)? {
            caos(["get", path(&child)])?; // expand the placeholder to its bytes
            let text = fs::read_to_string(&child)
                .map_err(|e| format!("reading {}: {e}", child.display()))?;
            total += text
                .trim()
                .parse::<u64>()
                .map_err(|e| format!("parsing count in {}: {e}", child.display()))?;
        }
        total
    } else {
        eprintln!("file-count: a file counts as 1");
        1
    };

    let out = scratch("file-count")?.join("count");
    fs::write(&out, format!("{total}\n")).map_err(|e| format!("writing count: {e}"))?;
    caos(["put", path(&out), "/cas/out"])
}
