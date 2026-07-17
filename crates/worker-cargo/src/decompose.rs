//! Per-crate decomposition — the deep-deps model applied to a cargo workspace
//! (design/cargo-workers.md, phase 2). One compile job per workspace member,
//! keyed on exactly what that member's build reads, so an edit recompiles the
//! edited crate and its dependents and nothing else — and untouched members
//! are cache hits.
//!
//! Modes (reached via curry; `mode=all` is the public entry):
//!
//! * **all** — the driver: map over every workspace member with `crate`,
//!   combine the per-member results into one `{exit, stdout, stderr}` (the
//!   flat modes' shape, so callers can't tell the difference).
//! * **crate** — cheap orchestration, keyed on the whole tree (deliberately —
//!   it re-runs on any edit and does no compiling): parse the manifests,
//!   compute the member's workspace-dep closure, PRUNE the workspace to what
//!   the member's build reads (root manifest + lockfile + every member
//!   manifest + the closure's sources — all by CAS links), and map-then over
//!   the direct deps with itself (cmd=dep) into a `job`.
//! * **job** — the compile, keyed on (pruned tree, children, name, cmd): the
//!   narrow key that buys incrementality. Assembles the pruned workspace at
//!   the baked root — the member's own sources at fresh mtimes, everything
//!   else at epoch (sound here because caos content-addressing guarantees the
//!   children artifacts were built from exactly these sources; see the spike
//!   notes in the design doc) — stubs the non-closure members' declared
//!   target files, merges the children's `target/` artifacts beside the baked
//!   deps, and runs `cargo <cmd> -p <member>`.
//! * **combine** — merge the member results.
//!
//! A dep job's result is `{target: <artifacts>}` — its own additions to
//! `target/` plus its children's, so a parent merges only its *direct* deps.
//! A failed dep is `{exit, stdout, stderr}` like any cargo failure; parents
//! propagate it as their own result without compiling, so diagnostics bubble
//! to the top as values, attributed to the crate that broke.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use worker_common::{
    arg, caos, caos_curry, entries, file_name, link, map_then, own_image, path, read_arg, scratch,
    Arg,
};

use crate::{exit_code, run_cargo, tail, ws_root};

/// One workspace member, as parsed from the manifests.
struct Member {
    name: String,
    /// Workspace-relative directory (e.g. `crates/server`).
    dir: String,
    /// Workspace-relative dirs of path deps (dependencies, dev-, build-).
    deps: Vec<String>,
    /// Explicitly-declared target file paths (member-relative) — what a stub
    /// of this member must create for the workspace to parse.
    target_paths: Vec<String>,
}

/// The parsed workspace: members keyed by dir.
struct Workspace {
    members: BTreeMap<String, Member>,
}

impl Workspace {
    fn by_dir(&self, dir: &str) -> Result<&Member, String> {
        self.members
            .get(dir)
            .ok_or_else(|| format!("no workspace member at {dir:?}"))
    }

    /// The member's workspace-dep closure (dirs, member itself excluded).
    fn closure(&self, dir: &str) -> Result<BTreeSet<String>, String> {
        let mut seen = BTreeSet::new();
        let mut queue = vec![dir.to_string()];
        while let Some(d) = queue.pop() {
            for dep in &self.by_dir(&d)?.deps {
                if dep != dir && seen.insert(dep.clone()) {
                    queue.push(dep.clone());
                }
            }
        }
        Ok(seen)
    }
}

// ---- mode=all: the driver ----------------------------------------------------

pub fn all(cmd: &str) -> Result<(), String> {
    let tree = workspace_tree()?;
    let ws = parse_workspace(&tree)?;

    // One map child per member: name -> a blob holding the member's dir (the
    // entry name can't carry '/'). `crate` re-derives everything else.
    let members_dir = scratch("members")?;
    for m in ws.members.values() {
        fs::write(members_dir.join(&m.name), &m.dir)
            .map_err(|e| format!("writing member blob: {e}"))?;
    }
    caos(["put", path(&members_dir), "/cas/members"])?;

    let map = self_curry(&[
        ("mode", Arg::Lit("crate")),
        ("tree", Arg::Path(&tree)),
        ("cmd", Arg::Lit(cmd)),
    ])?;
    let then = self_curry(&[("mode", Arg::Lit("combine")), ("cmd", Arg::Lit(cmd))])?;
    map_then("/cas/members", Some(&map), Some(&then))
}

// ---- mode=crate: prune + recurse ---------------------------------------------

