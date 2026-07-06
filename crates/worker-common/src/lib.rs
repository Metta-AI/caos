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
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Where `caos entrypoint` materializes this run's arguments.
pub const ARGS: &str = "/cas/args";

/// Absolute path of an argument under `/cas/args`.
pub fn arg(name: &str) -> String {
    format!("{ARGS}/{name}")
}

/// A built-in's image, referenced as a path into the standard-library tree the
/// server materialized at `/cas/std`. Pass the result to `caos run`/`caos curry`
/// like any image ref — `caos` resolves the recorded hash. Workers reach their
/// own image and other built-ins this way, so the binding rides in `std` (and
/// thus the cache key), not in env.
pub fn std_image(name: &str) -> String {
    format!("/cas/std/{name}")
}

/// A worker's `main`: run `run`, map its `Result` to an exit code, and prefix any
/// error with the worker's `name`. Every worker is `fn main() -> ExitCode {
/// worker_common::run_worker("name", run) }`.
pub fn run_worker(name: &str, run: fn() -> Result<(), String>) -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{name}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Run `caos` with the given arguments, inheriting stdio; error on failure.
pub fn caos<const N: usize>(args: [&str; N]) -> Result<(), String> {
    caos_argv(&args)
}

/// An argument value for `caos run`/`caos curry`. The two kinds serialize with
/// different operators — `--name=value` for a literal, `--name:@=value` for a
/// path — so the distinction is explicit, never sniffed from the value.
pub enum Arg<'a> {
    /// A literal string (e.g. a mode, or an image ref to bind).
    Lit(&'a str),
    /// A `/cas` path to reference (or, off-worker, a host path to ingest).
    Path(&'a str),
}

/// `caos run <image> <out> -- …` — run `image` over the given named arguments,
/// leaving its result at `out`.
pub fn caos_run(image: &str, out: &str, args: &[(&str, Arg)]) -> Result<(), String> {
    let argv = verb_argv("run", image, Some(out), args);
    caos_argv(&str_refs(&argv))
}

/// `caos curry <image> -- …` — bind the given named arguments to `image`,
/// returning a ref to the resulting curried image.
pub fn caos_curry(image: &str, args: &[(&str, Arg)]) -> Result<String, String> {
    let argv = verb_argv("curry", image, None, args);
    caos_capture(&str_refs(&argv))
}

/// Map-then: run `map` over each child of `input` (a CAS path), assemble the
/// results into a `children` tree under the original names, then record
/// `then(--in=<input>, --children=<children>)` as this worker's result at
/// `/cas/out`. A blob `input` has no children (a leaf), so `then` gets an empty
/// `children` tree. With no `then`, the children tree itself is the result;
/// with no `map`, `then(--in=<input>)` is a plain tail call. `map`/`then` are
/// image refs (a `/cas` path, a git/curry hash, or `docker://…`), usually
/// curried with whatever else they need.
///
/// This is a worker's *final act*: it produces `/cas/out`, so call it once, in
/// tail position. For now it is implemented in terms of blocking `caos run`
/// (children run one at a time, holding this worker's slot); the implementation
/// is moving into `caos run` itself, which will record the continuation for the
/// server to resolve — in parallel, with no worker held — after this worker
/// exits (see `design/map-then.md`).
pub fn map_then(input: &str, map: Option<&str>, then: Option<&str>) -> Result<(), String> {
    if map.is_none() && then.is_none() {
        return Err("map_then needs a map or a then image".to_string());
    }

    // Map phase: one folded child per entry of `input`, staged under its name.
    let children = if let Some(map) = map {
        let work = scratch("map-then")?;
        if Path::new(input).is_dir() {
            caos(["get", input])?; // one level: a placeholder per child
            for (i, child) in entries(input)?.iter().enumerate() {
                let node = format!("/cas/mt{i}");
                caos_run(map, &node, &[("in", Arg::Path(path(child)))])?;
                link(&node, work.join(file_name(child)))?;
            }
        }
        Some(work)
    } else {
        None
    };

    match (then, children) {
        // Combine: `then` over the original input and the mapped children.
        (Some(then), Some(work)) => {
            caos(["put", path(&work), "/cas/mt-children"])?;
            caos_run(
                then,
                "/cas/out",
                &[
                    ("in", Arg::Path(input)),
                    ("children", Arg::Path("/cas/mt-children")),
                ],
            )
        }
        // Tail call: no map ran, so no children argument.
        (Some(then), None) => caos_run(then, "/cas/out", &[("in", Arg::Path(input))]),
        // Pure map: the children tree is the result.
        (None, Some(work)) => caos(["put", path(&work), "/cas/out"]),
        (None, None) => unreachable!("checked above"),
    }
}

/// Build a `caos <verb> <image> [<out>] -- …` argument vector, serializing each
/// arg per its kind (literal `--k=v`, path `--k:@=v`).
fn verb_argv(verb: &str, image: &str, out: Option<&str>, args: &[(&str, Arg)]) -> Vec<String> {
    let mut argv = vec![verb.to_string(), image.to_string()];
    argv.extend(out.map(str::to_string));
    argv.push("--".to_string());
    argv.extend(args.iter().map(|(k, v)| match v {
        Arg::Lit(s) => format!("--{k}={s}"),
        Arg::Path(s) => format!("--{k}:@={s}"),
    }));
    argv
}

fn str_refs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// Run `caos`, inheriting stdio; error on failure. Slice form behind [`caos`].
fn caos_argv(args: &[&str]) -> Result<(), String> {
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

/// Run `caos`, capturing its stdout (stderr inherited) and returning it trimmed;
/// error on failure. For commands whose stdout is a result, e.g. `caos curry`.
fn caos_capture(args: &[&str]) -> Result<String, String> {
    let output = Command::new("caos")
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("running caos: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "caos {} exited with {}",
            args.join(" "),
            output.status
        ));
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("caos {} stdout not UTF-8: {e}", args.join(" ")))
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

/// Symlink `at` -> `target`, for staging an already-fetched CAS path into a
/// scratch tree before `caos put` (which resolves the link to the content's
/// recorded hash, so nothing is re-read).
pub fn link(target: impl AsRef<Path>, at: impl AsRef<Path>) -> Result<(), String> {
    let (target, at) = (target.as_ref(), at.as_ref());
    symlink(target, at)
        .map_err(|e| format!("symlink {} -> {}: {e}", at.display(), target.display()))
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
