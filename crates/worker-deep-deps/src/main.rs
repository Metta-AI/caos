//! caos-worker-deep-deps: restructure a tree so that each directory's declared
//! dependencies live, recursively deepened, inside it.
//!
//! Any directory may carry a `DEPS` file listing one dependency per line as
//!
//! ```text
//! <path> <name>
//! ```
//!
//! where `<path>` is relative to the DEPS file's OWN directory (so `../foo`
//! reaches out of the subtree) and `<name>` is what the dependency is mounted
//! under. The output mirrors the input tree, but every directory is "deepened":
//! its `DEPS` is replaced by a `DEEP-DEPS/` subtree whose children are the named
//! dependencies, each ITSELF deepened.
//!
//! ONE RUN, one pass. This used to recurse through `map_then` with this same
//! image on both sides — a `node` job per directory to enumerate it, an
//! `assemble` job per directory to rebuild it — so a tree of N directories cost
//! ~2N containers.
//!
//! The split bought narrow cache keys for `assemble` (a directory's own files
//! plus its deepened subgraph, so recompute was O(changed + dependents)) — but
//! it never delivered them, because `node` is keyed on the WHOLE TREE and so
//! re-ran for every directory on any edit. The tree changes on every edit that
//! matters, so the common case paid ~N container spawns of pure orchestration to
//! reach a cache that the same edit had already invalidated.
//!
//! Deepening is tree rewriting: no compiling, no network, nothing but reading a
//! materialized tree and staging symlinks. One worker doing all of it is far
//! cheaper than N containers doing it in pieces, and it stays cheap until the
//! tree is very large — at which point the answer is to make the ONE pass
//! incremental, not to reintroduce a per-directory container.
//!
//! Sharing is by absolute path: a dependency reached from two places is deepened
//! once and staged twice (as symlinks, so no bytes move), and `caos put` gives
//! the identical subtree one hash. That is what makes bootstrap's hand-deepen
//! (`build-builtins.sh`) reproducible — it must match this byte for byte, or the
//! seeded keys stop matching the keys resolution forms.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use worker_common::{arg, caos, entries, file_name, link, path, run_worker, scratch};

fn main() -> ExitCode {
    run_worker("deep-deps", run)
}

fn run() -> Result<(), String> {
    let input = arg("in");
    // The whole tree, in full: this pass reads every directory, and one
    // recursive fetch is one round trip instead of one per directory.
    caos(["get", "-r", &input])?;

    let staged = scratch("out")?;
    let mut memo: HashMap<String, PathBuf> = HashMap::new();
    deepen(&input, "", &staged, &mut memo, &mut Vec::new())?;
    caos(["put", path(&staged), "/cas/out"])
}