pub fn crate_mode(cmd: &str) -> Result<(), String> {
    // Here `in` is the member-dir blob we were mapped over; the workspace
    // tree rides the curry as `tree` (unlike `all`, whose `in` IS the tree).
    let tree = arg("tree");
    if !Path::new(&tree).exists() {
        return Err(format!("crate mode without a curried tree at {tree}"));
    }
    let ws = parse_workspace(&tree)?;
    let dir = read_blob(&arg("in"))?.trim().to_string();
    let member = ws.by_dir(&dir)?;

    let pruned = prune(&tree, &ws, member)?;
    let job = self_curry(&[
        ("mode", Arg::Lit("job")),
        ("ws", Arg::Path(&pruned)),
        ("name", Arg::Lit(&member.name)),
        ("dir", Arg::Lit(&member.dir)),
        ("cmd", Arg::Lit(cmd)),
    ])?;

    if member.deps.is_empty() {
        // No workspace deps: a plain tail call into the job (no children).
        return map_then(&arg("in"), None, Some(&job));
    }
    // Recurse on each direct dep with ourselves; deps always build as
    // artifacts (cmd=dep) regardless of what the top asked for.
    let deps_dir = scratch("deps")?;
    for dep in &member.deps {
        let name = &ws.by_dir(dep)?.name;
        fs::write(deps_dir.join(name), dep).map_err(|e| format!("writing dep blob: {e}"))?;
    }
    caos(["put", path(&deps_dir), "/cas/deps"])?;
    let map = self_curry(&[
        ("mode", Arg::Lit("crate")),
        ("tree", Arg::Path(&tree)),
        ("cmd", Arg::Lit("dep")),
    ])?;
    map_then("/cas/deps", Some(&map), Some(&job))
}

/// Prune the workspace to what `member`'s build reads: root manifest +
/// lockfile + every member's manifest + the member's and its closure's full
/// sources. Pure CAS linking — subtrees ride by hash, nothing is fetched.
fn prune(tree: &str, ws: &Workspace, member: &Member) -> Result<String, String> {
    let out = scratch("pruned")?;
    link_at(tree, "Cargo.toml", &out)?;
    // A lockfile rides when the workspace has one (caos does); a lockless
    // workspace (no external deps worth pinning) just resolves in the job.
    if Path::new(&format!("{tree}/Cargo.lock")).exists() {
        link_at(tree, "Cargo.lock", &out)?;
    }
    let mut full = ws.closure(&member.dir)?;
    full.insert(member.dir.clone());
    for m in ws.members.values() {
        if full.contains(&m.dir) {
            link_at(tree, &m.dir, &out)?;
        } else {
            link_at(tree, &format!("{}/Cargo.toml", m.dir), &out)?;
        }
    }
    caos(["put", path(&out), "/cas/pruned"])?;
    Ok("/cas/pruned".to_string())
}

