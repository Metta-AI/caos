//! Bounded shell execution over a writable projection of an input Git tree.
//! Input: {tree, cmd, paths, cwd?}. Paths are relative to the input root,
//! independent of cwd. Commit entries become directories and retain their
//! provenance through ordinary file operations; caos put restores the boundaries.
//! Result: {tree, exit, stdout, stderr, denied?}. Command failures are values.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use worker_common::{arg, caos, cas_hash, link, path, run_worker, scratch, ARGS};

/// Keep at most this many bytes (the tail) of each captured stream.
const STREAM_CAP: usize = 100_000;

fn main() -> ExitCode {
    run_worker("bash-tool", run)
}

fn run() -> Result<(), String> {
    // Locate the input: an `in` tree ({tree, cmd, paths} — how a run-then
    // sub-run passes it), or the three direct args under /cas/args.
    let base = if Path::new(&arg("in")).exists() {
        caos(["get", &arg("in")])?;
        arg("in")
    } else {
        ARGS.to_string()
    };
    let cmd = read_blob(&format!("{base}/cmd"))?;
    let paths = read_paths(&format!("{base}/paths"))?;
    let tree = format!("{base}/tree");
    if !Path::new(&tree).exists() {
        return Err(format!("no source tree at {tree}"));
    }
    let work = scratch("work")?;
    let hash = cas_hash(&tree)?;
    let mut checkout = vec!["checkout".to_string(), hash, path(&work).to_string()];
    checkout.extend(paths);
    worker_common::caos_argv(&checkout.iter().map(String::as_str).collect::<Vec<_>>())?;
    let cwd = if Path::new(&format!("{base}/cwd")).exists() {
        read_blob(&format!("{base}/cwd"))?
    } else {
        ".".to_string()
    };
    if Path::new(&cwd).is_absolute()
        || Path::new(&cwd)
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("cwd must be a conversation-relative directory".into());
    }
    let cwd = work
        .join(cwd)
        .canonicalize()
        .map_err(|e| format!("cwd: {e}"))?;
    if !cwd.starts_with(&work) {
        return Err("cwd leaves the writable tree".into());
    }

    // Run the command as this (already unprivileged) worker, cwd the tree root.
    let out = Command::new("/bin/sh")
        .args(["-c", &cmd])
        .current_dir(&cwd)
        .output()
        .map_err(|e| format!("running /bin/sh: {e}"))?;
    let exit = exit_code(&out.status);
    let stderr_text = String::from_utf8_lossy(&out.stderr).into_owned();
    let denied = scan_denied(&stderr_text, &work);

    // Stage the working tree back: `caos put` resolves the placeholder
    // symlinks to their recorded hashes (nothing untouched is re-read), and
    // stores every real file/dir the command left behind.
    caos(["put", path(&work), "/cas/newtree"])?;

    // Assemble the result value.
    let res = scratch("result")?;
    fs::write(res.join("exit"), format!("{exit}\n")).map_err(|e| format!("writing exit: {e}"))?;
    fs::write(res.join("stdout"), tail(&out.stdout)).map_err(|e| format!("writing stdout: {e}"))?;
    fs::write(res.join("stderr"), tail(&out.stderr)).map_err(|e| format!("writing stderr: {e}"))?;
    if !denied.is_empty() {
        let listing: Vec<&str> = denied.iter().map(String::as_str).collect();
        fs::write(res.join("denied"), listing.join("\n") + "\n")
            .map_err(|e| format!("writing denied: {e}"))?;
    }
    link("/cas/newtree", res.join("tree"))?;
    caos(["put", path(&res), "/cas/out"])
}

/// Fetch and read a blob at a CAS path.
fn read_blob(cas_path: &str) -> Result<String, String> {
    caos(["get", cas_path])?;
    fs::read_to_string(cas_path).map_err(|e| format!("reading {cas_path}: {e}"))
}

/// The declared paths: newline-separated in the `paths` blob, absent or empty
/// meaning none. Entries that aren't plain relative paths (absolute, `..`) are
/// skipped — the command then hits the placeholder and the `denied` hint
/// steers the retry.
fn read_paths(cas_path: &str) -> Result<Vec<String>, String> {
    if !Path::new(cas_path).exists() {
        return Ok(Vec::new());
    }
    let text = read_blob(cas_path)?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            *l == "."
                || (!l.starts_with('/')
                    && Path::new(l)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))))
        })
        .map(|l| l.trim_end_matches('/').to_string())
        .collect())
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(-1)
}

/// The tail of a captured stream, capped at [`STREAM_CAP`] bytes (with a
/// marker so a truncated stream is recognizable as such).
fn tail(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() <= STREAM_CAP {
        return bytes.to_vec();
    }
    let mut out = b"[... truncated ...]\n".to_vec();
    out.extend_from_slice(&bytes[bytes.len() - STREAM_CAP..]);
    out
}

/// Scan stderr for permission-denied complaints and collect the mentioned
/// paths that resolve to unmaterialized placeholders in the working tree —
/// the structured "retry with these in `paths`" hint. Tokens on offending
/// lines are tried as conversation-relative (or work-tree-absolute) paths; one
/// counts if its resolution crosses a placeholder symlink.
fn scan_denied(stderr: &str, work: &Path) -> BTreeSet<String> {
    let mut hits = BTreeSet::new();
    for line in stderr.lines() {
        if !line.to_ascii_lowercase().contains("permission denied") {
            continue;
        }
        for raw in line.split([' ', '\t', '\'', '"', '`']) {
            let tok = raw.trim_matches([':', ',', ';', '(', ')']);
            let rel = match tok.strip_prefix('/') {
                // An absolute path only counts inside the working tree.
                Some(_) => match Path::new(tok).strip_prefix(work) {
                    Ok(rel) => rel.to_string_lossy().into_owned(),
                    Err(_) => continue,
                },
                None => tok.trim_start_matches("./").to_string(),
            };
            if !rel.is_empty() && crosses_placeholder(work, &rel) {
                hits.insert(rel);
            }
        }
    }
    hits
}

/// Whether resolving `rel` from the work root crosses a placeholder symlink
/// (a link into `/cas` — the symlinks checkout creates for unloaded
/// nodes; git symlinks it recreates point elsewhere).
fn crosses_placeholder(work: &Path, rel: &str) -> bool {
    let cas = Path::new(ARGS).parent().unwrap_or(Path::new("/cas"));
    let mut cur = work.to_path_buf();
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." {
            return false;
        }
        cur.push(comp);
        let Ok(meta) = fs::symlink_metadata(&cur) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return fs::read_link(&cur)
                .map(|dest| dest.starts_with(cas))
                .unwrap_or(false);
        }
    }
    false
}
