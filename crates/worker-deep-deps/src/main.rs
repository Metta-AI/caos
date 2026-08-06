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
//! It recurses through `map_then`, with this same image on both sides:
//!   * `node` lists a directory, resolves its `DEPS` (relative paths against the
//!     WHOLE tree, curried in as `--tree`), and maps `node` over both its
//!     sub-directories and its dependency targets, finishing with `assemble`;
//!   * `assemble` rebuilds the directory from its own files (`--own`) plus the
//!     deepened children (`--children`), placing each by the key it was mapped
//!     under: a sub-directory back at its name, a dependency under `DEEP-DEPS`.
//!
//! Every child is addressed by its ABSOLUTE path from the tree root, so a
//! dependency reached from two places is one node — one memoized computation,
//! shared by hash. `assemble` is keyed only on a directory's own files and its
//! deepened subgraph, so real recompute is O(changed directory + its
//! dependents); `node` carries the whole tree and re-runs on any edit, but it is
//! pure orchestration. A dependency cycle re-enters the same `node` request and
//! is caught by the server's run-cycle detection.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, entries, file_name, link, map_then, own_image, path, read_arg_opt,
    run_worker, scratch, Arg,
};

fn main() -> ExitCode {
    run_worker("deep-deps", run)
}

fn run() -> Result<(), String> {
    // `--mode` is absent for the public call (deepen the whole `--in` tree). The
    // internal `node`/`assemble` modes are reached only by curry in the
    // recursion, never passed by a caller.
    match read_arg_opt("mode")?.as_deref() {
        None | Some("") => node_dir(&arg("in"), ""),
        Some("node") => {
            // `--in` is a blob naming the absolute path (from the tree root) of
            // the directory to deepen; the whole tree rides curried as `--tree`.
            let rel = read_blob(&arg("in"))?;
            node_dir(&arg("tree"), &rel)
        }
        Some("assemble") => assemble(),
        Some(other) => Err(format!("unknown mode {other:?}")),
    }
}

/// Deepen the directory at `rel` within the tree rooted at CAS path `base`:
/// stage its own files (minus `DEPS`), enumerate the children to recurse on —
/// its sub-directories (mounted back at their names) and its `DEPS` targets
/// (mounted under `DEEP-DEPS`) — then map `node` over them and `assemble` the
/// result. Sharing is by absolute path, so a target reached twice is one node.
fn node_dir(base: &str, rel: &str) -> Result<(), String> {
    let dir = fetch_dir(base, rel)?;

    let own = scratch("own")?;
    let work = scratch("work")?;
    let mut deps_file: Option<PathBuf> = None;
    for entry in entries(&dir)? {
        let name = file_name(&entry);
        let meta =
            fs::symlink_metadata(&entry).map_err(|e| format!("stat {}: {e}", entry.display()))?;
        if meta.is_dir() {
            // A sub-directory: recurse on it, mount the result back at its name.
            let child = join(rel, &name);
            write_blob(&work.join(format!("n_{name}")), &child)?;
        } else if name == "DEPS" {
            deps_file = Some(entry); // replaced by DEEP-DEPS, never an own file
        } else {
            link(&entry, own.join(&name))?; // a plain file, kept as-is
        }
    }

    if let Some(deps) = deps_file {
        for (dep_path, mount) in parse_deps(&deps)? {
            let target = normalize(&join(rel, &dep_path))?;
            let key = format!("d_{mount}");
            if work.join(&key).exists() {
                return Err(format!(
                    "directory {rel:?} declares two deps mounted as {mount:?}"
                ));
            }
            write_blob(&work.join(&key), &target)?;
        }
    }

    caos(["put", path(&own), "/cas/own"])?;
    caos(["put", path(&work), "/cas/work"])?;
    map_then(
        "/cas/work",
        Some(&node_image(base)?),
        Some(&assemble_image()?),
    )
}

