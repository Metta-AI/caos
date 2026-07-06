//! caos-worker-fold: a structural fold (catamorphism) over a CAS tree,
//! parameterized by one image "function":
//!   post — applied to `in` together with `--children` (the tree of folded child
//!          results, under their original names) to produce this node's result.
//!   in   — the CAS path to fold over.
//!
//! A tree's children are its own entries; a file is a leaf with no children.
//! `fold` is one `map_then`: map itself (curried with `post`) over `in`'s
//! children, then combine them with `post` — so the whole body is a single
//! continuation and the worker exits immediately. `post` is an image ref —
//! often curried — threaded unchanged through the recursion. Like every worker
//! the applied image takes its input as `--in` (plus `--children`) and leaves
//! its result at `/cas/out`.
//!
//! Identical subtrees are memoized, so a fold is incremental in the changed
//! nodes. (The old `pre` parameter — a computed recursion set — is gone: a
//! worker that wants to fold something other than the tree's own children
//! builds that tree locally and folds it instead.)

use std::process::ExitCode;

use worker_common::{arg, caos_curry, map_then, read_arg, run_worker, std_image, Arg};

fn main() -> ExitCode {
    run_worker("fold", run)
}

fn run() -> Result<(), String> {
    // `post` (the combining algebra) arrives as a blob naming an image to apply.
    let post = read_arg("post")?;
    let in_path = arg("in");

    // Recurse by mapping this same image — with the same `post` bound — over the
    // children; then `post` combines (`in`, children). A leaf (a file) maps to
    // an empty children tree.
    let fold = caos_curry(&std_image("fold"), &[("post", Arg::Lit(&post))])?;
    map_then(&in_path, Some(&fold), Some(&post))
}
