//! caos-worker-deep-deps: turn a flat, name-keyed package map into a DAG of
//! "deepened" nodes. The input `packages` tree holds one subtree per package,
//! each with a `DEPS` blob (dependency names, one per line). The output mirrors
//! it, but every node carries a `DEEP-DEPS` subtree of its recursively-deepened
//! direct deps (which themselves carry `DEEP-DEPS`).
//!
//! It recurses through `map_then`, with this same image on both sides of the
//! continuation:
//!   * `deepen` resolves a package's `DEPS` names against the map (pure CAS
//!     linking — no sub-runs), then maps *itself* over the resolved dep tree
//!     and finishes with `finish`;
//!   * `finish` builds the node from the package (curried in as `--pkg`) plus
//!     its deepened deps (`--children`).
//!
//! Incrementality comes from CAOS call memoization. `deepen` is curried with
//! the whole map, so it re-runs on any edit — cheap orchestration. But `finish`
//! is keyed only on a package and its deepened subgraph, so real recompute is
//! O(changed package + its dependents). A cycle re-enters the same `deepen`
//! request and is caught by the server's run-cycle detection.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, entries, file_name, link, map_then, own_image, path, read_arg_opt,
    run_worker, scratch, Arg, ARGS,
};

fn main() -> ExitCode {
    run_worker("deep-deps", run)
}

fn run() -> Result<(), String> {
    // `--mode` is optional; omitting it is the simple public API (deepen one
    // package by `--name`). The internal `deepen`/`finish` modes are reached only
    // via curry by the driver, never passed by a caller directly.
    match read_arg_opt("mode")?.as_deref() {
        None | Some("") => deepen_named(),
        Some("all") => deepen_all(),
        Some("deepen") => deepen(&arg("in")),
        Some("finish") => finish(),
        Some(other) => Err(format!("unknown mode {other:?}")),
    }
}

/// Deepen the package at `pkg` (a subtree of the `--packages` map): resolve its
/// `DEPS` names to the dep subtrees (pure linking, the old `resolve` step), then
/// map `deepen` over them and `finish` the node. Sharing is by hash, so a dep
/// reached from two parents is one node — and one memoized computation.
fn deepen(pkg: &str) -> Result<(), String> {
    caos(["get", &arg("packages")])?; // one level: a placeholder per package

    let work = scratch("deps")?;
    for dep in deps_of(pkg)? {
        let target = format!("{ARGS}/packages/{dep}");
        if !Path::new(&target).exists() {
            return Err(format!("dependency {dep:?} is not in the package map"));
        }
        link(&target, work.join(&dep))?;
    }
    caos(["put", path(&work), "/cas/deps"])?;

    // Recurse on each dep with this same image as the map; finish with the node
    // builder, which needs the *package* (the mapped-over tree is the deps), so
    // it rides in by curry.
    let finish = caos_curry(
        &me(),
        &[("mode", Arg::Lit("finish")), ("pkg", Arg::Path(pkg))],
    )?;
    map_then("/cas/deps", Some(&deepen_image()?), Some(&finish))
}

/// Build a node from a package's own files (minus `DEPS`) plus a `DEEP-DEPS` of
/// its already-deepened deps (`--children`). The package arrives curried as
/// `--pkg` (the call's `--in` is the resolved deps tree, which the node doesn't
/// use). Curried with nothing else, so its cache key is just this package and
/// its subgraph.
fn finish() -> Result<(), String> {
    caos(["get", &arg("pkg")])?; // one level: enough to list the package's files

    let node = scratch("node")?;
    for entry in entries(&arg("pkg"))? {
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

// ---- the drivers ---------------------------------------------------------------

/// Default mode: deepen the single package `--name` from the `--packages` map.
fn deepen_named() -> Result<(), String> {
    let name = read_arg_opt("name")?.ok_or("deepen: --name is required")?;
    caos(["get", &arg("packages")])?; // one level: a placeholder per package
    deepen(&format!("{ARGS}/packages/{name}"))
}

/// Top-level convenience: deepen every package into a tree `{name: node}` — a
/// pure map over the package map (no `then`: the mapped tree is the result).
fn deepen_all() -> Result<(), String> {
    caos(["get", &arg("packages")])?;
    map_then(&arg("packages"), Some(&deepen_image()?), None)
}

/// Curry this image into the recursive `deepen` step, carrying the whole map.
/// Currying the map into `deepen` (not `finish`) is what keeps it out of
/// `finish`'s cache key.
fn deepen_image() -> Result<String, String> {
    caos_curry(
        &me(),
        &[
            ("mode", Arg::Lit("deepen")),
            ("packages", Arg::Path(&arg("packages"))),
        ],
    )
}

/// This image, for currying `deepen`/`finish` — its own image from the
/// request's reserved `image` args entry, so recursion needs no std lookup.
fn me() -> String {
    own_image()
}
