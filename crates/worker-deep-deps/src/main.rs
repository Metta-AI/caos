//! caos-worker-deep-deps: turn a flat, name-keyed package map into a DAG of
//! "deepened" nodes. The input `packages` tree holds one subtree per package,
//! each with a `DEPS` blob (dependency names, one per line). The output mirrors
//! it, but every node carries a `DEEP-DEPS` subtree of its recursively-deepened
//! direct deps (which themselves carry `DEEP-DEPS`).
//!
//! It's written as a fold (`caos-worker-fold`) over the dependency graph, with
//! this same image supplying the fold's two functions — curried so the fold
//! treats them as plain images:
//!   * `--mode=resolve` is the fold's `pre`, curried with `--packages` (the whole
//!     map): given a package subtree as `--in`, it resolves that package's `DEPS`
//!     names to the dep subtrees to recurse into.
//!   * `--mode=finish` is the fold's `post`: given a package subtree as `--in`
//!     and its deepened deps as `--children`, it builds the node (the package's
//!     own files, minus `DEPS`, plus a `DEEP-DEPS` of the children).
//!
//! Incrementality comes entirely from CAOS call memoization. The driver and
//! `resolve` carry the whole map, so they re-run on any edit — cheap
//! orchestration. But `finish` (curried with nothing) is keyed only on a package
//! and its deepened subgraph, so real recompute is O(changed package + its
//! dependents). A dependency cycle re-enters the same fold `(image, args)` and is
//! caught by the compute server's run-cycle detection.
//!
//! This is a `/worker`: it reads inputs from `/cas/args`, shells out to `caos`
//! for every CAS operation, and leaves its result at `/cas/out`. It learns its
//! own image (to curry `resolve`/`finish`) from `CAOS_DEEP_DEPS_IMAGE` and the
//! fold image from `CAOS_FOLD_IMAGE`.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_stdout, entries, file_name, path, read_arg_opt, scratch, self_image, ARGS,
};

/// Env var naming this worker's own image, curried into the fold's `pre`/`post`.
const SELF_IMAGE_ENV: &str = "CAOS_DEEP_DEPS_IMAGE";
const DEFAULT_SELF_IMAGE: &str = "docker://caos-worker-deep-deps:latest";

/// Env var naming the fold worker's image, which the driver runs.
const FOLD_IMAGE_ENV: &str = "CAOS_FOLD_IMAGE";
const DEFAULT_FOLD_IMAGE: &str = "docker://caos-worker-fold:latest";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("deep-deps: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    // `--mode` is optional; omitting it is the simple public API (deepen one
    // package). The internal `resolve`/`finish` modes are reached via curry by
    // the driver, never passed by a caller directly.
    match read_arg_opt("mode")?.as_deref() {
        None | Some("") => deepen_one(),
        Some("all") => deepen_all(),
        Some("resolve") => resolve(),
        Some("finish") => finish(),
        Some(other) => Err(format!("unknown mode {other:?}")),
    }
}

/// Default mode: deepen the single package `--name` from the `--packages` map,
/// leaving its node at `/cas/out`.
fn deepen_one() -> Result<(), String> {
    let name = read_arg_opt("name")?.ok_or("deepen: --name is required")?;
    let fold = self_image(FOLD_IMAGE_ENV, DEFAULT_FOLD_IMAGE);
    let (pre, post) = fold_functions()?;

    caos(["get", &arg("packages")])?; // one level: a placeholder per package
    let in_pkg = format!("--in={ARGS}/packages/{name}");
    caos([
        "run",
        &fold,
        "/cas/out",
        "--",
        &format!("--pre={pre}"),
        &format!("--post={post}"),
        &in_pkg,
    ])
}

/// Top-level convenience: deepen every package into a tree `{name: node}`.
fn deepen_all() -> Result<(), String> {
    let fold = self_image(FOLD_IMAGE_ENV, DEFAULT_FOLD_IMAGE);
    let (pre, post) = fold_functions()?;

    caos(["get", &arg("packages")])?;
    let work = scratch("all")?;
    for (i, pkg) in entries(&arg("packages"))?.iter().enumerate() {
        let name = file_name(pkg);
        let node = format!("/cas/a{i}");
        caos([
            "run",
            &fold,
            &node,
            "--",
            &format!("--pre={pre}"),
            &format!("--post={post}"),
            &format!("--in={}", path(pkg)),
        ])?;
        symlink(&node, work.join(&name)).map_err(|e| format!("symlink {name}: {e}"))?;
    }
    caos(["put", path(&work), "/cas/out"])
}

/// Curry this image into the fold's `pre` (`resolve`, carrying the whole map) and
/// `post` (`finish`), returning refs to both. `resolve` carries `--packages` so
/// it can look up dep names; `finish` carries nothing, keeping it map-free.
fn fold_functions() -> Result<(String, String), String> {
    let me = self_image(SELF_IMAGE_ENV, DEFAULT_SELF_IMAGE);
    let pre = caos_stdout([
        "curry",
        &me,
        "--",
        "--mode=resolve",
        &format!("--packages={}", arg("packages")),
    ])?;
    let post = caos_stdout(["curry", &me, "--", "--mode=finish"])?;
    Ok((pre, post))
}

/// The fold's `pre`: given a package subtree as `--in` and the whole map as
/// `--packages` (curried), produce the tree of dep subtrees to recurse into —
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
        symlink(&target, work.join(&dep)).map_err(|e| format!("symlink {dep}: {e}"))?;
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
        symlink(&entry, node.join(&name)).map_err(|e| format!("symlink {name}: {e}"))?;
    }
    symlink(arg("children"), node.join("DEEP-DEPS")).map_err(|e| format!("symlink deps: {e}"))?;
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
