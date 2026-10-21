//! The `grep` tool: validate the call, run the rgrep fold over the scope, render
//! its sparse result as `path:linenum:line`.
//!
//! Two stages, named by `--stage` (SPEC, "Worker scripts"), because a worker may
//! not block on a run: the first records a continuation and exits, and the server
//! calls the second with the fold's result.
//!
//! * absent: check the pattern compiles and the scope exists, then `run-then` the
//!   rgrep image (this tool's `--rgrep` arg) over the scope, with the pattern
//!   curried on, and this worker's own image as the `then`.
//! * `render`: walk the sparse tree rgrep returned (files are match blobs, `linenum:line` per line;
//!   subtrees recurse) and print the lines while they fit the budget, then count what
//!   is left and say how to narrow.
//!
//! A call the model got wrong is a VALUE, never a failed worker: a `report` that
//! opens `FAILED`, the convention a reader tool answers with.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, cas_hash, entries, file_name, own_image, path, read_arg_opt, run_then,
    run_worker, scratch, Arg,
};

/// Rendered output past which the rest is only counted, so one search cannot fill
/// a model's context.
const MAX_BYTES: usize = 100_000;

fn main() -> ExitCode {
    run_worker("grep", run)
}

fn run() -> Result<(), String> {
    match read_arg_opt("stage")?.as_deref() {
        None | Some("") => start(),
        Some("render") => render(),
        Some(other) => Err(format!("unknown stage {other:?}")),
    }
}

/// An argument VERBATIM: a pattern's leading or trailing space is part of it.
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

/// The answer for a call the model got wrong.
fn failed(message: &str) -> Result<(), String> {
    let dir = scratch("grep-out")?;
    fs::write(dir.join("report"), format!("FAILED: {message}\n"))
        .map_err(|e| format!("writing the report: {e}"))?;
    caos(["put", path(&dir), "/cas/out"])
}

fn answer(text: &str) -> Result<(), String> {
    let file = scratch("grep-out")?.join("answer");
    fs::write(&file, text).map_err(|e| format!("writing the answer: {e}"))?;
    caos(["put", path(&file), "/cas/out"])
}

/// A path argument's components: a leading `/` is tolerated, and empty and `.`
/// segments drop out.
fn split_path(raw: &str) -> Vec<String> {
    raw.trim()
        .trim_start_matches('/')
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_string)
        .collect()
}

fn start() -> Result<(), String> {
    let Some(pattern) = input("pattern")? else {
        return failed("grep needs a string `pattern`");
    };
    if let Err(error) = regex::Regex::new(&pattern) {
        return failed(&format!("invalid pattern: {error}"));
    }
    let comps = split_path(&input("path")?.unwrap_or_default());
    if comps.iter().any(|c| c == "..") {
        return failed("`..` is not allowed in tree paths");
    }
    let relative = comps.join("/");
    let scope = "/cas/grep-scope";
    let output = std::process::Command::new("caos")
        .args(["resolve", &cas_hash(&arg("in"))?, &relative, scope])
        .output()
        .map_err(|e| format!("launching caos resolve: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.trim_end();
        let reason = reason.strip_prefix("caos: ").unwrap_or(reason);
        return failed(&format!(
            "{}: {reason}",
            if relative.is_empty() {
                "the tree"
            } else {
                &relative
            }
        ));
    }

    // The fold is the rgrep image with the pattern bound. Its cache key is
    // (subtree, pattern), which is what makes a scoped search cheap.
    let rgrep = cas_hash(&arg("rgrep"))?;
    let fold = caos_curry(Arg::Hash(&rgrep), &[("pattern", Arg::Lit(&pattern))])?;

    // `own_image` is the UNWRAPPED base: curry layers expand into args before a
    // request is stored, so the `then` rebinds `worker1` (the binary this image
    // ships as, when it is a runner-pool one) beside what the render stage needs.
    let me = own_image();
    let bin = arg("worker1");
    let mut then_args: Vec<(&str, Arg)> = vec![
        ("stage", Arg::Lit("render")),
        ("scope", Arg::Lit(&relative)),
    ];
    if Path::new(&bin).exists() {
        then_args.push(("worker1", Arg::Path(&bin)));
    }
    let then = caos_curry(Arg::Path(&me), &then_args)?;
    run_then(scope, Arg::Hash(&fold), Some(Arg::Hash(&then)))
}

fn render() -> Result<(), String> {
    let scope = input("scope")?.unwrap_or_default();
    let result = arg("result");
    caos(["get", &result])?;
    let p = Path::new(&result);

    // A file-scoped search's result is the match blob itself.
    if p.is_file() {
        let text = fs::read_to_string(p).map_err(|e| format!("reading the matches: {e}"))?;
        if text.is_empty() {
            return answer("no matches");
        }
        let rendered: String = text.lines().map(|l| format!("{scope}:{l}\n")).collect();
        return answer(rendered.trim_end());
    }

    let mut out = Render {
        text: String::new(),
        overflow_files: 0,
    };
    let prefix = if scope.is_empty() {
        String::new()
    } else {
        format!("{scope}/")
    };
    out.walk(p, &prefix)?;
    if out.text.is_empty() && out.overflow_files == 0 {
        return answer("no matches");
    }
    let mut text = out.text;
    if out.overflow_files > 0 {
        text += &format!(
            "\n[truncated — {} more matching file(s); narrow the pattern or search a \
             subdirectory]",
            out.overflow_files
        );
    }
    answer(text.trim_end())
}

struct Render {
    text: String,
    /// Matching files not rendered once the budget was hit.
    overflow_files: usize,
}

impl Render {
    /// Depth-first over the sparse tree: files are match blobs (`linenum:line`
    /// per line), subtrees recurse. Past [`MAX_BYTES`] of output, stop reading
    /// contents and just count matching files.
    fn walk(&mut self, dir: &Path, prefix: &str) -> Result<(), String> {
        caos(["get", path(dir)])?;
        for child in entries(path(dir))? {
            let name = file_name(&child);
            if child.is_dir() {
                self.walk(&child, &format!("{prefix}{name}/"))?;
                continue;
            }
            if self.text.len() >= MAX_BYTES {
                self.overflow_files += 1;
                continue;
            }
            caos(["get", path(&child)])?;
            let text = fs::read_to_string(&child)
                .map_err(|e| format!("reading {}: {e}", child.display()))?;
            for line in text.lines() {
                self.text.push_str(&format!("{prefix}{name}:{line}\n"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_splits_the_way_the_file_tools_split_paths() {
        assert_eq!(split_path("/a//b/./c"), ["a", "b", "c"]);
        assert!(split_path(" . ").is_empty());
    }
}
