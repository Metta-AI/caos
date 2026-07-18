//! caos-worker-file-count: counts the leaf files under `--in`, recursing with
//! itself through map-then. One image, three positions, told apart by the
//! arguments the server (or a caller) passes:
//!
//!   * `--in` a tree, no `--children` — the recursive case: record the
//!     continuation `{in, map: file-count, then: file-count}` and exit. The
//!     server counts each child (in parallel) and calls this image back with
//!     the results;
//!   * `--in` plus `--children` (the `then` position) — combine: the count is
//!     the sum of the child counts (each entry a number);
//!   * `--in` a file — a leaf: it counts as 1.
//!
//! The result, a blob holding the count, is left at `/cas/out`. It reaches its
//! own image at `/cas/args/image` — the request's reserved `image` entry — so
//! recursion needs no std lookup. `own_image` is the *unwrapped base*, though,
//! so when this worker ships as a runner-pool `curry(runner, bin)` the
//! recursion must rebind `bin` (the rgrep worker documents the same); with the
//! binding absent (a baked image) it's a no-op.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, entries, map_then, own_image, path, run_worker, scratch, Arg,
};

fn main() -> ExitCode {
    run_worker("file-count", run)
}

fn run() -> Result<(), String> {
    // `--in` and `--children` arrive as placeholders, so the type (and presence)
    // is readable without fetching content.
    let total: u64 = if Path::new(&arg("children")).exists() {
        eprintln!("file-count: summing child counts");
        sum_children()?
    } else if Path::new(&arg("in")).is_file() {
        eprintln!("file-count: a file counts as 1");
        1
    } else {
        // A tree with no counted children yet: recurse. Tail call — the
        // continuation is this worker's result.
        eprintln!("file-count: recursing over the tree's children");
        let me = recur_image()?;
        return map_then(&arg("in"), Some(&me), Some(&me));
    };

    let out = scratch("file-count")?.join("count");
    fs::write(&out, format!("{total}\n")).map_err(|e| format!("writing count: {e}"))?;
    caos(["put", path(&out), "/cas/out"])
}

/// The image to recurse with: our own (unwrapped) image, rebinding the
/// runner-pool `bin` when we ship as a curry so children re-exec this binary.
/// A no-op when there's no `bin` (a baked image already carries `/worker`).
fn recur_image() -> Result<String, String> {
    let me = own_image();
    let bin = arg("bin");
    if Path::new(&bin).exists() {
        caos_curry(&me, &[("bin", Arg::Path(&bin))])
    } else {
        Ok(me)
    }
}

/// Sum the counts in the `--children` tree (one numeric blob per child; an
/// empty tree — a childless directory — sums to 0).
fn sum_children() -> Result<u64, String> {
    let children = arg("children");
    caos(["get", &children])?; // expand the directory one level: a placeholder per child

    let mut total = 0u64;
    for child in entries(&children)? {
        caos(["get", path(&child)])?; // expand the placeholder to its bytes
        let text =
            fs::read_to_string(&child).map_err(|e| format!("reading {}: {e}", child.display()))?;
        total += text
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("parsing count in {}: {e}", child.display()))?;
    }
    Ok(total)
}
