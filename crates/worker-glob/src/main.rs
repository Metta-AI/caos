//! caos-worker-glob: find files in a workspace tree by glob pattern.
//!
//! The worker is a decomposed fold: each directory is its own cached job and
//! the result is a sparse tree containing an empty marker blob at every
//! matching path. Child jobs receive `{tree, prefix}` wrappers so patterns
//! match paths relative to the original input while the fold stays bounded.
//! Callers choose how to present the sparse tree; llm-step renders it as a
//! sorted path list for the `glob` tool.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use globset::GlobBuilder;
use worker_common::{
    arg, caos, caos_curry, cas_hash, entries, file_name, link, map_then, own_image, path, read_arg,
    run_worker, scratch, Arg,
};

/// git's well-known empty tree. Empty child results are omitted from the
/// combined sparse result.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn main() -> ExitCode {
    run_worker("glob", run)
}

fn run() -> Result<(), String> {
    if Path::new(&arg("children")).exists() {
        return combine();
    }

    let pattern = read_arg("pattern")?;
    let matcher = GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("invalid pattern {pattern:?}: {e}"))?
        .compile_matcher();

    let input = arg("in");
    caos(["get", &input])?;
    let (tree, prefix) = if Path::new(&arg("wrapped")).exists() {
        let prefix_path = format!("{input}/prefix");
        caos(["get", &prefix_path])?;
        let prefix =
            fs::read_to_string(&prefix_path).map_err(|e| format!("reading glob prefix: {e}"))?;
        let tree = format!("{input}/tree");
        caos(["get", &tree])?;
        (tree, prefix)
    } else {
        (input, String::new())
    };
    if !Path::new(&tree).is_dir() {
        return Err("glob input must be a tree".to_string());
    }

    let own = scratch("glob-own")?;
    let wrappers = scratch("glob-wrappers")?;
    let mut has_subdirs = false;
    for child in entries(&tree)? {
        let name = file_name(&child);
        let candidate = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = fs::symlink_metadata(&child)
            .map_err(|e| format!("reading metadata for {}: {e}", child.display()))?;
        if metadata.file_type().is_dir() {
            has_subdirs = true;
            let wrapper = wrappers.join(&name);
            fs::create_dir(&wrapper)
                .map_err(|e| format!("creating wrapper {}: {e}", wrapper.display()))?;
            link(&child, wrapper.join("tree"))?;
            fs::write(wrapper.join("prefix"), candidate)
                .map_err(|e| format!("writing glob prefix: {e}"))?;
        } else if matcher.is_match(&candidate) {
            fs::write(own.join(name), []).map_err(|e| format!("writing glob marker: {e}"))?;
        }
    }

    if !has_subdirs {
        return caos(["put", path(&own), "/cas/out"]);
    }

    let own_cas = "/cas/glob-own";
    caos(["put", path(&own), own_cas])?;
    let wrappers_cas = "/cas/glob-wrappers";
    caos(["put", path(&wrappers), wrappers_cas])?;

    let me = own_image();
    let bin = arg("bin");
    let mut map_kvs = vec![("pattern", Arg::Lit(&pattern)), ("wrapped", Arg::Lit("1"))];
    let mut then_kvs = vec![("own", Arg::Path(own_cas))];
    if Path::new(&bin).exists() {
        map_kvs.push(("bin", Arg::Path(&bin)));
        then_kvs.push(("bin", Arg::Path(&bin)));
    }
    let map = caos_curry(&me, &map_kvs)?;
    let then = caos_curry(&me, &then_kvs)?;
    map_then(wrappers_cas, Some(&map), Some(&then))
}

/// Merge local marker blobs with non-empty child sparse trees. Every entry is
/// linked by recorded hash; file contents are never fetched or copied.
fn combine() -> Result<(), String> {
    let dir = scratch("glob-combine")?;
    let own = arg("own");
    caos(["get", &own])?;
    for entry in entries(&own)? {
        link(&entry, dir.join(file_name(&entry)))?;
    }

    let children = arg("children");
    caos(["get", &children])?;
    for child in entries(&children)? {
        if cas_hash(path(&child))? != EMPTY_TREE {
            link(&child, dir.join(file_name(&child)))?;
        }
    }
    caos(["put", path(&dir), "/cas/out"])
}
