//! The `eval-path` tool: evaluate a path in a tree and report what it produces.
//!
//! Two stages, named by `--stage` (SPEC, "Worker scripts"), because evaluating a
//! `.caos-expr` may dispatch runs and a worker may not block on one: the first
//! records an `eval-path-then` continuation and exits, and the server walks the
//! expression and calls the second with the object it produced.
//!
//! * absent: pick the tree to evaluate in. A path that begins inside a SOURCE TREE
//!   (a commit entry on the way down) is evaluated against that tree's own root,
//!   because a repository's root `.caos-expr` is what makes its paths mean
//!   anything; the rest of the path is what is evaluated there. A path need not
//!   exist in the stored tree: `caos-std/` is a mount that only evaluation makes.
//! * `report`: print `<kind> <hash>`, and a blob's text.
//!
//! An expression that does not evaluate is a VALUE: it is delivered to the
//! second stage as an error and reported there, so the model is told rather than
//! the call failed (the question was what a path evaluates to, and "it doesn't"
//! is an answer).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, cas_hash, cas_kind, eval_then_catching, own_image, path, read_arg_opt,
    run_worker, scratch, Arg,
};

/// Text shown of a blob result; past this only its hash is given.
const MAX_BLOB_BYTES: usize = 100_000;

fn main() -> ExitCode {
    run_worker("eval-path", run)
}

fn run() -> Result<(), String> {
    match read_arg_opt("stage")?.as_deref() {
        None | Some("") => start(),
        Some("report") => report(),
        Some(other) => Err(format!("unknown stage {other:?}")),
    }
}

fn split_path(raw: &str) -> Vec<String> {
    raw.trim()
        .trim_start_matches('/')
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_string)
        .collect()
}

fn answer(text: &str) -> Result<(), String> {
    let file = scratch("eval-path-out")?.join("answer");
    fs::write(&file, text).map_err(|e| format!("writing the answer: {e}"))?;
    caos(["put", path(&file), "/cas/out"])
}

fn failed(message: &str) -> Result<(), String> {
    let dir = scratch("eval-path-out")?;
    fs::write(dir.join("report"), format!("FAILED: {message}\n"))
        .map_err(|e| format!("writing the report: {e}"))?;
    caos(["put", path(&dir), "/cas/out"])
}

/// `caos resolve <tree> <relative> <into>`: the node at a path, through code
/// references. `Ok(None)` when there is no such path.
fn resolve(tree: &str, relative: &str, into: &str) -> Result<Option<PathBuf>, String> {
    let output = std::process::Command::new("caos")
        .args(["resolve", tree, relative, into])
        .output()
        .map_err(|e| format!("launching caos resolve: {e}"))?;
    if output.status.success() {
        return Ok(Some(PathBuf::from(into)));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("no such path") {
        Ok(None)
    } else {
        Err(format!("resolving {relative}: {}", stderr.trim_end()))
    }
}

fn start() -> Result<(), String> {
    let comps = split_path(&input("path")?.unwrap_or_default());
    if comps.iter().any(|c| c == "..") {
        return failed("`..` is not allowed in tree paths");
    }
    let input = arg("in");
    let tree = cas_hash(&input)?;

    // The first code reference on the way down selects the tree the rest is
    // evaluated in. A prefix that is not in the stored tree ends the search: what
    // follows is evaluation's to make.
    let mut root = input.clone();
    let mut rest = comps.clone();
    for i in 0..comps.len() {
        let prefix = comps[..=i].join("/");
        let parent_into = format!("/cas/eval-parent-{i}");
        let Some(parent) = resolve(&tree, &comps[..i].join("/"), &parent_into)? else {
            break;
        };
        let entry = parent.join(&comps[i]);
        if !entry.exists() {
            break;
        }
        if cas_kind(path(&entry))? == "commit" {
            let into = format!("/cas/eval-root-{i}");
            if resolve(&tree, &prefix, &into)?.is_none() {
                break;
            }
            root = into;
            rest = comps[i + 1..].to_vec();
            break;
        }
    }

    let me = own_image();
    let bin = arg("worker1");
    let mut then_args: Vec<(&str, Arg)> = vec![("stage", Arg::Lit("report"))];
    if Path::new(&bin).exists() {
        then_args.push(("worker1", Arg::Path(&bin)));
    }
    let then = caos_curry(Arg::Path(&me), &then_args)?;
    eval_then_catching(&root, &rest.join("/"), Arg::Hash(&then))
}

fn report() -> Result<(), String> {
    let error = arg("error");
    if Path::new(&error).exists() {
        caos(["get", &error])?;
        let text = fs::read_to_string(&error).unwrap_or_default();
        return failed(text.trim());
    }
    let result = arg("result");
    let kind = cas_kind(&result)?;
    let hash = cas_hash(&result)?;
    match kind.as_str() {
        "blob" => {
            caos(["get", &result])?;
            let bytes = fs::read(&result).map_err(|e| format!("reading the result: {e}"))?;
            let shown = match String::from_utf8(bytes) {
                Ok(text) if text.len() <= MAX_BLOB_BYTES => text,
                Ok(_) => "(a text blob too large to show here)".to_string(),
                Err(_) => "(a binary blob)".to_string(),
            };
            answer(&format!("{kind} {hash}\n\n{shown}"))
        }
        _ => answer(&format!(
            "{kind} {hash}\n\nPass {hash} as `in` to read, ls or grep to look inside it, or to \
             eval-path to evaluate a path within it."
        )),
    }
}

/// An argument VERBATIM, or `None` if it was not passed.
fn input(name: &str) -> Result<Option<String>, String> {
    let file = arg(name);
    if !Path::new(&file).exists() {
        return Ok(None);
    }
    caos(["get", &file])?;
    fs::read_to_string(&file)
        .map(Some)
        .map_err(|e| format!("reading {name}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_splits_the_way_the_file_tools_split_it() {
        assert_eq!(
            split_path("/feature//dirty/./tests"),
            ["feature", "dirty", "tests"]
        );
        assert!(split_path("").is_empty());
    }
}
