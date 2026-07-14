//! caos-worker-bash-tool: the agent harness's *bounded* bash tool (see
//! design/agent-harness.md). Input is `{tree, cmd, paths}` — a workspace tree,
//! a shell command, and the workspace-relative paths the command intends to
//! touch. Only the declared paths are materialized; the whole tree never is.
//! The command runs via `/bin/sh -c` in a mirror of the workspace where every
//! *undeclared* entry is a symlink to its owner-only `/cas` placeholder, so a
//! touch of one fails loudly with EACCES — and the result carries a structured
//! retry hint (`denied`) naming the placeholder paths the stderr mentions.
//!
//! The input arrives either as one `in` tree argument (`{tree, cmd, paths}` —
//! the shape a run-then sub-run passes) or as the three direct arguments
//! `--tree`/`--cmd`/`--paths` (convenient for `caos-cli run`). `paths` is a
//! blob of newline-separated relative paths, and may be absent or empty.
//!
//! **Any command outcome is a value, never a worker error** — a failing
//! command is something the model must see and react to. The result is a tree:
//!
//! ```text
//! exit    blob  the command's exit code, decimal (128+signal if killed)
//! stdout  blob  captured stdout, the last 100KB
//! stderr  blob  captured stderr, the last 100KB
//! denied  blob  (only when detected) unmaterialized paths the command
//!               touched, one per line — retry with them added to `paths`
//! tree    tree  the workspace after the command: real files staged back,
//!               untouched placeholders round-tripped by their recorded hash
//! ```
//!
//! Only infrastructure failures (a fetch failing, the staging put failing)
//! error the run.
//!
//! Known limitation: the CAS materializes blobs without git's executable bit,
//! so a round-trip through this tool drops the exec bit on files under
//! *declared* paths (undeclared subtrees round-trip untouched, by hash).

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use worker_common::{arg, caos, entries, file_name, link, path, run_worker, scratch, ARGS};

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
        return Err(format!("no workspace tree at {tree}"));
    }
    caos(["get", &tree])?; // the root's entries become visible (as placeholders)

    // Materialize exactly the declared paths, nothing else.
    for p in &paths {
        materialize_declared(&tree, p)?;
    }

    // Mirror the workspace into a writable working tree: loaded content as real
    // (rw) files/dirs, unloaded placeholders as symlinks into /cas.
    let work = scratch("work")?;
    mirror(Path::new(&tree), &work)?;

    // Run the command as this (already unprivileged) worker, cwd the tree root.
    let out = Command::new("/bin/sh")
        .args(["-c", &cmd])
        .current_dir(&work)
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
            !l.starts_with('/')
                && Path::new(l)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
        })
        .map(|l| l.trim_end_matches('/').to_string())
        .collect())
}

/// Materialize one declared path inside the (already one-level-loaded) tree:
/// each intermediate directory one level, then the leaf recursively — so a
/// declared file costs its ancestor trees plus itself, and a declared
/// directory its whole subtree, never anything beside them. A path that
/// doesn't exist (the command may be about to create it) or descends into a
/// blob is left alone.
fn materialize_declared(tree: &str, rel: &str) -> Result<(), String> {
    let mut cur = PathBuf::from(tree);
    let comps: Vec<&str> = rel.split('/').collect();
    for (i, comp) in comps.iter().enumerate() {
        cur.push(comp);
        let Ok(meta) = fs::symlink_metadata(&cur) else {
            return Ok(()); // no such entry — nothing to load
        };
        if meta.file_type().is_symlink() {
            return Ok(()); // a git symlink is already fully materialized
        }
        if i + 1 == comps.len() {
            caos(["get", "-r", path(&cur)])?; // the leaf, in full
        } else if meta.is_dir() {
            caos(["get", path(&cur)])?; // an ancestor, one level
        } else {
            return Ok(()); // path descends into a blob — nothing to do
        }
    }
    Ok(())
}

/// Whether a CAS node has been fetched (world-readable) as opposed to an
/// owner-only placeholder — the same mode convention the client uses.
fn is_loaded(meta: &fs::Metadata) -> bool {
    meta.permissions().mode() & 0o044 != 0
}

/// Mirror the (partially loaded) CAS tree at `src` into the writable working
/// tree at `dst`: loaded directories become real rw directories (recursed),
/// loaded blobs writable copies, git symlinks are recreated as-is, and
/// unloaded placeholders become symlinks to their `/cas` node — unreadable to
/// the worker (EACCES on touch) and resolved back to their recorded hash by
/// `caos put` when staging.
fn mirror(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in entries(path(src))? {
        let target = dst.join(file_name(&entry));
        let meta =
            fs::symlink_metadata(&entry).map_err(|e| format!("{}: {e}", entry.display()))?;
        if meta.file_type().is_symlink() {
            let dest = fs::read_link(&entry).map_err(|e| format!("{}: {e}", entry.display()))?;
            symlink(&dest, &target)
                .map_err(|e| format!("linking {}: {e}", target.display()))?;
        } else if !is_loaded(&meta) {
            link(&entry, &target)?;
        } else if meta.is_dir() {
            fs::create_dir(&target).map_err(|e| format!("creating {}: {e}", target.display()))?;
            mirror(&entry, &target)?;
        } else {
            fs::copy(&entry, &target).map_err(|e| format!("copying {}: {e}", entry.display()))?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644))
                .map_err(|e| format!("chmod {}: {e}", target.display()))?;
        }
    }
    Ok(())
}

/// The command's exit code — or 128+signal when it died to one, the shell
/// convention the model will recognize.
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
/// lines are tried as workspace-relative (or work-tree-absolute) paths; one
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
/// (a link into `/cas` — the only symlinks `mirror` creates for unloaded
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
