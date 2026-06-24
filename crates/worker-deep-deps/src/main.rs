//! caos-worker-deep-deps: turn a flat, name-keyed package map into a DAG of
//! "deepened" nodes. The input `packages` tree holds one subtree per package,
//! each with a `DEPS` blob (dependency names, one per line). The output mirrors
//! it, but every node carries a `DEEP-DEPS` subtree of its recursively-deepened
//! direct deps (which themselves carry `DEEP-DEPS`).
//!
//! It's a fold (`caos-worker-fold`) over the dependency graph, with this same
//! image supplying the fold's two functions — curried so the fold treats them as
//! plain images (see `fold_functions`):
//!   * `resolve` is the fold's `pre`, curried with the whole map: given a package
//!     as `--in`, it resolves that package's `DEPS` names to the dep subtrees to
//!     recurse into.
//!   * `finish` is the fold's `post`: given a package as `--in` and its deepened
//!     deps as `--children`, it builds the node.
//!
//! Incrementality comes from CAOS call memoization. The driver and `resolve`
//! carry the whole map, so they re-run on any edit — cheap orchestration. But
//! `finish` (curried with nothing) is keyed only on a package and its deepened
//! subgraph, so real recompute is O(changed package + its dependents). A cycle
//! re-enters the same fold `(image, args)` and is caught by the server's
//! run-cycle detection.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, caos_run, entries, file_name, link, path, read_arg_opt, run_worker,
    scratch, std_image, Arg, ARGS,
};

fn main() -> ExitCode {
    run_worker("deep-deps", run)
}

fn run() -> Result<(), String> {
    // `--mode` is optional; omitting it is the simple public API (deepen one
    // package). The internal `resolve`/`finish` modes are reached only via curry
    // by the driver, never passed by a caller directly.
    match read_arg_opt("mode")?.as_deref() {
        None | Some("") => deepen_one(),
        Some("all") => deepen_all(),
        Some("resolve") => resolve(),
        Some("finish") => finish(),
        Some(other) => Err(format!("unknown mode {other:?}")),
    }
}

// ---- the fold algebra: how one package becomes a deepened node ----------------

/// The fold's `pre`: given a package as `--in` and the whole map as `--packages`
/// (curried in), produce the tree of dep subtrees to recurse into —
/// `{dep: <map[dep] subtree>}`, one per name in the package's `DEPS`. Sharing is
/// by hash, so a dep reached from two parents is one node.
fn resolve() -> Result<(), String> {
    caos(["get", &arg("packages")])?; // one level: a placeholder per package

    let work = scratch("resolve")?;
    for dep in deps_of(&arg("in"))? {
        let target = format!("{ARGS}/packages/{dep}");
        if !Path::new(&target).exists() {
            return Err(format!("dependency {dep:?} is not in the package map"));
        }
        link(&target, work.join(&dep))?;
    }
    caos(["put", path(&work), "/cas/out"])
}

/// The fold's `post`: build a node from a package's own files (minus `DEPS`) plus
/// a `DEEP-DEPS` of its already-deepened deps (`--children`). Curried with
/// nothing, so its cache key is just this package and its subgraph.
fn finish() -> Result<(), String> {
    caos(["get", &arg("in")])?; // one level: enough to list the package's files

    let node = scratch("node")?;
    for entry in entries(&arg("in"))? {
        let name = file_name(&entry);
        if name == "DEPS" {
            continue; // replaced by DEEP-DEPS
        }
        link(&entry, node.join(&name))?;
    }
    link(arg("children"), node.join("DEEP-DEPS"))?;
    caos(["put", path(&node), "/cas/out"])
}

/// The non-empty dependency names in `<pkg_dir>/DEPS`, or none if it's absent.
fn deps_of(pkg_dir: &str) -> Result<Vec<String>, String> {
    caos(["get", pkg_dir])?; // one level: the package's `DEPS` placeholder appears
    let deps = format!("{pkg_dir}/DEPS");
    if !Path::new(&deps).exists() {
        return Ok(Vec::new());
    }
    caos(["get", &deps])?; // expand the blob so it's readable
    let text = fs::read_to_string(&deps).map_err(|e| format!("reading DEPS: {e}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

// ---- the driver: fold the algebra over the package map -----------------------

/// Default mode: deepen the single package `--name` from the `--packages` map,
/// leaving its node at `/cas/out`.
fn deepen_one() -> Result<(), String> {
    let name = read_arg_opt("name")?.ok_or("deepen: --name is required")?;
    caos(["get", &arg("packages")])?; // one level: a placeholder per package
    let (pre, post) = fold_functions()?;
    let in_pkg = format!("{ARGS}/packages/{name}");
    caos_run(
        &fold_image(),
        "/cas/out",
        &[
            ("pre", Arg::Lit(&pre)),
            ("post", Arg::Lit(&post)),
            ("in", Arg::Path(&in_pkg)),
        ],
    )
}

/// Top-level convenience: deepen every package into a tree `{name: node}`.
fn deepen_all() -> Result<(), String> {
    caos(["get", &arg("packages")])?;
    let (pre, post) = fold_functions()?;
    let work = scratch("all")?;
    for (i, pkg) in entries(&arg("packages"))?.iter().enumerate() {
        let node = format!("/cas/a{i}");
        caos_run(
            &fold_image(),
            &node,
            &[
                ("pre", Arg::Lit(&pre)),
                ("post", Arg::Lit(&post)),
                ("in", Arg::Path(path(pkg))),
            ],
        )?;
        link(&node, work.join(file_name(pkg)))?;
    }
    caos(["put", path(&work), "/cas/out"])
}

/// Curry this image into the fold's `pre` (`resolve`, carrying the whole map) and
/// `post` (`finish`). Currying the map into `pre` is what keeps it out of
/// `finish`'s cache key.
fn fold_functions() -> Result<(String, String), String> {
    let pre = caos_curry(
        &me(),
        &[
            ("mode", Arg::Lit("resolve")),
            ("packages", Arg::Path(&arg("packages"))),
        ],
    )?;
    let post = caos_curry(&me(), &[("mode", Arg::Lit("finish"))])?;
    Ok((pre, post))
}

/// This image, for currying `resolve`/`finish` — the built-in `/cas/std/deep-deps`.
fn me() -> String {
    std_image("deep-deps")
}

/// The fold worker's image, which the driver runs — the built-in `/cas/std/fold`.
fn fold_image() -> String {
    std_image("fold")
}