/// Symlink `tree/<rel>` at `out/<rel>` (creating parents). The source only
/// needs to *exist* (a placeholder is fine): `caos put` resolves it by hash.
fn link_at(tree: &str, rel: &str, out: &Path) -> Result<(), String> {
    let src = format!("{tree}/{rel}");
    if !Path::new(&src).exists() {
        return Err(format!("workspace has no {rel:?}"));
    }
    let dst = out.join(rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    link(&src, &dst)
}

// ---- mode=job: the compile ---------------------------------------------------

pub fn job(cmd: &str) -> Result<(), String> {
    let name = read_arg("name")?;
    let dir = read_arg("dir")?;
    let pruned = arg("ws");
    caos(["get", "-r", &pruned])?;

    // Materialize at the baked root: own sources fresh, all else epoch.
    let ws = ws_root()?;
    crate::materialize(Path::new(&pruned), Path::new(&ws))?;
    stamp_epoch_except(Path::new(&pruned), Path::new(&ws), &dir)?;
    stub_missing_members(&pruned, &ws)?;

    // Merge the children's artifacts (and propagate a failed dep as our own
    // result: its {exit, stdout, stderr} bubbles up unchanged, no compile).
    let children = arg("children");
    let mut child_targets: Vec<PathBuf> = Vec::new();
    if Path::new(&children).exists() {
        caos(["get", &children])?;
        for child in entries(&children)? {
            caos(["get", path(&child)])?;
            if child.join("exit").exists() {
                return propagate_failure(&child);
            }
            let target = child.join("target");
            if target.exists() {
                caos(["get", "-r", path(&target)])?;
                crate::copy_into(&target, &Path::new(&ws).join("target"))?;
                child_targets.push(target);
            }
        }
    }

    // The compile. `dep` builds both artifact kinds (rmeta for dependents'
    // checks, rlibs for their builds/tests); the other cmds match the flat
    // modes, scoped to the member.
    let before = if cmd == "dep" {
        snapshot_target(&ws)?
    } else {
        HashSet::new()
    };
    let runs: Vec<Vec<String>> = match cmd {
        "dep" => vec![
            vec!["check".into(), "-p".into(), name.clone()],
            vec!["build".into(), "-p".into(), name.clone()],
        ],
        "check" => vec![vec![
            "check".into(),
            "-p".into(),
            name.clone(),
            "--all-targets".into(),
        ]],
        "build" => vec![vec!["build".into(), "-p".into(), name.clone()]],
        "test" => vec![vec!["test".into(), "-p".into(), name.clone()]],
        other => return Err(format!("unknown job cmd {other:?}")),
    };
    let mut last = None;
    for argv in &runs {
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let out = run_cargo(&refs, &ws)?;
        let failed = !out.status.success();
        last = Some(out);
        if failed {
            break;
        }
    }
    let out = last.expect("at least one cargo run");
    let exit = exit_code(&out.status);

    let res = scratch("result")?;
    if cmd == "dep" && exit == 0 {
        // Success: artifacts only — our own additions plus the children's, so
        // our dependents merge just their direct deps.
        let target = res.join("target");
        for child in &child_targets {
            link_files(child, &target)?;
        }
        stage_delta(&ws, &before, &target)?;
    } else {
        fs::write(res.join("exit"), format!("{exit}\n"))
            .map_err(|e| format!("writing exit: {e}"))?;
        fs::write(res.join("stdout"), tail(&out.stdout))
            .map_err(|e| format!("writing stdout: {e}"))?;
        fs::write(res.join("stderr"), tail(&out.stderr))
            .map_err(|e| format!("writing stderr: {e}"))?;
    }
    caos(["put", path(&res), "/cas/out"])
}

/// A failed dep's result becomes ours, verbatim.
fn propagate_failure(child: &Path) -> Result<(), String> {
    caos(["get", "-r", path(child)])?;
    let res = scratch("result")?;
    for entry in entries(path(child))? {
        let dst = res.join(file_name(&entry));
        fs::copy(&entry, &dst).map_err(|e| format!("copying {}: {e}", entry.display()))?;
    }
    caos(["put", path(&res), "/cas/out"])
}

/// Everything the pruned tree materialized gets epoch mtimes, except the
/// member's own dir (fresh, so it always recompiles). Sound because the
/// children artifacts were built from these exact contents (content-addressed
/// keys), so "unchanged since the fingerprint" is true by construction.
fn stamp_epoch_except(pruned: &Path, ws: &Path, own_dir: &str) -> Result<(), String> {
    fn walk(src: &Path, dst: &Path, skip: &Path) -> Result<(), String> {
        for entry in entries(path(src))? {
            let target = dst.join(file_name(&entry));
            if target == skip {
                continue;
            }
            let meta =
                fs::symlink_metadata(&entry).map_err(|e| format!("{}: {e}", entry.display()))?;
            if meta.file_type().is_symlink() {
                continue; // git symlinks carry no useful mtime
            } else if meta.is_dir() {
                walk(&entry, &target, skip)?;
            } else {
                let times = fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH);
                File::options()
                    .write(true)
                    .open(&target)
                    .and_then(|f| f.set_times(times))
                    .map_err(|e| format!("stamping {}: {e}", target.display()))?;
            }
        }
        Ok(())
    }
    walk(pruned, ws, &ws.join(own_dir))
}

/// Create the declared target files of members whose sources were pruned
/// away, so the workspace parses: cargo validates every member's explicit
/// target paths at load. The stubs are empty (never compiled — no job builds
/// these members), at epoch mtimes like the manifests.
fn stub_missing_members(pruned: &str, ws: &str) -> Result<(), String> {
    let parsed = parse_workspace(pruned)?;
    for m in parsed.members.values() {
        if Path::new(pruned).join(&m.dir).join("src").exists() || target_paths_present(pruned, m) {
            continue; // sources rode the pruned tree — a closure member
        }
        for rel in &m.target_paths {
            let p = Path::new(ws).join(&m.dir).join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            }
            let content = if rel.ends_with("main.rs") || rel.contains("/bin/") {
                "fn main() {}\n"
            } else {
                ""
            };
            fs::write(&p, content).map_err(|e| format!("stubbing {}: {e}", p.display()))?;
            let times = fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH);
            File::options()
                .write(true)
                .open(&p)
                .and_then(|f| f.set_times(times))
                .map_err(|e| format!("stamping {}: {e}", p.display()))?;
        }
    }
    Ok(())
}