/// Deepen the directory at `rel` within the materialized tree at `base`, staging
/// the result at `into`: its own files (minus `DEPS`), its sub-directories
/// deepened in place, and its `DEPS` targets deepened under `DEEP-DEPS/`.
///
/// `memo` maps an absolute tree path to a staged directory already deepened, so
/// a dependency reached twice is computed once. `visiting` is the current
/// dependency chain, which is what catches a cycle: the recursion used to
/// re-enter the same `node` request and rely on the server's run-cycle
/// detection, and a single pass has no server round trip to be caught by.
fn deepen(
    base: &str,
    rel: &str,
    into: &Path,
    memo: &mut HashMap<String, PathBuf>,
    visiting: &mut Vec<String>,
) -> Result<(), String> {
    let dir = if rel.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rel}")
    };

    let mut deps_file: Option<PathBuf> = None;
    let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries(&dir)? {
        let name = file_name(&entry);
        let meta =
            fs::symlink_metadata(&entry).map_err(|e| format!("stat {}: {e}", entry.display()))?;
        if meta.is_dir() {
            subdirs.push((name, entry));
        } else if name == "DEPS" {
            deps_file = Some(entry); // replaced by DEEP-DEPS, never an own file
        } else {
            link(&entry, into.join(&name))?; // a plain file, kept as-is
        }
    }

    // Sub-directories stay at their own names.
    for (name, _) in &subdirs {
        let child = into.join(name);
        fs::create_dir_all(&child).map_err(|e| format!("creating {}: {e}", child.display()))?;
        deepen(base, &join(rel, name), &child, memo, visiting)?;
    }

    let Some(deps) = deps_file else {
        return Ok(());
    };
    let parsed = parse_deps(&deps)?;
    if parsed.is_empty() {
        // A DEPS of nothing but comments drops the file and adds NO `DEEP-DEPS`.
        // Creating an empty one would be a real (empty-tree) entry in the output
        // and would diverge from bootstrap's hand-deepen.
        return Ok(());
    }
    let dd = into.join("DEEP-DEPS");
    fs::create_dir_all(&dd).map_err(|e| format!("creating {}: {e}", dd.display()))?;
    for (dep_path, mount) in parsed {
        let target = normalize(&join(rel, &dep_path))?;
        let at = dd.join(&mount);
        if at.exists() {
            return Err(format!(
                "directory {rel:?} declares two deps mounted as {mount:?}"
            ));
        }
        // Already deepened somewhere else: re-stage that result rather than
        // walking it again. NOT a symlink to it — `caos put` resolves a symlink
        // into /cas to its recorded hash, but one pointing at a staging
        // directory is recorded AS A SYMLINK, which would put a link where the
        // subtree belongs. Copying the staged structure (all symlinks, so no
        // content moves) yields the identical tree, and identical trees are one
        // object.
        if let Some(done) = memo.get(&target) {
            copy_staged(&done.clone(), &at)?;
            continue;
        }
        if visiting.contains(&target) {
            return Err(format!(
                "dependency cycle: {} -> {target:?}",
                visiting.join(" -> ")
            ));
        }
        visiting.push(target.clone());
        fs::create_dir_all(&at).map_err(|e| format!("creating {}: {e}", at.display()))?;
        deepen(base, &target, &at, memo, visiting)?;
        visiting.pop();
        memo.insert(target, at);
    }
    Ok(())
}

// ---- helpers -------------------------------------------------------------------

/// The `(path, name)` pairs in a `DEPS` file: one per non-empty, non-`#` line,
/// each two whitespace-separated fields.
fn parse_deps(deps: &Path) -> Result<Vec<(String, String)>, String> {
    let text = fs::read_to_string(deps).map_err(|e| format!("reading DEPS: {e}"))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(dep_path), Some(name), None) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(format!(
                "malformed DEPS line {line:?}: expected `<path> <name>`"
            ));
        };
        out.push((dep_path.to_string(), name.to_string()));
    }
    Ok(out)
}

/// Join a tree-relative directory `rel` with a further `comp` (a name or a
/// relative dep path), leaving normalization to [`normalize`].
fn join(rel: &str, comp: &str) -> String {
    if rel.is_empty() {
        comp.to_string()
    } else {
        format!("{rel}/{comp}")
    }
}

/// Resolve `.`/`..` in a `/`-separated path lexically, erroring if it escapes
/// the tree root.
fn normalize(p: &str) -> Result<String, String> {
    let mut stack: Vec<&str> = Vec::new();
    for comp in p.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                stack
                    .pop()
                    .ok_or_else(|| format!("dep path {p:?} escapes the tree root"))?;
            }
            c => stack.push(c),
        }
    }
    Ok(stack.join("/"))
}

/// Recreate a staged directory at `to`: directories are made, symlinks are
/// remade with the same target. Everything a staged tree holds is one or the
/// other (a plain file is staged as a symlink into `/cas`), so this reproduces
/// it exactly while moving no content.
fn copy_staged(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("creating {}: {e}", to.display()))?;
    for entry in entries(path(from))? {
        let name = file_name(&entry);
        let meta =
            fs::symlink_metadata(&entry).map_err(|e| format!("stat {}: {e}", entry.display()))?;
        if meta.is_symlink() {
            let target =
                fs::read_link(&entry).map_err(|e| format!("readlink {}: {e}", entry.display()))?;
            link(&target, to.join(&name))?;
        } else if meta.is_dir() {
            copy_staged(&entry, &to.join(&name))?;
        } else {
            return Err(format!("unexpected staged entry {}", entry.display()));
        }
    }
    Ok(())
}
