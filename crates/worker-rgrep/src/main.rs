//! caos-worker-rgrep: recursive grep over a workspace tree — one job per
//! directory, the result a **sparse tree** (design/agent-harness.md): only
//! matching files appear, each holding its matches as `linenum:line` lines;
//! a directory's result embeds its children's result trees *by hash*, so
//! nothing is copied as results ride up the fold, and every level is cached
//! per (subtree hash, pattern) — editing one file re-runs only its ancestor
//! chain, identical subtrees share one job. Flattening to classic
//! `path:linenum:line` output is the *caller's* presentation choice
//! (llm-step renders it at the transcript boundary).
//!
//! Three positions, told apart by the arguments present:
//!
//! * `--in` a tree, no `--children` — grep the files at THIS level (they stay
//!   local to this job), then map-then over a synthetic tree holding just the
//!   subdirectories, currying the local matches into `then`.
//! * `--children` present (the `then` position) — combine: local match files
//!   plus each non-empty child result tree, linked into one tree.
//! * `--in` a file (a file-scoped grep) — the match blob itself.
//!
//! The pattern is a curried arg, so the recursion image — this request's own
//! image — carries it, and a child's cache key is exactly (subtree, pattern).
//! Binary files (NUL in the first 8KB) and symlinks are skipped, like grep -I.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, cas_hash, entries, file_name, link, map_then, own_image, path,
    read_arg, run_worker, scratch, Arg,
};

/// git's well-known empty tree — a child result with no matches, skipped so
/// the combined tree stays sparse.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn main() -> ExitCode {
    run_worker("rgrep", run)
}

fn run() -> Result<(), String> {
    if Path::new(&arg("children")).exists() {
        return combine();
    }

    let pattern = read_arg("pattern")?;
    let re = regex::Regex::new(&pattern)
        .map_err(|e| format!("invalid pattern {pattern:?}: {e} (the caller validates)"))?;

    let input = arg("in");
    caos(["get", &input])?; // a file: its content; a tree: one level

    if Path::new(&input).is_file() {
        // File-scoped grep: the matches blob is the whole result.
        let out = scratch("rgrep-file")?.join("matches");
        let matches = grep_file(&re, Path::new(&input))?.unwrap_or_default();
        fs::write(&out, matches).map_err(|e| format!("writing matches: {e}"))?;
        return caos(["put", path(&out), "/cas/out"]);
    }

    // A directory: grep the local files, recurse over the subdirectories.
    let own = scratch("rgrep-own")?;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for child in entries(&input)? {
        if child.is_dir() {
            subdirs.push(child);
            continue;
        }
        caos(["get", path(&child)])?;
        let is_symlink = fs::symlink_metadata(&child)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_symlink {
            continue;
        }
        if let Some(matches) = grep_file(&re, &child)? {
            fs::write(own.join(file_name(&child)), matches)
                .map_err(|e| format!("writing local matches: {e}"))?;
        }
    }

    if subdirs.is_empty() {
        // Leaf directory: the local matches are the whole (possibly empty,
        // hence sparse) result.
        return caos(["put", path(&own), "/cas/out"]);
    }

    // Fan out: map ourselves (this request's image — the pattern rides in it)
    // over just the subdirectories; the local matches ride into `then`.
    let own_cas = "/cas/rgrep-own";
    caos(["put", path(&own), own_cas])?;
    let dir = scratch("rgrep-subdirs")?;
    for d in &subdirs {
        link(d, dir.join(file_name(d)))?;
    }
    let subdirs_cas = "/cas/rgrep-subdirs";
    caos(["put", path(&dir), subdirs_cas])?;

    // `own_image` is the UNWRAPPED base image — curry layers (including the
    // runner-pool `bin` binding this worker ships as) expand into args before
    // a request is stored. So the recursion curries rebind `bin` (when
    // present) plus what each position needs: the pattern for the mapped
    // children, the local matches for `then` (combine is pure linking, no
    // pattern). Content-addressed, hence identical objects at every level.
    let me = own_image();
    let bin = arg("bin");
    let mut map_kvs: Vec<(&str, Arg)> = vec![("pattern", Arg::Lit(&pattern))];
    let mut then_kvs: Vec<(&str, Arg)> = vec![("own", Arg::Path(own_cas))];
    if Path::new(&bin).exists() {
        map_kvs.push(("bin", Arg::Path(&bin)));
        then_kvs.push(("bin", Arg::Path(&bin)));
    }
    let map = caos_curry(&me, &map_kvs)?;
    let then = caos_curry(&me, &then_kvs)?;
    map_then(subdirs_cas, Some(&map), Some(&then))
}

/// The `then` position: one sparse tree from the local match files (`--own`)
/// and the non-empty child results (`--children`, named by subdirectory).
/// Links only — the child trees are embedded by hash, never read.
fn combine() -> Result<(), String> {
    let dir = scratch("rgrep-combine")?;
    let own = arg("own");
    if Path::new(&own).exists() {
        caos(["get", &own])?;
        for e in entries(&own)? {
            link(&e, dir.join(file_name(&e)))?;
        }
    }
    let children = arg("children");
    caos(["get", &children])?;
    for child in entries(&children)? {
        if cas_hash(path(&child))? == EMPTY_TREE {
            continue;
        }
        link(&child, dir.join(file_name(&child)))?;
    }
    caos(["put", path(&dir), "/cas/out"])
}

/// One file's matches as `linenum:line` lines — `None` when there are none,
/// or the file is binary (NUL in the first 8KB).
fn grep_file(re: &regex::Regex, p: &Path) -> Result<Option<String>, String> {
    let bytes = fs::read(p).map_err(|e| format!("reading {}: {e}", p.display()))?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if re.is_match(line) {
            out.push_str(&format!("{}:{line}\n", i + 1));
        }
    }
    Ok(if out.is_empty() { None } else { Some(out) })
}
