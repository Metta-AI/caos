//! caos-worker-fold: a recursive fold over a CAS tree, parameterized by two
//! image "functions":
//!   pre  — (optional) applied to `in` to produce the tree of children to fold.
//!          Omitted means the structural default: a tree's own children, and a
//!          file is a leaf with no children.
//!   post — applied to `in` together with `--children` (the tree of folded child
//!          results, under their original names) to produce this node's result.
//!   in   — the CAS path to fold over.
//!
//! So `fold` first decides this node's children (via `pre`, or structurally),
//! folds each with itself, then combines them with `post`. `pre`/`post` are
//! image refs — often curried (e.g. `pre` carrying the context it resolves
//! against) — and are threaded unchanged through the recursion. Like every
//! worker the applied images take their input as `--in` and leave the result at
//! `/cas/out`.
//!
//! Unlike the leaf workers it drives the compute server via `caos run` — both to
//! apply `pre`/`post` and to recurse — so it relies on `CAOS_COMPUTE_SERVER_URL`
//! (injected by the compute server) and learns its own image name, for the
//! recursive call, from `CAOS_FOLD_IMAGE`.

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, entries, file_name, path, read_arg, read_arg_opt, scratch, self_image,
};

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
    // `pre` is optional; `post` (the combining algebra, formerly `func`) is not.
    // Both arrive as blobs naming an image to apply.
    let pre = read_arg_opt("pre")?;
    let post = read_arg("post")?;
    let in_path = arg("in");

    // Decide this node's children: `pre`'s output tree if given, otherwise the
    // input's own children — and a plain file has none, so it's a leaf.
    let children = if let Some(pre) = &pre {
        eprintln!("fold: applying pre {pre} to {in_path}");
        caos(["run", pre, "/cas/pre", "--", &format!("--in={in_path}")])?;
        caos(["get", "/cas/pre"])?; // one level: a placeholder per child
        entries("/cas/pre")?
    } else if Path::new(&in_path).is_dir() {
        caos(["get", &in_path])?;
        entries(&in_path)?
    } else {
        Vec::new()
    };

    // Fold each child with this same (pre, post); collect the results by name.
    let work = scratch("folded")?;
    for (i, child) in children.iter().enumerate() {
        let name = file_name(child);
        let node = format!("/cas/c{i}");
        let in_arg = format!("--in={}", path(child));
        let post_arg = format!("--post={post}");
        if let Some(pre) = &pre {
            let pre_arg = format!("--pre={pre}");
            caos(["run", &fold_image, &node, "--", &pre_arg, &post_arg, &in_arg])?;
        } else {
            caos(["run", &fold_image, &node, "--", &post_arg, &in_arg])?;
        }
        // Symlink into the CAS so `caos put` reuses the result's recorded hash
        // (no content re-read) under the child's original name.
        symlink(&node, work.join(&name)).map_err(|e| format!("symlink {name}: {e}"))?;
        eprintln!("  folded {name} -> {node}");
    }

    // Assemble the folded children, then combine them with `post` over (`in`,
    // children). A leaf passes an empty children tree.
    caos(["put", path(&work), "/cas/children"])?;
    caos([
        "run",
        &post,
        "/cas/out",
        "--",
        &format!("--in={in_path}"),
        "--children=/cas/children",
    ])
}