/// Whether any of the member's declared target files came with the pruned
/// tree (i.e. it was a closure member even without a src/ dir).
fn target_paths_present(pruned: &str, m: &Member) -> bool {
    m.target_paths
        .iter()
        .any(|rel| Path::new(pruned).join(&m.dir).join(rel).exists())
}

/// The set of files currently under `<ws>/target`, as target-relative paths.
fn snapshot_target(ws: &str) -> Result<HashSet<PathBuf>, String> {
    fn walk(dir: &Path, root: &Path, out: &mut HashSet<PathBuf>) -> Result<(), String> {
        for entry in entries(path(dir))? {
            let meta =
                fs::symlink_metadata(&entry).map_err(|e| format!("{}: {e}", entry.display()))?;
            if meta.is_dir() {
                walk(&entry, root, out)?;
            } else {
                out.insert(entry.strip_prefix(root).unwrap_or(&entry).to_path_buf());
            }
        }
        Ok(())
    }
    let mut set = HashSet::new();
    let target = Path::new(ws).join("target");
    if target.exists() {
        walk(&target, &target, &mut set)?;
    }
    Ok(set)
}

/// Stage every file added under `<ws>/target` since `before` into `out`,
/// preserving relative structure — the member's own artifact delta.
fn stage_delta(ws: &str, before: &HashSet<PathBuf>, out: &Path) -> Result<(), String> {
    let target = Path::new(ws).join("target");
    let after = snapshot_target(ws)?;
    for rel in after.difference(before) {
        let src = target.join(rel);
        let dst = out.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        fs::copy(&src, &dst).map_err(|e| format!("staging {}: {e}", src.display()))?;
    }
    Ok(())
}

/// Symlink every file of the (fetched) tree at `src` into `dst` by relative
/// path, skipping paths that already exist — how children's artifact trees
/// union into a result without copying content (put resolves the links).
fn link_files(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in entries(path(src))? {
        let target = dst.join(file_name(&entry));
        let meta = fs::symlink_metadata(&entry).map_err(|e| format!("{}: {e}", entry.display()))?;
        if meta.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|e| format!("creating {}: {e}", target.display()))?;
            link_files(&entry, &target)?;
        } else if !target.exists() {
            link(&entry, &target)?;
        }
    }
    Ok(())
}

// ---- mode=combine ------------------------------------------------------------

/// Merge the per-member results into the flat modes' shape: first non-zero
/// exit wins the exit code; the streams concatenate under member headers
/// (bounded like any stream).
pub fn combine() -> Result<(), String> {
    let children = arg("children");
    caos(["get", "-r", &children])?;
    let mut exit = 0i32;
    let mut stdout = String::new();
    let mut stderr = String::new();
    for child in entries(&children)? {
        let name = file_name(&child);
        let child_exit: i32 = fs::read_to_string(child.join("exit"))
            .map_err(|e| format!("reading {name} exit: {e}"))?
            .trim()
            .parse()
            .unwrap_or(-1);
        if exit == 0 && child_exit != 0 {
            exit = child_exit;
        }
        for (stream, buf) in [("stdout", &mut stdout), ("stderr", &mut stderr)] {
            let p = child.join(stream);
            if let Ok(text) = fs::read_to_string(&p) {
                if !text.trim().is_empty() {
                    buf.push_str(&format!("── {name} ──\n{}\n", text.trim_end()));
                }
            }
        }
    }
    let res = scratch("result")?;
    fs::write(res.join("exit"), format!("{exit}\n")).map_err(|e| format!("writing exit: {e}"))?;
    fs::write(res.join("stdout"), tail(stdout.as_bytes()))
        .map_err(|e| format!("writing stdout: {e}"))?;
    fs::write(res.join("stderr"), tail(stderr.as_bytes()))
        .map_err(|e| format!("writing stderr: {e}"))?;
    caos(["put", path(&res), "/cas/out"])
}

// ---- shared helpers ----------------------------------------------------------

/// The workspace tree argument (`in` from a run-then/tool call, else `tree`).
fn workspace_tree() -> Result<String, String> {
    let tree = if Path::new(&arg("in")).exists() {
        arg("in")
    } else {
        arg("tree")
    };
    if !Path::new(&tree).exists() {
        return Err(format!("no workspace tree at {tree} (pass --tree or in)"));
    }
    Ok(tree)
}

