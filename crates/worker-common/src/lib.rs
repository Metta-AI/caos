//! Shared helpers for the Rust caos workers.
//!
//! A worker is a `/worker` program: the `caos` runner materializes the run's
//! arguments under `/cas/args` (one entry per `--name=value` arg the run request
//! passed), runs the worker, and on exit reads the hash of `/cas/out`. Every CAS
//! operation is a shell-out to the `caos` CLI — these helpers wrap the handful of
//! calls every worker repeats: fetching args, reading blobs, staging a result in
//! a scratch directory, and listing a fetched tree.
//!
//! Workers stage results by symlinking already-fetched `/cas/...` paths into a
//! scratch tree and `caos put`ting it; `caos put` resolves those symlinks to the
//! content's recorded hash, so nothing is re-read or re-uploaded. That's why a
//! worker needs no `cp`/coreutils — and so no shell in its image.
//!
//! What a worker curries and calls is an *ArgTree*, not an image (SPEC, "Work"):
//! currying takes an ArgTree and args and returns a new ArgTree, and its in-code
//! form is a curry node (`{base, args, .caos-curry}`). An image is just one arg
//! (the reserved `image`), so the *simplest* ArgTree is a bare image — which is
//! exactly what `own_image` hands you: an image ref, the value under
//! that one arg. `caos curry` binds args onto such a ref to build a richer
//! ArgTree; the `map`/`then`/`run` operands are ArgTree refs (a bare image, or a
//! curry node carrying an image plus other args).
//!
//! To recurse, a worker calls a new ArgTree built from itself. Two ways: curry
//! onto [`own_image`] (the bare base) and re-supply the args to carry — simple,
//! but every carried arg must be re-listed; or carry the whole ArgTree forward
//! with [`own_args_tree`] + [`caos_recurry`], unbinding only the args that
//! change. The latter is what a worker with mutable loop state wants — config
//! rides along untouched, and only the state is unbound and rebound.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Where the `caos` runner materializes this run's arguments.
pub const ARGS: &str = "/cas/args";

/// Where the `caos` runner drops the secrets this job was granted, one file per
/// secret (design/secrets.md). Read one with [`secret`].
pub const SECRETS: &str = "/secret";

/// The target triple a produced binary is built for: musl, so it is static and
/// runs on ANY base — the runner pool's image today, a scratch image tomorrow,
/// or (llm-stub) as a plain sidecar process in a test's own container. The arch
/// follows this worker's own build, which is the image's arch.
///
/// It lives here because two workers need the same answer: `worker-cargo`
/// defaults `--target` to it for `--cmd=build`, and `worker-rustc` is a wrapper
/// around exactly that call. A `.caos-expr` is static text and cannot compute a
/// triple, so a std entry built straight by cargo relies on this default.
pub const MUSL_TARGET: &str = if cfg!(target_arch = "aarch64") {
    "aarch64-unknown-linux-musl"
} else {
    "x86_64-unknown-linux-musl"
};

/// Absolute path of an argument under `/cas/args`.
pub fn arg(name: &str) -> String {
    format!("{ARGS}/{name}")
}

/// Read the granted secret `name` from `/secret/<name>` (design/secrets.md).
/// The server injects it out of band only when this worker's ArgTree is a
/// superset of one of the secret's readers, so a worker that isn't entitled
/// (or a rotated-away secret) gets a plain not-found error — which a tool that
/// needs the secret should surface as a failure, never a silent no-op. The
/// bytes are returned verbatim (no trimming): a token is used as-is.
pub fn secret(name: &str) -> Result<String, String> {
    let path = format!("{SECRETS}/{name}");
    fs::read_to_string(&path).map_err(|e| format!("reading secret {name}: {e}"))
}

/// This worker's *own* image ref — the request's reserved `image` args entry,
/// which a git image materializes as a tree at `/cas/args/image`. It's the
/// UNWRAPPED base image (never a curry node), so curry your args back onto it to
/// recurse by calling yourself — an ArgTree built from the exact image running,
/// needing no std lookup, for any git image (a rustc-built worker as much as a
/// builtin). Not for `docker://` workers — there the entry is a blob naming the
/// registry ref, and a path resolves to the recorded hash, not the ref.
///
/// Reaching for this to rebuild your whole ArgTree by hand? Prefer
/// [`own_args_tree`] + [`caos_recurry`], which carry every other arg forward.
pub fn own_image() -> String {
    arg("image")
}

