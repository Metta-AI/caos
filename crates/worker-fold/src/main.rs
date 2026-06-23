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
//! Unlike the leaf workers it drives the server via `caos run` — both to
//! apply `pre`/`post` and to recurse — so it relies on `CAOS_SERVER_URL`
//! (injected by the server) and reaches its own image, for the recursive call,
//! as the built-in `/cas/std/fold`.

use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_run, entries, file_name, link, path, read_arg, read_arg_opt, run_worker,
    scratch, std_image,
};

fn main() -> ExitCode {
    run_worker("fold", run)
}

fn run() -> Result<(), String> {
    let fold_image = std_image("fold");
    // `pre` is optional; `post` (the combining algebra, formerly `func`) is not.
    // Both arrive as blobs naming an image to apply.
    let pre = read_arg_opt("pre")?;
    let post = read_arg("post")?;
    let in_path = arg("in");

    // Decide this node's children: `pre`'s output tree if given, otherwise the
    // input's own children — and a plain file has none, so it's a leaf.
    let children = if let Some(pre) = &pre {
        eprintln!("fold: applying pre {pre} to {in_path}");
        caos_run(pre, "/cas/pre", &[("in", &in_path)])?;
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
        // Thread pre (if any) and post through unchanged; recurse on the child.
        let mut fold_args = vec![("post", post.as_str()), ("in", path(child))];
        if let Some(pre) = &pre {
            fold_args.insert(0, ("pre", pre));
        }
        caos_run(&fold_image, &node, &fold_args)?;
        // Link into the CAS so `caos put` reuses the result's recorded hash (no
        // content re-read) under the child's original name.
        link(&node, work.join(&name))?;
        eprintln!("  folded {name} -> {node}");
    }

    // Assemble the folded children, then combine them with `post` over (`in`,
    // children). A leaf passes an empty children tree.
    caos(["put", path(&work), "/cas/children"])?;
    caos_run(&post, "/cas/out", &[("in", &in_path), ("children", "/cas/children")])
}