/// Rebuild our own curry (we run as curry(cargo-base, bin=…)) with `extras`.
fn self_curry(extras: &[(&str, Arg)]) -> Result<String, String> {
    let bin = arg("bin");
    let mut kvs: Vec<(&str, Arg)> = Vec::new();
    if Path::new(&bin).exists() {
        kvs.push(("bin", Arg::Path(&bin)));
    }
    for (name, value) in extras {
        kvs.push((
            name,
            match value {
                Arg::Lit(s) => Arg::Lit(s),
                Arg::Path(s) => Arg::Path(s),
            },
        ));
    }
    caos_curry(&own_image(), &kvs)
}

/// Fetch and read a blob at a CAS path.
fn read_blob(cas_path: &str) -> Result<String, String> {
    caos(["get", cas_path])?;
    fs::read_to_string(cas_path).map_err(|e| format!("reading {cas_path}: {e}"))
}

// ---- manifest parsing --------------------------------------------------------

/// Parse the workspace's manifests out of a (partially fetched) tree: the
/// root's member list, then each member's name, path deps and declared target
/// paths. Fetches only tree levels and the manifest blobs themselves.
fn parse_workspace(tree: &str) -> Result<Workspace, String> {
    caos(["get", tree])?; // the root level: manifests + member-dir placeholders
    let root_text = read_blob(&format!("{tree}/Cargo.toml"))?;
    let root: toml::Value = root_text
        .parse()
        .map_err(|e| format!("parsing root Cargo.toml: {e}"))?;
    let member_dirs: Vec<String> = root
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .ok_or("root Cargo.toml has no [workspace] members")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| "non-string workspace member".to_string())
        })
        .collect::<Result<_, _>>()?;

    let mut members = BTreeMap::new();
    for dir in &member_dirs {
        if dir.contains(['*', '?']) {
            return Err(format!("glob workspace member {dir:?} is not supported"));
        }
        // Walk the path level by level (each tree fetch is one level), ending
        // with the member dir's own level so its Cargo.toml becomes visible.
        let mut cur = PathBuf::from(tree);
        for comp in dir.split('/') {
            caos(["get", path(&cur)])?;
            cur.push(comp);
        }
        caos(["get", path(&cur)])?;
        let text = read_blob(&format!("{tree}/{dir}/Cargo.toml"))?;
        let manifest: toml::Value = text
            .parse()
            .map_err(|e| format!("parsing {dir}/Cargo.toml: {e}"))?;
        members.insert(dir.clone(), parse_member(dir, &manifest)?);
    }
    Ok(Workspace { members })
}

fn parse_member(dir: &str, manifest: &toml::Value) -> Result<Member, String> {
    let name = manifest
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .ok_or_else(|| format!("{dir}/Cargo.toml has no package.name"))?
        .to_string();

    // Path deps, normalized to workspace-relative dirs.
    let mut deps = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(table) = manifest.get(section).and_then(|d| d.as_table()) else {
            continue;
        };
        for spec in table.values() {
            let Some(rel) = spec.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            deps.push(normalize(dir, rel)?);
        }
    }
    deps.sort();
    deps.dedup();

    // Explicit target paths ([lib].path, [[bin]].path, …) — what a stub must
    // create. Members without any explicit target get src/lib.rs so a stub
    // still has one target (a target-less package is a manifest error).
    let mut target_paths = Vec::new();
    if let Some(p) = manifest
        .get("lib")
        .and_then(|l| l.get("path"))
        .and_then(|p| p.as_str())
    {
        target_paths.push(p.to_string());
    }
    for kind in ["bin", "example", "test", "bench"] {
        if let Some(items) = manifest.get(kind).and_then(|b| b.as_array()) {
            for item in items {
                if let Some(p) = item.get("path").and_then(|p| p.as_str()) {
                    target_paths.push(p.to_string());
                }
            }
        }
    }
    if target_paths.is_empty() {
        target_paths.push("src/lib.rs".to_string());
    }

    Ok(Member {
        name,
        dir: dir.to_string(),
        deps,
        target_paths,
    })
}

/// Resolve `rel` (a path-dep value) against member dir `base`, staying
/// workspace-relative. Only `..` and plain components appear in practice.
fn normalize(base: &str, rel: &str) -> Result<String, String> {
    let mut parts: Vec<&str> = base.split('/').collect();
    for comp in rel.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(format!("path dep {rel:?} escapes the workspace"));
                }
            }
            c => parts.push(c),
        }
    }
    Ok(parts.join("/"))
}
