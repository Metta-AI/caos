//! Shared helpers for the Rust caos workers.
//!
//! A worker is a `/worker` program: `caos entrypoint` materializes the run's
//! arguments under `/cas/args` (one entry per `--name=value` arg `caos run`
//! passed), runs the worker, and on exit reads the hash of `/cas/out`. Every CAS
//! operation is a shell-out to the `caos` CLI — these helpers wrap the handful of
//! calls every worker repeats: fetching args, reading blobs, staging a result in
//! a scratch directory, and listing a fetched tree.
//!
//! Workers stage results by symlinking already-fetched `/cas/...` paths into a
//! scratch tree and `caos put`ting it; `caos put` resolves those symlinks to the
//! content's recorded hash, so nothing is re-read or re-uploaded. That's why a
//! worker needs no `cp`/coreutils — and so no shell in its image.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where `caos entrypoint` materializes this run's arguments.
pub const ARGS: &str = "/cas/args";

/// Absolute path of an argument under `/cas/args`.
pub fn arg(name: &str) -> String {
    format!("{ARGS}/{name}")
}

/// A worker's own image name, for the recursive `caos run` calls of workers that
/// reinvoke themselves: the value of environment variable `env`, or `default`
/// when it's unset or empty.
pub fn self_image(env: &str, default: &str) -> String {
    match std::env::var(env) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

/// Run `caos` with the given arguments, inheriting stdio; error on failure.
pub fn caos<const N: usize>(args: [&str; N]) -> Result<(), String> {
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
pub fn read_arg(name: &str) -> Result<String, String> {
    caos(["get", &arg(name)])?;
    let text = fs::read_to_string(arg(name)).map_err(|e| format!("reading {name}: {e}"))?;
    Ok(text.trim().to_string())
}

/// Like [`read_arg`], but `Ok(None)` if the argument wasn't passed.
pub fn read_arg_opt(name: &str) -> Result<Option<String>, String> {
    if Path::new(&arg(name)).exists() {
        read_arg(name).map(Some)
    } else {
        Ok(None)
    }
}

/// (Re)create an empty scratch directory under `/tmp` and return its path.
pub fn scratch(name: &str) -> Result<PathBuf, String> {
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
pub fn entries(dir: &str) -> Result<Vec<PathBuf>, String> {
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
pub fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// `&Path` as a `&str` for passing to `caos` (CAS paths are UTF-8).
pub fn path(p: &Path) -> &str {
    p.to_str().unwrap_or_default()
}
