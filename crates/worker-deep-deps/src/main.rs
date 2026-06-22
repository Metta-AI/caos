//! caos-worker-deep-deps: turn a flat, name-keyed package map into a DAG of
//! "deepened" nodes. The input `packages` tree holds one subtree per package,
//! each with a `DEPS` blob (dependency names, one per line). The output mirrors
//! it, but every node carries a `DEEP-DEPS` subtree of its recursively-deepened
//! direct deps (which themselves carry `DEEP-DEPS`).
//!
//! Incrementality comes entirely from CAOS call memoization — see the `--mode`
//! handlers below. This is a `/worker`: it reads its inputs from `/cas/args`,
//! shells out to the `caos` CLI for every CAS operation, and leaves its result
//! at `/cas/out`. It drives the compute server via `caos run` (both to recurse
//! and to apply the memoized boundary), learning its own image name — for those
//! recursive calls — from `CAOS_DEEP_DEPS_IMAGE`. Acyclic input only.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Where `caos entrypoint` materializes this run's arguments.
const ARGS: &str = "/cas/args";

/// Env var naming this worker's own image, used for the recursive `caos run`
/// calls. Defaults to the conventional name/tag as a plain docker image.
const SELF_IMAGE_ENV: &str = "CAOS_DEEP_DEPS_IMAGE";
const DEFAULT_SELF_IMAGE: &str = "docker://caos-worker-deep-deps:latest";

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
    // package). It arrives as a blob, so read it only if it was passed.
    match read_arg_opt("mode")?.as_deref() {
        None | Some("") => deepen_one(),
        Some("finishDeepening") => finish_deepening(),
        Some("all") => deepen_all(),
        Some(other) => Err(format!("unknown mode {other:?}")),
    }
}

/// Default mode: deepen the single package named by `--name`, looking it up in
/// the whole `--packages` map. Recurses on each direct dep, then hands off to
/// the memoized `finishDeepening` boundary. Because it takes the whole map it
/// re-runs on any edit — but that's cheap orchestration, not real recompute.
fn deepen_one() -> Result<(), String> {
    let name = read_arg("name")?;

    // Expand the map one level so the child exists, then this package fully so
    // its `DEPS` blob is readable.
    caos(["get", &arg("packages")])?;
    let pkg_dir = format!("{ARGS}/packages/{name}");
    caos(["get", "-r", &pkg_dir])?;

    // Deepen each direct dep with this same image, sharing results by hash: a
    // dep reached from two parents references the one deepened node.
    let work = scratch("deep-deps")?;
    for (i, dep) in deps_of(&pkg_dir)?.iter().enumerate() {
        let node = format!("/cas/d{i}");
        caos([
            "run",
            &self_image(),
            &node,
            "--",
            &format!("--packages={ARGS}/packages"),
            &format!("--name={dep}"),
        ])?;
        symlink(&node, work.join(dep)).map_err(|e| format!("symlink {dep}: {e}"))?;
    }
    caos(["put", path(&work), "/cas/deep-deps"])?;

    // Hand off to the content-keyed boundary (it never sees the whole map).
    caos([
        "run",
        &self_image(),
        "/cas/out",
        "--",
        "--mode=finishDeepening",
        &format!("--pkg={ARGS}/packages/{name}"),
        "--deep-deps=/cas/deep-deps",
    ])
}

/// The memoized boundary: build a node from a package's own files (minus `DEPS`)
/// plus a `DEEP-DEPS` subtree of its already-deepened direct deps. It never sees
/// the map, so its cache key is just this package and its subgraph — a hit
/// unless one of those moved. So real recompute is O(changed package + its
/// dependents).
fn finish_deepening() -> Result<(), String> {
    caos(["get", &arg("pkg")])?; // one level: enough to list the package's files

    let node = scratch("node")?;
    for entry in entries(&arg("pkg"))? {
        let name = file_name(&entry);
        if name == "DEPS" {
            continue; // replaced by DEEP-DEPS
        }
        symlink(&entry, node.join(&name)).map_err(|e| format!("symlink {name}: {e}"))?;
    }
    symlink(arg("deep-deps"), node.join("DEEP-DEPS")).map_err(|e| format!("symlink deps: {e}"))?;
    caos(["put", path(&node), "/cas/out"])
}

/// Top-level convenience: deepen every package into a tree `{name: node}`.
fn deepen_all() -> Result<(), String> {
    caos(["get", &arg("packages")])?;

    let work = scratch("all")?;
    for (i, pkg) in entries(&arg("packages"))?.iter().enumerate() {
        let name = file_name(pkg);
        let node = format!("/cas/a{i}");
        caos([
            "run",
            &self_image(),
            &node,
            "--",
            &format!("--packages={ARGS}/packages"),
            &format!("--name={name}"),
        ])?;
        symlink(&node, work.join(&name)).map_err(|e| format!("symlink {name}: {e}"))?;
    }
    caos(["put", path(&work), "/cas/out"])
}

// --- helpers ---------------------------------------------------------------

/// This worker's own image, for the recursive `caos run` calls.
fn self_image() -> String {
    std::env::var(SELF_IMAGE_ENV).unwrap_or_else(|_| DEFAULT_SELF_IMAGE.to_string())
}

/// Absolute path of an argument under `/cas/args`.
fn arg(name: &str) -> String {
    format!("{ARGS}/{name}")
}

/// Run `caos` with the given arguments, inheriting stdio; error on failure.
fn caos<const N: usize>(args: [&str; N]) -> Result<(), String> {
    let status = Command::new("caos")
        .args(args)
        .status()
        .map_err(|e| format!("running caos: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("caos {} exited with {status}", args.join(" ")))
    }
}

/// Fetch and read a blob argument as a trimmed string.
fn read_arg(name: &str) -> Result<String, String> {
    caos(["get", &arg(name)])?;
    let text = fs::read_to_string(arg(name)).map_err(|e| format!("reading {name}: {e}"))?;
    Ok(text.trim().to_string())
}

/// Like [`read_arg`], but `Ok(None)` if the argument wasn't passed.
fn read_arg_opt(name: &str) -> Result<Option<String>, String> {
    if Path::new(&arg(name)).exists() {
        read_arg(name).map(Some)
    } else {
        Ok(None)
    }
}

/// The non-empty dependency names listed in `<pkg_dir>/DEPS`, or none if absent.
fn deps_of(pkg_dir: &str) -> Result<Vec<String>, String> {
    let deps = format!("{pkg_dir}/DEPS");
    if !Path::new(&deps).is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&deps).map_err(|e| format!("reading DEPS: {e}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// (Re)create an empty scratch directory under `/tmp` and return its path.
fn scratch(name: &str) -> Result<PathBuf, String> {
    let dir = PathBuf::from(format!("/tmp/{name}"));
    if let Err(e) = fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("clearing {}: {e}", dir.display()));
        }
    }
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Child paths of `dir`, sorted for deterministic ordering.
fn entries(dir: &str) -> Result<Vec<PathBuf>, String> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("reading {dir}: {e}"))?
        .map(|e| {
            e.map(|e| e.path())
                .map_err(|e| format!("reading {dir}: {e}"))
        })
        .collect::<Result<_, _>>()?;
    paths.sort();
    Ok(paths)
}

/// The final path component of `p` as a string (entries never end in `..`).
fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// `&Path`/`&PathBuf` as a `&str` for passing to `caos` (CAS paths are UTF-8).
fn path(p: &Path) -> &str {
    p.to_str().unwrap_or_default()
}
