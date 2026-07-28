//! Find workspace files through the generic worker-tool ABI.
//!
//! The model-facing invocation receives its JSON object as `in` and the
//! workspace as a separate runtime argument. It launches a decomposed fold
//! over that workspace: one cached job per directory, with sparse marker trees
//! flowing back up by hash. A final continuation renders those markers into
//! the generic `result.json` envelope.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use globset::GlobBuilder;
use serde_json::{json, Value};
use worker_common::{
    arg, caos, caos_curry, cas_hash, entries, file_name, link, map_then, own_image, path, read_arg,
    read_arg_opt, run_then, run_worker, scratch, Arg,
};

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const MAX_MATCHES: usize = 1_000;

fn main() -> ExitCode {
    run_worker("glob", run)
}

fn run() -> Result<(), String> {
    if Path::new(&arg("children")).exists() {
        return combine();
    }
    match read_arg_opt("mode")?.as_deref() {
        None => start(),
        Some("fold") => fold(),
        Some("finish") => finish(),
        Some(mode) => Err(format!("unknown glob worker mode {mode:?}")),
    }
}

/// Parse the generic call, report call errors as values, and launch the fold.
fn start() -> Result<(), String> {
    let input = arg("in");
    caos(["get", &input])?;
    if !Path::new(&input).is_file() {
        return Err("glob input must be a JSON blob".to_string());
    }

    let call: Value = serde_json::from_str(
        &fs::read_to_string(&input).map_err(|error| format!("reading glob input: {error}"))?,
    )
    .map_err(|error| format!("parsing glob input: {error}"))?;
    let Some(pattern) = call["pattern"].as_str() else {
        return write_result("glob needs a string `pattern`", true);
    };
    if let Err(error) = matcher(pattern) {
        return write_result(&format!("invalid pattern: {error}"), true);
    }

    let workspace = arg("workspace");
    caos(["get", &workspace])?;
    if !Path::new(&workspace).is_dir() {
        return Err("glob needs a workspace tree".to_string());
    }
    let fold = self_curry(&[("mode", Arg::Lit("fold")), ("pattern", Arg::Lit(pattern))])?;
    let finish = self_curry(&[("mode", Arg::Lit("finish"))])?;
    run_then(&workspace, &fold, Some(&finish))
}

/// Scan one directory and recurse over wrapped child directories.
fn fold() -> Result<(), String> {
    let pattern = read_arg("pattern")?;
    let matcher = matcher(&pattern)
        .map_err(|error| format!("invalid curried pattern {pattern:?}: {error}"))?;

    let input = arg("in");
    caos(["get", &input])?;
    let (tree, prefix) = if Path::new(&arg("wrapped")).exists() {
        let prefix_path = format!("{input}/prefix");
        caos(["get", &prefix_path])?;
        let prefix =
            fs::read_to_string(&prefix_path).map_err(|error| format!("reading prefix: {error}"))?;
        let tree = format!("{input}/tree");
        caos(["get", &tree])?;
        (tree, prefix)
    } else {
        (input, String::new())
    };
    if !Path::new(&tree).is_dir() {
        return Err("glob fold input must be a tree".to_string());
    }

    let own = scratch("glob-own")?;
    let wrappers = scratch("glob-wrappers")?;
    let mut has_subdirs = false;
    for child in entries(&tree)? {
        let name = file_name(&child);
        if prefix.is_empty() && name == ".caos" {
            continue;
        }
        let candidate = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if child.is_dir() {
            has_subdirs = true;
            let wrapper = wrappers.join(&name);
            fs::create_dir(&wrapper)
                .map_err(|error| format!("creating {}: {error}", wrapper.display()))?;
            link(&child, wrapper.join("tree"))?;
            fs::write(wrapper.join("prefix"), candidate)
                .map_err(|error| format!("writing glob prefix: {error}"))?;
        } else if matcher.is_match(&candidate) {
            fs::write(own.join(name), [])
                .map_err(|error| format!("writing glob marker: {error}"))?;
        }
    }

    if !has_subdirs {
        return caos(["put", path(&own), "/cas/out"]);
    }

    let own_cas = "/cas/glob-own";
    caos(["put", path(&own), own_cas])?;
    let wrappers_cas = "/cas/glob-wrappers";
    caos(["put", path(&wrappers), wrappers_cas])?;
    let map = self_curry(&[
        ("mode", Arg::Lit("fold")),
        ("pattern", Arg::Lit(&pattern)),
        ("wrapped", Arg::Lit("1")),
    ])?;
    let then = self_curry(&[("own", Arg::Path(own_cas))])?;
    map_then(wrappers_cas, Some(&map), Some(&then))
}

/// Merge this directory's markers with non-empty child sparse trees.
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

/// Convert the sparse fold result into the generic result envelope.
fn finish() -> Result<(), String> {
    let result = arg("result");
    caos(["get", &result])?;
    let mut paths = Vec::new();
    collect_paths(Path::new(&result), "", &mut paths)?;
    paths.sort();
    let total = paths.len();
    paths.truncate(MAX_MATCHES);
    let mut content = if paths.is_empty() {
        "no matches".to_string()
    } else {
        paths.join("\n")
    };
    if total > paths.len() {
        content +=
            &format!("\n[truncated: first {MAX_MATCHES} of {total} matches — narrow the pattern]");
    }
    write_result(&content, false)
}

fn collect_paths(dir: &Path, prefix: &str, paths: &mut Vec<String>) -> Result<(), String> {
    caos(["get", path(dir)])?;
    for child in entries(path(dir))? {
        let name = file_name(&child);
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if child.is_dir() {
            collect_paths(&child, &relative, paths)?;
        } else {
            paths.push(relative);
        }
    }
    Ok(())
}

fn matcher(pattern: &str) -> Result<globset::GlobMatcher, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
}

fn write_result(content: &str, is_error: bool) -> Result<(), String> {
    let out = scratch("glob-result")?;
    let result = json!({"content": content, "is_error": is_error});
    fs::write(out.join("result.json"), result.to_string())
        .map_err(|error| format!("writing result.json: {error}"))?;
    caos(["put", path(&out), "/cas/out"])
}

fn self_curry<'a>(extras: &[(&'a str, Arg<'a>)]) -> Result<String, String> {
    let worker = arg("worker1");
    let mut args = Vec::new();
    if Path::new(&worker).exists() {
        args.push(("worker1", Arg::Path(worker.as_str())));
    }
    for (name, value) in extras {
        args.push((
            *name,
            match value {
                Arg::Lit(value) => Arg::Lit(value),
                Arg::Path(value) => Arg::Path(value),
            },
        ));
    }
    caos_curry(&own_image(), &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_patterns_include_root_and_nested_files() {
        let matcher = matcher("**/*.rs").unwrap();
        assert!(matcher.is_match("root.rs"));
        assert!(matcher.is_match("src/deep/mod.rs"));
        assert!(!matcher.is_match("src/readme.md"));
    }

    #[test]
    fn one_star_does_not_cross_directories() {
        let matcher = matcher("src/*.rs").unwrap();
        assert!(matcher.is_match("src/lib.rs"));
        assert!(!matcher.is_match("src/deep/mod.rs"));
    }
}
