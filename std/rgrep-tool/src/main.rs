//! caos-worker-rgrep-tool: the agent harness's grep tool.
//!
//! `std/rgrep` is the fold — one job per directory, a sparse result tree cached
//! per (subtree, pattern). This is the TOOL over it: it resolves the scope,
//! runs the fold, and flattens the sparse tree into the `path:linenum:line` a
//! model reads. The workspace arrives as `--in` and the model's arguments as
//! curried `--pattern`/`--path`, which is what `launch_std_tool` hands any std
//! tool; the result is the ordinary `{report}` tree every caos tool returns, so
//! every caller renders it through the same generic path
//! (`tree_tool_result_block`, `report_conventions`) with no grep-specific code.
//!
//! That last part is the reason this exists. The flattening used to live in the
//! callers: llm-step carried `grep_precheck`/`grep_result_block`/`GrepRender`
//! and a bespoke `launch_grep` continuation, and the Claude Code tool server
//! grew a SECOND copy of the same walk. One user-visible contract, two
//! implementations, and neither reachable by `run-tool`.
//!
//! Two stages, named by `--stage` (SPEC, "Worker scripts"):
//!
//! * absent — validate the pattern, resolve `path` within `tree`, and
//!   `run-then` the fold over that scope, currying ourselves into `then`.
//! * `render` — flatten the fold's sparse result into the report.
//!
//! The pattern is validated HERE rather than in each caller: it is the tool's
//! own contract, and a caller that had to pre-check it would be reimplementing
//! the tool's argument handling in order to call it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, entries, file_name, own_image, path, read_arg, read_arg_opt, run_then,
    run_worker, scratch, Arg,
};

/// Rendering stops here, so one broad pattern cannot flood a model's context.
/// What is past it is COUNTED, never silently dropped: the count is what tells
/// the caller its pattern was too broad.
const MAX_REPORT_BYTES: usize = 100_000;

fn main() -> ExitCode {
    run_worker("rgrep-tool", run)
}

fn run() -> Result<(), String> {
    match read_arg_opt("stage")?.as_deref() {
        None | Some("") => launch(),
        Some("render") => render(),
        Some(other) => Err(format!("unknown stage {other:?}")),
    }
}

/// Resolve the scope and hand it to the fold.
///
/// A bad pattern or a missing path is a REPORT, not a worker error: the caller
/// is a model that must see what it got wrong and try again, and failing the
/// run would take the whole turn down instead.
fn launch() -> Result<(), String> {
    let pattern = read_arg("pattern")?;
    if let Err(error) = regex::Regex::new(&pattern) {
        return failed(&format!("invalid pattern {pattern:?}: {error}"));
    }
    let scope = read_arg_opt("path")?.unwrap_or_default();
    let scope = scope.trim().trim_matches('/').to_string();

    // `in` is the workspace: `launch_std_tool` run-thens the tool over it, so
    // this is the same input every std tool receives.
    let tree = arg("in");
    caos(["get", &tree])?;
    let mut target = PathBuf::from(&tree);
    for component in scope.split('/').filter(|c| !c.is_empty() && *c != ".") {
        caos(["get", path(&target)])?;
        if !target.is_dir() {
            return failed(&format!("{scope}: a parent is a file, not a directory"));
        }
        target = target.join(component);
        if !target.exists() {
            return failed(&format!("no such path: {scope}"));
        }
    }
    caos(["get", path(&target)])?;

    let fold = caos_curry(Arg::Path(&arg("fold")), &[("pattern", Arg::Lit(&pattern))])?;
    // `own_image` is the UNWRAPPED base image: curry layers (including the
    // runner-pool `bin` binding this worker ships as) expand into args before a
    // request is stored, so the continuation rebinds what it needs by hand.
    let me = own_image();
    let bin = arg("worker1");
    let mut then_kvs: Vec<(&str, Arg)> = vec![
        ("stage", Arg::Lit("render")),
        ("scope", Arg::Lit(&scope)),
    ];
    if Path::new(&bin).exists() {
        then_kvs.push(("worker1", Arg::Path(&bin)));
    }
    let then = caos_curry(Arg::Path(&me), &then_kvs)?;
    run_then(path(&target), Arg::Hash(&fold), Some(Arg::Hash(&then)))
}

/// Flatten the fold's sparse result into `path:linenum:line`.
fn render() -> Result<(), String> {
    let scope = read_arg_opt("scope")?.unwrap_or_default();
    let result = arg("result");
    caos(["get", &result])?;
    let node = Path::new(&result);

    // A file-scoped grep's result is the match blob itself: `linenum:line`
    // lines that only this stage can prefix, because only it knows what was
    // scoped. The blob does not carry its own path.
    if node.is_file() {
        let text = fs::read_to_string(node).map_err(|e| format!("reading {result}: {e}"))?;
        let rendered: String = text.lines().map(|l| format!("{scope}:{l}\n")).collect();
        return report(&rendered);
    }

    let prefix = match scope.is_empty() {
        true => String::new(),
        false => format!("{scope}/"),
    };
    let mut render = Render {
        out: String::new(),
        overflow_files: 0,
    };
    render.walk(node, &prefix)?;
    let mut text = render.out;
    if render.overflow_files > 0 {
        text += &format!(
            "\n[truncated — {} more matching file(s); narrow the pattern or grep a \
             subdirectory]",
            render.overflow_files
        );
    }
    report(&text)
}

struct Render {
    out: String,
    /// Matching files not rendered once the budget was spent.
    overflow_files: usize,
}

impl Render {
    /// Depth-first over the sparse tree: files are match blobs (`linenum:line`
    /// per line), subtrees recurse. Past the budget, stop reading contents and
    /// just count matching files.
    fn walk(&mut self, dir: &Path, prefix: &str) -> Result<(), String> {
        caos(["get", path(dir)])?;
        for child in entries(path(dir))? {
            let name = file_name(&child);
            if child.is_dir() {
                self.walk(&child, &format!("{prefix}{name}/"))?;
                continue;
            }
            if self.out.len() >= MAX_REPORT_BYTES {
                self.overflow_files += 1;
                continue;
            }
            caos(["get", path(&child)])?;
            let text = fs::read_to_string(&child)
                .map_err(|e| format!("reading {}: {e}", child.display()))?;
            for line in text.lines() {
                self.out.push_str(&format!("{prefix}{name}:{line}\n"));
            }
        }
        Ok(())
    }
}

/// The result: a tree carrying one `report` blob, which is what makes this an
/// ordinary caos tool rather than something callers must special-case.
fn report(text: &str) -> Result<(), String> {
    let body = match text.trim_end() {
        "" => "no matches".to_string(),
        trimmed => trimmed.to_string(),
    };
    let dir = scratch("rgrep-tool-out")?;
    fs::write(dir.join("report"), format!("{body}\n"))
        .map_err(|e| format!("writing report: {e}"))?;
    caos(["put", path(&dir), "/cas/out"])
}

/// A user mistake, in the one form every caller already understands.
///
/// `tree_tool_result_block` marks a report containing `FAILED` as an `is_error`
/// tool_result, so the model sees its mistake and retries; `report_conventions`
/// makes `run-tool` exit non-zero on the same word. Reporting rather than
/// erroring is what keeps a bad pattern from taking down the turn.
fn failed(message: &str) -> Result<(), String> {
    report(&format!("FAILED: {message}"))
}