/// Rebuild a directory: its own files (`--own`) plus the deepened `--children`,
/// each placed by the key it was mapped under — `n_<name>` back at `<name>`,
/// `d_<name>` under `DEEP-DEPS/<name>`. Curried with only `--own`, so its cache
/// key is just this directory's own files and its deepened subgraph.
fn assemble() -> Result<(), String> {
    let node = scratch("node")?;

    let own = arg("own");
    caos(["get", &own])?; // one level: the directory's own files
    for entry in entries(&own)? {
        link(&entry, node.join(file_name(&entry)))?;
    }

    let children = arg("children");
    caos(["get", &children])?; // one level: a deepened node per key
    let mut deep_deps: Option<PathBuf> = None;
    for entry in entries(&children)? {
        let key = file_name(&entry);
        if let Some(name) = key.strip_prefix("n_") {
            link(&entry, node.join(name))?;
        } else if let Some(name) = key.strip_prefix("d_") {
            let dd = deep_deps
                .get_or_insert_with(|| node.join("DEEP-DEPS"))
                .clone();
            fs::create_dir_all(&dd).map_err(|e| format!("creating {}: {e}", dd.display()))?;
            link(&entry, dd.join(name))?;
        } else {
            return Err(format!("unexpected child key {key:?}"));
        }
    }

    caos(["put", path(&node), "/cas/out"])
}

// ---- helpers -------------------------------------------------------------------

/// The `(path, name)` pairs in a `DEPS` file: one per non-empty, non-`#` line,
/// each two whitespace-separated fields.
fn parse_deps(deps: &Path) -> Result<Vec<(String, String)>, String> {
    caos(["get", path(deps)])?; // expand the blob so it's readable
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

/// Materialize (one level each) the directory at `rel` within the tree rooted at
/// `base`, walking from the root so every ancestor is fetched, and return its
/// CAS path. `rel` is empty for the tree root itself.
fn fetch_dir(base: &str, rel: &str) -> Result<String, String> {
    let mut cur = base.to_string();
    caos(["get", &cur])?;
    for comp in rel.split('/').filter(|c| !c.is_empty()) {
        cur = format!("{cur}/{comp}");
        caos(["get", &cur])?;
    }
    Ok(cur)
}

/// The recursion's `map` image: this image, in `node` mode, carrying the whole
/// tree so a child's own relative deps resolve against it. Its `--in` is each
/// child's absolute-path blob.
fn node_image(base: &str) -> Result<String, String> {
    let mut kvs: Vec<(&str, Arg)> = vec![("mode", Arg::Lit("node")), ("tree", Arg::Path(base))];
    rebind_worker1(&mut kvs);
    caos_curry(&own_image(), &kvs)
}

/// The recursion's `then` image: this image in `assemble` mode, carrying only
/// this directory's own files (`--own`) — so it keys narrowly.
fn assemble_image() -> Result<String, String> {
    let mut kvs: Vec<(&str, Arg)> = vec![
        ("mode", Arg::Lit("assemble")),
        ("own", Arg::Path("/cas/own")),
    ];
    rebind_worker1(&mut kvs);
    caos_curry(&own_image(), &kvs)
}

/// Rebind the runner-pool `worker1` binary (when present) so the recursion
/// re-execs this binary. A no-op for a baked image (see the fixture rgrep).
fn rebind_worker1<'a>(kvs: &mut Vec<(&'a str, Arg<'a>)>) {
    if Path::new(WORKER1).exists() {
        kvs.push(("worker1", Arg::Path(WORKER1)));
    }
}

/// The runner-pool binary's CAS path (`/cas/args/worker1`).
const WORKER1: &str = "/cas/args/worker1";

/// Read a CAS blob's trimmed contents (expanding the placeholder first).
fn read_blob(cas_path: &str) -> Result<String, String> {
    caos(["get", cas_path])?;
    Ok(fs::read_to_string(cas_path)
        .map_err(|e| format!("reading {cas_path}: {e}"))?
        .trim()
        .to_string())
}

/// Write `content` as a file at `at` (a work-tree entry).
fn write_blob(at: &Path, content: &str) -> Result<(), String> {
    fs::write(at, content).map_err(|e| format!("writing {}: {e}", at.display()))
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