/// This worker's *own* ArgTree — the git hash of the args tree the server
/// materialized at `/cas/args` (the `image` base plus every bound and call arg).
/// Unlike [`own_image`], which is just the base, this is the WHOLE ArgTree, so a
/// worker can recurse by currying it *forward* — [`caos_recurry`] — instead of
/// re-listing its config arg by arg (miss one and it silently vanishes next
/// round). Its per-call args (`in`, `result`, `children`, …) ride along too, so
/// `unbind` any that shouldn't persist into the next call.
pub fn own_args_tree() -> Result<String, String> {
    cas_hash(ARGS)
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

/// An argument value for `caos curry`. The two kinds serialize with different
/// operators — `--name=value` for a literal, `--name:@=value` for a path — so
/// the distinction is explicit, never sniffed from the value.
pub enum Arg<'a> {
    /// A literal string (e.g. a mode, or an arg-tree ref to bind).
    Lit(&'a str),
    /// A `/cas` path to reference (or, off-worker, a host path to ingest).
    Path(&'a str),
}

/// `caos curry <arg tree> -- …` — bind the given named arguments to `arg_tree`,
/// returning a ref to the resulting curried ArgTree. Currying is strict: it
/// refuses to rebind an already-bound name — use [`caos_recurry`] to release
/// one first.
pub fn caos_curry(arg_tree: &str, args: &[(&str, Arg)]) -> Result<String, String> {
    caos_recurry(arg_tree, &[], args)
}

/// `caos curry <arg tree> --unbind=… -- …` — carry `arg_tree` forward, dropping
/// each name in `unbind` so it can be rebound, then binding `args`; returns a ref
/// to the new ArgTree. This is the self-recurry primitive: pair it with
/// [`own_args_tree`] to change a few args and keep every other one untouched, e.g.
/// `caos_recurry(&own_args_tree()?, &["step"], &[("step", Arg::Path(&next))])`.
pub fn caos_recurry(
    arg_tree: &str,
    unbind: &[&str],
    args: &[(&str, Arg)],
) -> Result<String, String> {
    let mut argv = vec!["curry".to_string(), arg_tree.to_string()];
    argv.extend(unbind.iter().map(|name| format!("--unbind={name}")));
    argv.push("--".to_string());
    argv.extend(args.iter().map(|(k, v)| match v {
        Arg::Lit(s) => format!("--{k}={s}"),
        Arg::Path(s) => format!("--{k}:@={s}"),
    }));
    caos_capture(&str_refs(&argv))
}

/// Map-then: record a continuation over `input` (a CAS path) as this worker's
/// result at `/cas/out` — `caos map-then <input> -- --map=<map> --then=<then>`. The
/// *server* resolves it after this worker exits: `map` runs over each child of
/// `input` in parallel, the results are assembled into a `children` tree under
/// the original names, and `then(--in=<input>, --children=<children>)` produces
/// the final result — with no worker slot held anywhere in between (see
/// `design/map-then.md`). A blob `input` has no children (a leaf), so `then`
/// gets an empty `children` tree. With no `then`, the children tree itself is
/// the result; with no `map`, `then(--in=<input>)` is a plain tail call.
/// `map`/`then` are arg-tree refs (a `/cas` path, a git/curry hash, or
/// `docker://…`), usually curried with whatever else they need.
///
/// This is a worker's *final act*: it produces `/cas/out`, so call it once, in
/// tail position.
pub fn map_then(input: &str, map: Option<&str>, then: Option<&str>) -> Result<(), String> {
    if map.is_none() && then.is_none() {
        return Err("map_then needs a map or a then arg tree".to_string());
    }
    let mut argv: Vec<String> = vec!["map-then".into(), input.into(), "--".into()];
    if let Some(map) = map {
        argv.push(format!("--map={map}"));
    }
    if let Some(then) = then {
        argv.push(format!("--then={then}"));
    }
    caos_argv(&str_refs(&argv))
}

/// Run-then: the single-valued [`map_then`] — record a continuation over
/// `input` (a CAS path) as this worker's result at `/cas/out`: `caos run-then
/// <input> -- --run=<run> [--then=<then>]`. The *server* resolves it after this
/// worker exits: one sub-run `run(--in=<input>)` yields R, then
/// `then(--in=<input>, --result=<R>)` produces the final result — or R itself
/// with no `then`, a plain tail call to `run`. R may itself be a promise; the
/// server collapses it fully before `then` sees it. `run`/`then` are arg-tree refs
/// exactly as in [`map_then`], usually curried with whatever else they need
/// (e.g. a worker currying its own state into `then` to be called back with the
/// sub-run's result).
///
/// Like `map_then`, this is a worker's *final act*: it produces `/cas/out`, so
/// call it once, in tail position.
///
/// [`run_then_catching`] is the same call with a failing `run` turned into a
/// value for `then` instead of an error that propagates.
pub fn run_then(input: &str, run: &str, then: Option<&str>) -> Result<(), String> {
    run_then_inner(input, run, then, false)
}

/// [`run_then`] with `--catch`: if the sub-run FAILS, the server calls
/// `then(--in=<input>, --error=<blob>)` — the blob holding the failure text,
/// in place of `--result` — and the request succeeds. `then` is required (an
/// error needs a recipient), and the enclosing request is left uncached so a
/// retry genuinely retries.
///
/// For drivers that must outlive their callees. The agent loop is the case this
/// exists for: a tool that dies has to come back to the model as an `is_error`
/// tool_result it can read and react to, not take the whole turn down with it.
pub fn run_then_catching(input: &str, run: &str, then: &str) -> Result<(), String> {
    run_then_inner(input, run, Some(then), true)
}

fn run_then_inner(input: &str, run: &str, then: Option<&str>, catch: bool) -> Result<(), String> {
    let mut argv: Vec<String> = vec![
        "run-then".into(),
        input.into(),
        "--".into(),
        format!("--run={run}"),
    ];
    if let Some(then) = then {
        argv.push(format!("--then={then}"));
    }
    if catch {
        argv.push("--catch".into());
    }
    caos_argv(&str_refs(&argv))
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

/// A parsed git commit, as a worker sees one: the tree it snapshots, its
/// parents, its author's name, and its message. In caos a commit is a
/// first-class value — e.g. an agent conversation head, where the message is a
/// turn's text, the tree the workspace state, and the parent the previous
/// turn. The author name is how the agent harness tells its own turn commits
/// (author `caos-agent`) apart from everything else when walking a
/// conversation (see `design/agent-harness.md`).
pub struct Commit {
    pub tree: String,
    pub parents: Vec<String>,
    pub author: String,
    pub message: String,
}

/// The git hash recorded on a CAS path (`caos hash`) — e.g. a commit-valued
/// arg's own id, which becomes the `parent` of the commit minted from it.
pub fn cas_hash(cas_path: &str) -> Result<String, String> {
    caos_capture(&["hash", cas_path])
}

/// Fetch and parse the commit at a CAS path (e.g. a `--name:commit=` arg, which
/// materializes as a file holding the raw commit object). Walk the history by
/// hash from here: `caos get-hash <commit.tree> <path>` for the snapshot,
/// `get-hash` on a parent for the previous commit.
pub fn read_commit(cas_path: &str) -> Result<Commit, String> {
    caos(["get", cas_path])?;
    let text = fs::read_to_string(cas_path).map_err(|e| format!("reading {cas_path}: {e}"))?;
    parse_commit(&text)
}

/// Mint a commit — `{tree, parents, message}` — record it at `out` (usually
/// `/cas/out`, making `commit <hash>` this worker's result), and return its
/// hash. The author/committer identity and timestamp are fixed, so a commit is
/// a pure value: its hash depends only on the tree, parents, and message.
pub fn write_commit(
    tree: &str,
    parents: &[&str],
    message: &str,
    out: &str,
) -> Result<String, String> {
    write_commit_as(tree, parents, message, None, out)
}

/// [`write_commit`] with an explicit author: `(name, unix-seconds)` stamps the
/// commit with a real identity and wall-clock time (the agent harness stamps
/// step/turn commits `caos-agent` + now, so a retried turn is a *distinct*
/// commit). `None` keeps the fixed zero-timestamp identity — the pure-value
/// behavior every other caller wants.
pub fn write_commit_as(
    tree: &str,
    parents: &[&str],
    message: &str,
    author: Option<(&str, i64)>,
    out: &str,
) -> Result<String, String> {
    let (name, timestamp) = author.unwrap_or(("caos", 0));
    let mut text = format!("tree {tree}\n");
    for parent in parents {
        text += &format!("parent {parent}\n");
    }
    text += &format!(
        "author {name} <caos@caos> {timestamp} +0000\n\
         committer {name} <caos@caos> {timestamp} +0000\n\n"
    );
    text += message;
    if !message.ends_with('\n') {
        text.push('\n');
    }
    let file = scratch("commit")?.join("commit");
    fs::write(&file, text).map_err(|e| format!("writing {}: {e}", file.display()))?;
    caos_capture(&["put-commit", path(&file), out])
}

/// Parse a raw commit object: header lines (`tree`, `parent`, `author` — the
/// name only; the email/timestamp aren't surfaced) up to the first blank line,
/// then the message.
fn parse_commit(text: &str) -> Result<Commit, String> {
    let (headers, message) = text
        .split_once("\n\n")
        .ok_or_else(|| format!("malformed commit (no blank line): {text:?}"))?;
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = String::new();
    for line in headers.lines() {
        if let Some(hash) = line.strip_prefix("tree ") {
            tree = Some(hash.to_string());
        } else if let Some(hash) = line.strip_prefix("parent ") {
            parents.push(hash.to_string());
        } else if let Some(ident) = line.strip_prefix("author ") {
            // `author NAME <EMAIL> TS TZ` — keep just the name.
            author = ident
                .split_once(" <")
                .map(|(name, _)| name)
                .unwrap_or(ident)
                .to_string();
        }
    }
    Ok(Commit {
        tree: tree.ok_or_else(|| format!("commit has no tree line: {text:?}"))?,
        parents,
        author,
        message: message.to_string(),
    })
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
