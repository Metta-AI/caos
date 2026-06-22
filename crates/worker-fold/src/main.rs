//! caos-worker-fold: a recursive "fold" — a catamorphism over a CAS tree. Two
//! args:
//!   func — the worker image to apply (the "algebra"), a literal value
//!   in   — the file or tree to fold over, a CAS path
//! Given a file it runs `func` on it. Given a tree it folds each child with
//! itself (the same `func`), assembles the results into a tree under the original
//! child names, then runs `func` on that tree. Like every worker, the applied
//! image takes its single input as `--in`; the result is left at `/cas/out`.
//!
//! Unlike the leaf workers it drives the compute server via `caos run` — both to
//! apply `func` and to recurse — so it relies on `CAOS_COMPUTE_SERVER_URL`
//! (injected by the compute server) and learns its own image name, for the
//! recursive call, from `CAOS_FOLD_IMAGE`.

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{arg, caos, entries, file_name, path, read_arg, scratch, self_image};

/// Env var naming this worker's own image, used for the recursive `caos run`
/// calls. Defaults to the conventional name/tag as a plain docker image.
const SELF_IMAGE_ENV: &str = "CAOS_FOLD_IMAGE";
const DEFAULT_SELF_IMAGE: &str = "docker://caos-worker-fold:latest";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("fold: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let fold_image = self_image(SELF_IMAGE_ENV, DEFAULT_SELF_IMAGE);
    // The function to apply is a blob arg: expand the placeholder and read it.
    let func = read_arg("func")?;
    let in_path = arg("in");

    if Path::new(&in_path).is_dir() {
        eprintln!("fold: input is a tree; folding its children with {func}");
        caos(["get", &in_path])?; // expand the tree one level: a placeholder per child

        let work = scratch("folded")?;
        for (i, child) in entries(&in_path)?.iter().enumerate() {
            let name = file_name(child);
            let node = format!("/cas/c{i}");
            // Fold this child with the same function; its result lands at <node>.
            caos([
                "run",
                &fold_image,
                &node,
                "--",
                &format!("--func={func}"),
                &format!("--in={}", path(child)),
            ])?;
            // Symlink into the CAS so `caos put` reuses the result's recorded
            // hash (no content re-read) under the child's original name.
            symlink(&node, work.join(&name)).map_err(|e| format!("symlink {name}: {e}"))?;
            eprintln!("  folded {name} -> {node}");
        }

        // Assemble the folded children into a tree, then apply the function.
        caos(["put", path(&work), "/cas/folded"])?;
        caos(["run", &func, "/cas/out", "--", "--in=/cas/folded"])
    } else {
        eprintln!("fold: input is a file; applying {func}");
        caos(["run", &func, "/cas/out", "--", &format!("--in={in_path}")])
    }
}
