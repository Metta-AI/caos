//! Inline file tools — `read`, `ls`, `write`, `edit` — executed in-process by
//! the step worker (design/agent-harness.md, "Tool classes"): hash-level
//! workspace operations that need no sub-run, no container, no dispatch.
//! Reads materialize only the path they touch; writes rebuild the tree by
//! symlinking every untouched entry and `caos put`ting the result (staging
//! resolves links by recorded hash — the same surgery `mint_step` does for
//! `.caos`), so the never-materialize rule holds throughout.
//!
//! A failed call — missing file, non-unique `old-string`, a file where a
//! directory was expected — is an `is_error` tool_result the model reacts to,
//! never a worker error. Parameter shapes mirror Claude Code's file tools
//! (`file-path`, `content`, `old-string`/`new-string`/`replace-all`), which
//! models know well.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use worker_common::{caos, entries, file_name, link, path, scratch};

use crate::{fresh, fresh_name, result_block};

/// The reserved workspace entry (step transcripts); refused in tool paths.
const STEP_DIR: &str = ".caos";

/// Reads larger than this are truncated (with a note) unless `offset`/`limit`
/// narrow them; `ls` listings cap at [`MAX_ENTRIES`] the same way.
const MAX_READ_BYTES: usize = 100_000;
const MAX_ENTRIES: usize = 1_000;

/// True if `name` is one of the inline tools this module executes.
pub fn is_inline(name: &str) -> bool {
    matches!(name, "read" | "ls" | "write" | "edit")
}

/// Help text for the built-in tools, authored exactly like a caos-tools
/// `.caos-expr`'s `HELP` here-string (SPEC, "Tools"): free description text,
/// then dashed `@param` tags. They are parsed by the same `parse_help` the tree
/// tools use, so a built-in and a project tool are described one way — the docs
/// live with the tool, not inside a hand-written JSON schema.
const READ_HELP: &str = "Read a file's contents. Defaults to the current workspace; pass `root` — a commit, tree, or blob hash (one printed by `log`/`show`/`diff`, or a stage oid from `.caos/conflicts`) — to read as of another revision. With a commit or tree `root`, `file-path` names the file within it; with a blob `root`, omit `file-path` to read the blob directly. Prefer this over `cat` via bash — it is immediate and needs no `paths` declaration. Large files are truncated; use `offset`/`limit` (line-based) to page.
@param [file-path] Workspace-relative path (the workspace root is the repo root).
@param [root] Optional commit/tree/blob hash to read from — an older revision, or a bare blob (e.g. a `.caos/conflicts` stage oid). Omit for the current workspace.
@param [offset] 1-based first line to return.
@param [limit] Number of lines to return.";

const LS_HELP: &str = "List a directory: one entry per line, directories with a trailing `/`. Defaults to the current workspace; pass `root` (a commit or tree hash) to list it as of another revision, and `path` to descend within that root. Prefer this over `ls` via bash.
@param [path] Directory to list (relative to `root`, or to the workspace root); omit for the root itself.
@param [root] Optional commit or tree hash to list as of another revision. Omit for the current workspace.";

const WRITE_HELP: &str = "Write a file into the workspace (creating parent directories, overwriting an existing file). Prefer this over heredocs/redirection via bash.
@param file-path Workspace-relative path (the workspace root is the repo root).
@param content The full new file content.";

const EDIT_HELP: &str = "Replace text in a workspace file. `old-string` must match the file content exactly and (unless `replace-all`) appear exactly once — include surrounding context to disambiguate. Prefer this over sed via bash.
@param file-path Workspace-relative path (the workspace root is the repo root).
@param old-string Exact text to replace.
@param new-string Replacement text.
@param [replace-all] Replace every occurrence (default false).";

const GREP_HELP: &str = "Search the workspace with a regular expression (Rust regex syntax, line-based). Returns matches as `path:linenum:line`. Scope with `path` (a directory or file) to narrow the search; results are cached per unchanged subtree, so repeated and scoped greps are cheap. Pass `root` (a commit or tree hash) to search as of another revision. Prefer this over grep/find via bash.
@param pattern The regular expression to search for.
@param [path] Directory or file to search (relative to `root`, or to the workspace root); omit for everything.
@param [root] Optional commit or tree hash to search as of another revision. Omit for the current workspace.";

/// Build a built-in tool's registry entry from its help text, through the very
/// same `parse_help` → `tree_tool_declaration` path a discovered caos-tools
/// tool takes. History tools in `githist.rs` use the same builder with `@git`.
pub(crate) fn builtin_tool(name: &str, help: &str) -> TreeTool {
    let (doc, args, git) = parse_help(&format!("built-in {name}"), help);
    TreeTool {
        name: name.to_string(),
        doc,
        args,
        git,
    }
}

fn builtin_declaration(name: &str, help: &str) -> Value {
    tree_tool_declaration(&builtin_tool(name, help))
}

/// The inline tools' registry entries, alongside `bash`'s.
pub fn declarations() -> Vec<Value> {
    [
        ("read", READ_HELP),
        ("ls", LS_HELP),
        ("write", WRITE_HELP),
        ("edit", EDIT_HELP),
    ]
    .iter()
    .map(|(name, help)| builtin_declaration(name, help))
    .collect()
}

/// The grep tool's registry entry (present only when a `grep-image` is
/// curried — see `Config`). It runs as the rgrep fold sub-run; this module
/// contributes the declaration, the pre-launch validation, and the
/// transcript-boundary rendering of its sparse result tree.
pub fn grep_declaration() -> Value {
    builtin_declaration("grep", GREP_HELP)
}

// ---- Tree tools (caos-tools/<name>/, SPEC "Tools") -------------------------

/// Reserved built-in tool names a tree tool may not shadow: the model's
/// primitives (including the repair path for a broken tool edit — bash and
/// the file tools) must stay stable whatever the tree carries, and the
/// built-in history tools (`log`/`show`/`diff` — see `githist.rs`) are
/// standard, not project-defined.
const RESERVED_TOOLS: &[&str] = &[
    "bash",
    "grep",
    "read",
    "ls",
    "write",
    "edit",
    "log",
    "show",
    "diff",
    "caos-build",
    "caos-test",
    "caos-test-result",
    "spawn_agent",
    "wait_agent",
    "harvest_agent",
    "run_async",
];

/// The tree's tool directory (`caos-tools/` in the workspace), expanded one
/// level; `None` when the tree defines no tools.
fn tree_tools_dir(ws: &str) -> Result<Option<String>, String> {
    caos(["get", ws])?;
    let dir = format!("{ws}/caos-tools");
    if !Path::new(&dir).is_dir() {
        return Ok(None);
    }
    caos(["get", &dir])?;
    Ok(Some(dir))
}

/// Arg names a tree tool may not declare: the interpreter binds these itself
/// on every tool run, and `caos curry` errors on a rebind (SPEC, "Currying").
/// `wc`/`refs` are bound only for `@git` tools, but reserved unconditionally
/// so a tool can't declare a model arg the interpreter would then clobber.
///
/// EVERY NAME HERE IS ONE SOMETHING BINDS. `std` used to be on this list and is
/// not any more: there is no `std` arg, a dependency rides inside the tree as a
/// `DEEP-DEPS/<name>` mount, so nothing would ever have collided with a tool
/// declaring one. Reserving a name for its history costs a tool author a
/// perfectly good parameter and tells the next reader that something binds it.
const RESERVED_ARGS: &[&str] = &[
    "in", "worker1", "base", "salt", "wc", "refs",
    // The tool's own ArgTree binds `help` (SPEC, "Tools"), and `caos curry`
    // refuses to rebind — so a tool declaring `@param help` would fail at
    // invocation rather than here, where the model can be told why.
    "help",
];

/// One tree tool as the registry sees it: its name, its description, and the
/// parameters it accepts — parsed from its javadoc `help` (SPEC, "Tools").
pub struct TreeTool {
    pub name: String,
    pub doc: String,
    pub args: Vec<TreeArg>,
    /// The tool declared `@git`: bind the workspace commit (`wc`) and the
    /// turn's ref snapshot (`refs`) so it can walk history. Off by default —
    /// `wc` changes every step, so binding it into a tool that doesn't need it
    /// (build/test) would turn every cache hit into a miss.
    pub git: bool,
}

/// One `@param` tag: `@param <name> <description>` is required, `@param
/// [<name>] <description>` optional. The name becomes the script's `--<name>`
/// arg, readable at `/cas/args/<name>`.
pub struct TreeArg {
    pub name: String,
    pub doc: String,
    pub required: bool,
}

/// Parse one `@param` tag's payload (everything after the tag) into a
/// parameter. `None` — reported by the caller — for a malformed name, so a
/// typo costs a visible skip rather than an arg the model can't use.
fn parse_arg(payload: &str) -> Option<TreeArg> {
    let (token, doc) = match payload.split_once(char::is_whitespace) {
        Some((t, d)) => (t, d.trim()),
        None => (payload, ""),
    };
    let (name, required) = match token.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        Some(inner) => (inner, false),
        None => (token, true),
    };
    let ok = !name.is_empty()
        && !RESERVED_ARGS.contains(&name)
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then(|| TreeArg {
        name: name.to_string(),
        doc: doc.to_string(),
        required,
    })
}

/// Parse a tool's `help` string as a JAVADOC comment (SPEC, "Tools"): the free
/// text before the first block tag is the description; `@param <name>` /
/// `@param [<name>]` tags declare the parameters; a bare `@git` tag asks for the
/// history context. Returns `(description, params, git)` — an empty description
/// for the caller to placeholder. A malformed `@param` is skipped with a
/// message. This is the DURABLE parser: Phase 4 feeds it the isolated `--help`
/// here-string; today it is fed text lifted from the script header (below).
fn parse_help(ctx: &str, text: &str) -> (String, Vec<TreeArg>, bool) {
    let mut doc: Vec<&str> = Vec::new();
    let mut args = Vec::new();
    let mut git = false;
    let mut in_tags = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("@param") {
            in_tags = true;
            match parse_arg(rest.trim()) {
                Some(a) => args.push(a),
                None => eprintln!("{ctx}: unusable @param tag: {line}"),
            }
        } else if trimmed == "@git" {
            in_tags = true;
            git = true;
        } else if !in_tags {
            // Description text — everything before the first block tag.
            doc.push(trimmed);
        }
    }
    (doc.join(" ").trim().to_string(), args, git)
}

/// The `help` string a tool's `.caos-expr` binds, read from the EXPRESSION'S
/// OWN TEXT: a `HELP=<<END … END` here-string whose variable the value line
/// passes as `--help=$HELP`, or a one-line `--help=<literal>`.
///
/// **Read, not evaluated, and that is the point.** Evaluating a tool would
/// dispatch the runs its expression names (a compiled tool builds), and a
/// worker may not block on a run — discovery happens mid-turn, in the middle of
/// this worker's function, where there is no continuation to tail-call into. So
/// listing reads the bytes the expression authors, and INVOCATION evaluates
/// (`eval-path-then`). The two agree because they read the same here-string:
/// the arg tree's `--help` is this text.
///
/// `None` when the expression binds no `help` — a directory that is not a tool,
/// or a tool whose docs went missing; the caller says which and skips it.
fn expr_help(expr: &str) -> Option<String> {
    let mut here: Vec<(String, String)> = Vec::new();
    let mut value_lines: Vec<&str> = Vec::new();
    let lines: Vec<&str> = expr.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `NAME=<<TERM` opens a here-string: the body runs to a line equal to
        // TERM, exactly as the evaluator reads it (design/caos-expr.md).
        if let Some((name, term)) = line.split_once("=<<") {
            if !term.is_empty() && !term.contains(char::is_whitespace) {
                let mut body: Vec<&str> = Vec::new();
                while i < lines.len() && lines[i].trim() != term {
                    body.push(lines[i]);
                    i += 1;
                }
                i += 1; // the terminator
                here.push((name.to_string(), body.join("\n")));
                continue;
            }
        }
        value_lines.push(lines[i - 1]);
    }
    // `--help=…` on any command line: a `$VAR` names a here-string above, and
    // anything else is the literal itself.
    for line in value_lines {
        for tok in line.split_whitespace() {
            let Some(value) = tok.strip_prefix("--help=") else {
                continue;
            };
            return match value.strip_prefix('$') {
                Some(var) => here
                    .iter()
                    .find(|(name, _)| name == var)
                    .map(|(_, body)| body.clone()),
                None => Some(value.to_string()),
            };
        }
    }
    None
}

/// Read one tool directory into its registry shape: the `help` its
/// `.caos-expr` binds, parsed into a description and parameters.
fn read_tool(name: &str, dir: &str) -> Result<Option<TreeTool>, String> {
    // Materialize the directory's own entries first. A `caos get` is SHALLOW:
    // the parent fetch made this directory exist, but nothing inside it —
    // without this, every tool looks like a directory with no `.caos-expr` and
    // the whole registry comes back empty (it did).
    caos(["get", dir])?;
    let path = format!("{dir}/.caos-expr");
    if !Path::new(&path).is_file() {
        return Ok(None);
    }
    caos(["get", &path])?;
    let text = fs::read_to_string(&path).map_err(|e| format!("reading {path}: {e}"))?;
    let Some(help) = expr_help(&text) else {
        eprintln!("caos-tools/{name}/.caos-expr binds no --help — not registered as a tool");
        return Ok(None);
    };
    let (doc, args, git) = parse_help(&format!("caos-tools/{name}/.caos-expr"), &help);
    let doc = if doc.is_empty() {
        format!("Project tool caos-tools/{name} (no description).")
    } else {
        doc
    };
    Ok(Some(TreeTool {
        name: name.to_string(),
        doc,
        args,
        git,
    }))
}

/// Discover the tree-defined tools: each CHILD DIRECTORY of `caos-tools/` that
/// carries a `.caos-expr`, described by the `help` that expression binds.
/// Resolved fresh from the CURRENT workspace every round, so an agent that
/// adds, edits, or removes a tool sees the change on its next request.
/// Reserved names are skipped loudly; a directory with no `.caos-expr`, or one
/// whose expression binds no `--help`, is not a tool.
pub fn tree_tools(ws: &str) -> Result<Vec<TreeTool>, String> {
    let Some(dir) = tree_tools_dir(ws)? else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| format!("reading {dir}: {e}"))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();

    let mut out = Vec::new();
    for name in names {
        let p = format!("{dir}/{name}");
        if !Path::new(&p).is_dir() {
            continue;
        }
        if RESERVED_TOOLS.contains(&name.as_str()) {
            eprintln!("caos-tools/{name} shadows the built-in {name:?} tool — ignored");
            continue;
        }
        if let Some(tool) = read_tool(&name, &p)? {
            out.push(tool);
        }
    }
    Ok(out)
}

/// A harness-provided std tool (std/caos-build, std/caos-test), described by the
/// `help` its curried IMAGE carries — read the same way a tree tool's help is,
/// so a built-in std tool and a project caos-tools tool are one mechanism, just
/// sourced differently. `dir` is the materialized arg-tree path
/// (`/cas/args/<name>-image`): a curry with only a base is the tool's ready
/// ArgTree, whose `help` entry is the blob the tool's `.caos-expr` bound.
///
/// `None` when the image carries no `help` — treated as a configuration error by
/// the caller, since the harness itself curried the image.
pub fn std_tool(name: &str, dir: &str) -> Result<Option<TreeTool>, String> {
    caos(["get", dir])?;
    let help_path = format!("{dir}/help");
    if !Path::new(&help_path).exists() {
        return Ok(None);
    }
    caos(["get", &help_path])?;
    let help = fs::read_to_string(&help_path).map_err(|e| format!("reading {help_path}: {e}"))?;
    let (doc, args, git) = parse_help(&format!("std/{name}"), &help);
    let doc = if doc.is_empty() {
        format!("Built-in tool {name} (no description).")
    } else {
        doc
    };
    Ok(Some(TreeTool {
        name: name.to_string(),
        doc,
        args,
        git,
    }))
}

/// One discovered tool's registry entry. A tool with no `@param` tags takes
/// no parameters — the workspace tree IS its input — and one with them takes
/// them as strings, since every arg reaches the script as a `/cas/args/<name>`
/// blob whatever JSON type it left the model as.
pub fn tree_tool_declaration(tool: &TreeTool) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for a in &tool.args {
        props.insert(
            a.name.clone(),
            json!({"type": "string", "description": a.doc}),
        );
        if a.required {
            required.push(Value::String(a.name.clone()));
        }
    }
    let mut schema = json!({"type": "object", "properties": Value::Object(props)});
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    json!({
        "name": tool.name,
        "description": tool.doc,
        "input_schema": schema
    })
}

/// Resolve tool `name` in the CURRENT workspace — invocation-time lookup, so a
/// call made right after an edit runs the edited tool. Returns the tool's
/// registry entry; its ArgTree comes from EVALUATING `caos-tools/<name>`, which
/// only the server can do for a worker (see `launch_tree_tool`). `None` when
/// the tree doesn't define it (or the name is reserved / not a clean name).
pub fn tree_tool(ws: &str, name: &str) -> Result<Option<TreeTool>, String> {
    if RESERVED_TOOLS.contains(&name) || name.contains('/') || name.contains("..") {
        return Ok(None);
    }
    let Some(dir) = tree_tools_dir(ws)? else {
        return Ok(None);
    };
    let p = format!("{dir}/{name}");
    if !Path::new(&p).is_dir() {
        return Ok(None);
    }
    read_tool(name, &p)
}

/// Bind a tree-tool call's inputs to the parameters the script declared,
/// returning the `--<name>=<value>` pairs for the curry. A missing required
/// arg, an undeclared one, or a non-scalar value is the model's mistake, so it
/// comes back as a ready-made `is_error` tool_result rather than a worker
/// error — the same contract `grep_precheck` uses.
pub fn tree_tool_args(call: &Value, tool: &TreeTool) -> Result<Vec<(String, String)>, Value> {
    let id = call["id"].as_str().unwrap_or("");
    let fail = |msg: String| Err(result_block(id, &msg, true));
    let empty = serde_json::Map::new();
    let input = call["input"].as_object().unwrap_or(&empty);
    for key in input.keys() {
        if !tool.args.iter().any(|a| &a.name == key) {
            let known: Vec<&str> = tool.args.iter().map(|a| a.name.as_str()).collect();
            return fail(format!(
                "{} takes no {key:?} argument (declared: {})",
                tool.name,
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
    }
    let mut out = Vec::new();
    for a in &tool.args {
        let value = match input.get(&a.name) {
            None | Some(Value::Null) => {
                if a.required {
                    return fail(format!("{} needs a {:?} argument", tool.name, a.name));
                }
                continue;
            }
            Some(Value::String(s)) => s.clone(),
            Some(v @ (Value::Number(_) | Value::Bool(_))) => v.to_string(),
            Some(_) => {
                return fail(format!("{}'s {:?} must be a string", tool.name, a.name));
            }
        };
        out.push((a.name.clone(), value));
    }
    Ok(out)
}

/// The tool_result block for a tree tool's result — a VALUE whose shape the
/// tool chose, rendered by `caos-cli run-tool`'s conventions: a tree with a
/// `report` shows the report (a FAILED banner renders `is_error`); a plain
/// blob shows its text; any other tree shows its top-level listing.
pub fn tree_tool_result_block(id: &str, result: &str) -> Result<Value, String> {
    caos(["get", result])?;
    let p = Path::new(result);
    let (mut text, is_err) = if p.is_dir() {
        let report = p.join("report");
        if report.exists() {
            caos(["get", path(&report)])?;
            let text = fs::read_to_string(&report)
                .map_err(|e| format!("reading {}: {e}", report.display()))?;
            let failed = text.contains("FAILED");
            (text, failed)
        } else {
            let mut names: Vec<String> = fs::read_dir(p)
                .map_err(|e| format!("reading {}: {e}", p.display()))?
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect();
            names.sort();
            (format!("result tree: {}", names.join(" ")), false)
        }
    } else {
        let bytes = fs::read(p).map_err(|e| format!("reading {}: {e}", p.display()))?;
        (String::from_utf8_lossy(&bytes).into_owned(), false)
    };
    if text.len() > MAX_READ_BYTES {
        // Keep the tail: reports and diagnostics put the summary last.
        let mut cut = text.len() - MAX_READ_BYTES;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text = format!("[... truncated ...]\n{}", &text[cut..]);
    }
    Ok(result_block(id, text.trim_end(), is_err))
}

/// Validate a grep call before its sub-run launches: the pattern must compile
/// and the scope must exist. Returns the scope's CAS path and its
/// workspace-relative prefix (`""` for the root) — or, on a user mistake, the
/// ready-made `is_error` tool_result.
pub fn grep_precheck(call: &Value, ws: &str) -> Result<(String, String), Value> {
    let id = call["id"].as_str().unwrap_or("");
    let fail = |msg: String| Err(result_block(id, &msg, true));
    let Some(pattern) = call["input"]["pattern"].as_str() else {
        return fail("grep needs a string `pattern`".to_string());
    };
    if let Err(e) = regex::Regex::new(pattern) {
        return fail(format!("invalid pattern: {e}"));
    }
    let root = opt_hash(call, "root");
    let comps = match components_opt(call, "path") {
        Ok(c) => c,
        Err(User(msg)) => return fail(msg),
        Err(Infra(e)) => return fail(e),
    };
    // `resolve` handles all four cases: no root + no path is the workspace
    // root; a `root` hash roots the search at another revision's tree.
    match resolve(root.as_deref(), ws, &comps) {
        Ok(p) => Ok((p.to_string_lossy().into_owned(), comps.join("/"))),
        Err(User(msg)) => fail(msg),
        Err(Infra(e)) => fail(e),
    }
}

/// The tool_result block for a finished grep: walk the sparse result tree and
/// render classic `path:linenum:line` lines while they fit the transcript
/// budget; past it, count the remaining matching files and say how to narrow.
pub fn grep_result_block(id: &str, result: &str, scope: &str) -> Result<Value, String> {
    let _ = caos(["get", result]);
    let p = Path::new(result);

    // A file-scoped grep's result is the match blob itself.
    if p.is_file() {
        let text = fs::read_to_string(p).map_err(|e| format!("reading {result}: {e}"))?;
        if text.is_empty() {
            return Ok(result_block(id, "no matches", false));
        }
        let rendered: String = text.lines().map(|l| format!("{scope}:{l}\n")).collect();
        return Ok(result_block(id, rendered.trim_end(), false));
    }

    let mut render = GrepRender {
        out: String::new(),
        overflow_files: 0,
    };
    let prefix = if scope.is_empty() {
        String::new()
    } else {
        format!("{scope}/")
    };
    render.walk(p, &prefix)?;
    if render.out.is_empty() && render.overflow_files == 0 {
        return Ok(result_block(id, "no matches", false));
    }
    let mut text = render.out;
    if render.overflow_files > 0 {
        text += &format!(
            "\n[truncated — {} more matching file(s); narrow the pattern or grep a \
             subdirectory]",
            render.overflow_files
        );
    }
    Ok(result_block(id, text.trim_end(), false))
}

struct GrepRender {
    out: String,
    /// Matching files not rendered once the budget was hit.
    overflow_files: usize,
}

impl GrepRender {
    /// Depth-first over the sparse tree: files are match blobs (`linenum:line`
    /// per line), subtrees recurse. Past [`MAX_READ_BYTES`] of output, stop
    /// reading contents and just count matching files.
    fn walk(&mut self, dir: &Path, prefix: &str) -> Result<(), String> {
        let _ = caos(["get", path(dir)]);
        for child in entries(path(dir))? {
            let name = file_name(&child);
            if child.is_dir() {
                self.walk(&child, &format!("{prefix}{name}/"))?;
                continue;
            }
            if self.out.len() >= MAX_READ_BYTES {
                self.overflow_files += 1;
                continue;
            }
            let _ = caos(["get", path(&child)]);
            let text = fs::read_to_string(&child)
                .map_err(|e| format!("reading {}: {e}", child.display()))?;
            for line in text.lines() {
                self.out.push_str(&format!("{prefix}{name}:{line}\n"));
            }
        }
        Ok(())
    }
}

/// A tool call's failure mode: `User` becomes an `is_error` tool_result the
/// model reacts to; `Infra` fails the worker (CAS/transport trouble).
enum Fail {
    User(String),
    Infra(String),
}

use Fail::{Infra, User};

impl Fail {
    fn from_infra(e: String) -> Fail {
        Infra(e)
    }
}

/// Execute one inline call against the workspace at CAS path `ws`. Returns the
/// tool_result block and, for a mutation, the new workspace CAS path.
pub fn execute(call: &Value, ws: &str) -> Result<(Value, Option<String>), String> {
    let id = call["id"].as_str().unwrap_or("");
    let name = call["name"].as_str().unwrap_or("");
    let outcome = match name {
        "read" => read(call, ws).map(|text| (text, None)),
        "ls" => ls(call, ws).map(|text| (text, None)),
        "write" => write(call, ws).map(|(text, new_ws)| (text, Some(new_ws))),
        "edit" => edit(call, ws).map(|(text, new_ws)| (text, Some(new_ws))),
        other => Err(User(format!("unknown inline tool {other:?}"))),
    };
    match outcome {
        Ok((text, new_ws)) => Ok((result_block(id, &text, false), new_ws)),
        Err(User(msg)) => Ok((result_block(id, &msg, true), None)),
        Err(Infra(e)) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// The four tools.
// ---------------------------------------------------------------------------

fn read(call: &Value, ws: &str) -> Result<String, Fail> {
    let root = opt_hash(call, "root");
    let comps = components_opt(call, "file-path")?;
    if root.is_none() && comps.is_empty() {
        return Err(User(
            "read needs a `file-path` (or a `root` blob hash to read directly)".to_string(),
        ));
    }
    let p = resolve(root.as_deref(), ws, &comps)?;
    if p.is_dir() {
        let what = if comps.is_empty() {
            "that root is a tree".to_string()
        } else {
            format!("{} is a directory", comps.join("/"))
        };
        return Err(User(format!("{what}; use ls")));
    }
    let bytes = fs::read(&p).map_err(|e| Infra(format!("reading {}: {e}", p.display())))?;
    bounded(&bytes, call)
}

/// Resolve `(root, path)` to a materialized node. `root` `None` reads the
/// current workspace tree `ws`; a `root` hash may name a TREE (navigate into
/// it), a COMMIT (navigate into its tree), or a BLOB (a leaf — valid only with
/// no `path`). This is the one place history reads root elsewhere; `read` and
/// `ls` share it, then each checks the node is the kind it wants.
fn resolve(root: Option<&str>, ws: &str, comps: &[String]) -> Result<PathBuf, Fail> {
    let Some(hash) = root else {
        return materialize(ws, comps);
    };
    if !valid_oid(hash) {
        return Err(User(format!("{hash:?} is not a git object hash")));
    }
    let dst = fresh("root");
    caos(["get-hash", hash, &dst]).map_err(|e| User(format!("cannot read {hash}: {e}")))?;
    let p = PathBuf::from(&dst);
    if p.is_dir() {
        // A tree: navigate straight into it.
        return materialize(&dst, comps);
    }
    // A non-tree object materializes as a file — a commit (navigate into its
    // tree) or a blob (a leaf).
    let _ = caos(["get", &dst]);
    let bytes = fs::read(&p).map_err(|e| Infra(format!("reading {}: {e}", p.display())))?;
    if let Some(tree) = commit_tree_of(&bytes) {
        let tdst = fresh("root");
        caos(["get-hash", &tree, &tdst])
            .map_err(|e| User(format!("cannot read tree {tree}: {e}")))?;
        return materialize(&tdst, comps);
    }
    // A blob: it is the node, but only if no path was asked for.
    if comps.is_empty() {
        Ok(p)
    } else {
        Err(User(format!(
            "{hash} is a blob; it has no paths inside it (drop `file-path`/`path` to read it)"
        )))
    }
}

/// If `bytes` is a git commit object, its `tree` hash. A commit's header (up to
/// the first blank line) carries both a `tree <oid>` and an `author ` line;
/// requiring both keeps a blob that merely starts with "tree " from passing.
fn commit_tree_of(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let header = text.split("\n\n").next().unwrap_or("");
    let mut tree = None;
    let mut has_author = false;
    for line in header.lines() {
        if let Some(h) = line.strip_prefix("tree ").map(str::trim) {
            if valid_oid(h) {
                tree = Some(h.to_string());
            }
        } else if line.starts_with("author ") {
            has_author = true;
        }
    }
    has_author.then_some(tree).flatten()
}

/// An optional hash-valued input (`root`): trimmed, empty treated as absent.
fn opt_hash(call: &Value, key: &str) -> Option<String> {
    call["input"][key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A git object hash: hex, 40 (sha1) or 64 (sha256) chars.
fn valid_oid(oid: &str) -> bool {
    (oid.len() == 40 || oid.len() == 64) && oid.chars().all(|c| c.is_ascii_hexdigit())
}

/// Apply `read`'s bounds to raw bytes: a line window when `offset`/`limit` is
/// set, else a head-truncation at [`MAX_READ_BYTES`].
/// Read an integer arg tolerantly — the model may send it as a JSON number or,
/// since the built-in tools' `@param`s are untyped strings, as a numeric
/// string. `None` when absent or unparseable.
fn num(call: &Value, key: &str) -> Option<u64> {
    let v = &call["input"][key];
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Read a boolean arg tolerantly — a JSON bool, or a `"true"`/`"1"`/`"yes"`
/// string. Anything else (including absent) is false.
fn flag(call: &Value, key: &str) -> bool {
    let v = &call["input"][key];
    v.as_bool()
        .or_else(|| v.as_str().map(|s| matches!(s.trim(), "true" | "1" | "yes")))
        .unwrap_or(false)
}

fn bounded(bytes: &[u8], call: &Value) -> Result<String, Fail> {
    let total = bytes.len();
    let text = String::from_utf8_lossy(bytes);
    let offset = num(call, "offset").map(|n| n.max(1) as usize);
    let limit = num(call, "limit").map(|n| n as usize);
    if offset.is_some() || limit.is_some() {
        let start = offset.unwrap_or(1) - 1;
        let lines: Vec<&str> = text.lines().collect();
        let end = limit.map_or(lines.len(), |l| (start + l).min(lines.len()));
        if start >= lines.len() {
            return Err(User(format!(
                "offset {} is past the end ({} lines)",
                start + 1,
                lines.len()
            )));
        }
        return Ok(lines[start..end].join("\n"));
    }
    if total > MAX_READ_BYTES {
        let cut = text
            .char_indices()
            .take_while(|(i, _)| *i < MAX_READ_BYTES)
            .count();
        let head: String = text.chars().take(cut).collect();
        return Ok(format!(
            "{head}\n[truncated: first {MAX_READ_BYTES} of {total} bytes — use offset/limit]"
        ));
    }
    Ok(text.into_owned())
}

fn ls(call: &Value, ws: &str) -> Result<String, Fail> {
    let root = opt_hash(call, "root");
    let comps = components_opt(call, "path")?;
    let dir = resolve(root.as_deref(), ws, &comps)?;
    if !dir.is_dir() {
        return Err(User(format!("{} is not a directory", dir.display())));
    }
    let children = entries(path(&dir)).map_err(Fail::from_infra)?;
    let mut lines: Vec<String> = children
        .iter()
        .map(|c| {
            let name = file_name(c);
            if c.is_dir() {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    let total = lines.len();
    if total > MAX_ENTRIES {
        lines.truncate(MAX_ENTRIES);
        lines.push(format!(
            "[truncated: first {MAX_ENTRIES} of {total} entries]"
        ));
    }
    if lines.is_empty() {
        return Ok("(empty directory)".to_string());
    }
    Ok(lines.join("\n"))
}

fn write(call: &Value, ws: &str) -> Result<(String, String), Fail> {
    let comps = components(call, "file-path")?;
    let content = call["input"]["content"]
        .as_str()
        .ok_or_else(|| User("write needs a string `content`".to_string()))?;
    let new_ws = rebuild(ws, &comps, content.as_bytes(), None)?;
    Ok((
        format!("wrote {} ({} bytes)", comps.join("/"), content.len()),
        new_ws,
    ))
}

fn edit(call: &Value, ws: &str) -> Result<(String, String), Fail> {
    let comps = components(call, "file-path")?;
    let old = call["input"]["old-string"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| User("edit needs a non-empty `old-string`".to_string()))?;
    let new = call["input"]["new-string"]
        .as_str()
        .ok_or_else(|| User("edit needs a string `new-string`".to_string()))?;
    let replace_all = flag(call, "replace-all");

    let p = materialize(ws, &comps)?;
    if p.is_dir() {
        return Err(User(format!("{} is a directory", comps.join("/"))));
    }
    let bytes = fs::read(&p).map_err(|e| Infra(format!("reading {}: {e}", p.display())))?;
    let mode = fs::metadata(&p)
        .map(|m| m.permissions().mode())
        .map_err(|e| Infra(format!("stat {}: {e}", p.display())))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        User(format!(
            "{} is not valid UTF-8; edit only text files",
            comps.join("/")
        ))
    })?;

    let count = text.matches(old).count();
    let replaced = match (count, replace_all) {
        (0, _) => {
            return Err(User(
                "old-string not found in the file (it must match exactly, including \
                 whitespace)"
                    .to_string(),
            ))
        }
        (n, false) if n > 1 => {
            return Err(User(format!(
                "old-string appears {n} times; include more surrounding context to make it \
                 unique, or set replace-all"
            )))
        }
        (_, true) => text.replace(old, new),
        (_, false) => text.replacen(old, new, 1),
    };
    let new_ws = rebuild(ws, &comps, replaced.as_bytes(), Some(mode))?;
    let n = if replace_all { count } else { 1 };
    Ok((
        format!(
            "edited {} ({n} replacement{})",
            comps.join("/"),
            if n == 1 { "" } else { "s" }
        ),
        new_ws,
    ))
}

// ---------------------------------------------------------------------------
// Workspace plumbing.
// ---------------------------------------------------------------------------

/// Like [`components`] but for an OPTIONAL path: an absent or blank argument
/// yields the empty path (the root), rather than an error.
fn components_opt(call: &Value, key: &str) -> Result<Vec<String>, Fail> {
    match call["input"][key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => Ok(Vec::new()),
        Some(_) => components(call, key),
    }
}

/// Validate and split a workspace-relative path argument. A leading `/` is
/// tolerated (treated as the workspace root); `..` and the reserved `.caos`
/// are refused.
fn components(call: &Value, key: &str) -> Result<Vec<String>, Fail> {
    let raw = call["input"][key]
        .as_str()
        .ok_or_else(|| User(format!("missing string `{key}`")))?;
    let comps: Vec<String> = raw
        .trim()
        .trim_start_matches('/')
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_string)
        .collect();
    if comps.is_empty() {
        return Err(User(format!("`{key}` names no path: {raw:?}")));
    }
    if comps.iter().any(|c| c == "..") {
        return Err(User("`..` is not allowed in workspace paths".to_string()));
    }
    if comps[0] == STEP_DIR && !(comps.len() == 2 && comps[1] == "conflicts") {
        return Err(User(format!(
            "{STEP_DIR}/ is reserved for the harness; only {STEP_DIR}/conflicts (the merge \
             conflict set) is editable"
        )));
    }
    Ok(comps)
}

/// Walk `comps` down from the workspace root, materializing each level (`caos
/// get` — a no-op when already fetched, hence the ignored result) and
/// returning the leaf path. Missing entries and file-as-directory are user
/// errors.
fn materialize(ws: &str, comps: &[String]) -> Result<PathBuf, Fail> {
    let mut cur = PathBuf::from(ws);
    for (i, comp) in comps.iter().enumerate() {
        let _ = caos(["get", path(&cur)]);
        if !cur.is_dir() {
            return Err(User(format!(
                "{} is a file, not a directory",
                comps[..i].join("/")
            )));
        }
        cur = cur.join(comp);
        if !cur.exists() {
            return Err(User(format!("no such path: {}", comps[..=i].join("/"))));
        }
    }
    let _ = caos(["get", path(&cur)]);
    Ok(cur)
}

/// Rebuild the workspace with `comps` holding `content` (mode `mode`, default
/// 0644): at each level every untouched entry is symlinked (staging resolves
/// links by recorded hash — nothing else materializes) and the target
/// component is descended into or written. Returns the new workspace CAS path.
fn rebuild(ws: &str, comps: &[String], content: &[u8], mode: Option<u32>) -> Result<String, Fail> {
    let dir = scratch(&fresh_name("inline")).map_err(Fail::from_infra)?;
    build_level(Some(Path::new(ws)), &dir, comps, content, mode)?;
    let out = fresh("ws-inline");
    caos(["put", path(&dir), &out]).map_err(Fail::from_infra)?;
    Ok(out)
}

fn build_level(
    src: Option<&Path>,
    dst: &Path,
    comps: &[String],
    content: &[u8],
    mode: Option<u32>,
) -> Result<(), Fail> {
    if let Some(src) = src {
        let _ = caos(["get", path(src)]);
        for child in entries(path(src)).map_err(Fail::from_infra)? {
            if file_name(&child) != comps[0] {
                link(&child, dst.join(file_name(&child))).map_err(Fail::from_infra)?;
            }
        }
    }
    let target = dst.join(&comps[0]);
    if comps.len() == 1 {
        // Overwriting an existing file keeps its mode (the exec bit) unless
        // the caller pinned one (edit does).
        let mode = mode.or_else(|| {
            src.map(|s| s.join(&comps[0])).and_then(|orig| {
                let _ = caos(["get", path(&orig)]);
                fs::metadata(&orig).ok().map(|m| m.permissions().mode())
            })
        });
        fs::write(&target, content)
            .map_err(|e| Infra(format!("writing {}: {e}", target.display())))?;
        if let Some(m) = mode {
            let _ = fs::set_permissions(&target, fs::Permissions::from_mode(m));
        }
        return Ok(());
    }
    fs::create_dir(&target).map_err(|e| Infra(format!("mkdir {}: {e}", target.display())))?;
    let src_sub = match src.map(|s| s.join(&comps[0])) {
        Some(p) if p.is_dir() => Some(p),
        Some(p) if p.exists() => {
            return Err(User(format!("{} is a file, not a directory", comps[0])))
        }
        _ => None,
    };
    build_level(src_sub.as_deref(), &target, &comps[1..], content, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_help_splits_description_and_params() {
        let help = "Print one test's record.\n\
                    A second description line.\n\
                    @param hash The record hash.\n\
                    @param [log] Which inner-stack log.";
        let (doc, args, git) = parse_help("t", help);
        assert_eq!(doc, "Print one test's record. A second description line.");
        assert!(!git);
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name, "hash");
        assert!(args[0].required);
        assert_eq!(args[0].doc, "The record hash.");
        assert_eq!(args[1].name, "log");
        assert!(!args[1].required);
    }

    #[test]
    fn parse_help_git_tag_and_no_params() {
        let (doc, args, git) = parse_help("t", "Just a description.\n@git");
        assert_eq!(doc, "Just a description.");
        assert!(git);
        assert!(args.is_empty());

        // Empty help → empty description (the caller placeholders it) and no args.
        let (doc, args, git) = parse_help("t", "");
        assert!(doc.is_empty());
        assert!(args.is_empty());
        assert!(!git);
    }

    #[test]
    fn parse_help_skips_reserved_and_malformed_params() {
        // `in` is reserved; `Bad` is not a lowercase name → both skipped, the
        // good one survives.
        let (_doc, args, _git) =
            parse_help("t", "d\n@param in nope\n@param Bad nope\n@param ok yes");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].name, "ok");
    }

    #[test]
    fn expr_help_reads_the_here_string_the_value_line_names() {
        let expr = "# a comment\n\
                    HELP=<<END\n\
                    A tool.\n\
                    @param x The x.\n\
                    END\n\
                    curry --base:@=DEEP-DEPS/bash --worker1:@=worker.sh --help=$HELP\n";
        assert_eq!(expr_help(expr).as_deref(), Some("A tool.\n@param x The x."));
    }

    #[test]
    fn expr_help_takes_a_literal_and_reports_none() {
        assert_eq!(
            expr_help("curry --base:@=DEEP-DEPS/bash --help=terse\n").as_deref(),
            Some("terse")
        );
        // A directory whose expression binds no help is not a tool.
        assert_eq!(expr_help("run --base:@=DEEP-DEPS/bash --in:@=.\n"), None);
        // A `$VAR` naming no here-string is not help either.
        assert_eq!(expr_help("curry --base:@=x --help=$NOPE\n"), None);
    }
    #[test]
    fn oid_shape() {
        assert!(valid_oid(&"a".repeat(40))); // sha1
        assert!(valid_oid(&"0".repeat(64))); // sha256
        assert!(valid_oid("0123456789abcdef0123456789abcdef01234567"));
        assert!(!valid_oid("")); // empty
        assert!(!valid_oid(&"a".repeat(39))); // too short
        assert!(!valid_oid(&"a".repeat(41))); // between the two lengths
        assert!(!valid_oid(&"g".repeat(40))); // not hex
        assert!(!valid_oid("src/main.rs")); // a path, not an oid
    }

    #[test]
    fn commit_vs_blob_root() {
        // A commit object resolves to its tree; a lookalike blob does not.
        let tree = "a".repeat(40);
        let commit = format!(
            "tree {tree}\nparent {}\nauthor x <x> 0 +0000\ncommitter x <x> 0 +0000\n\nmsg\n",
            "b".repeat(40)
        );
        assert_eq!(commit_tree_of(commit.as_bytes()), Some(tree));
        // No author line — a blob, even if it opens with "tree ...".
        assert_eq!(
            commit_tree_of(format!("tree {}\nsome file text\n", "c".repeat(40)).as_bytes()),
            None
        );
        assert_eq!(commit_tree_of(b"just a normal file\n"), None);
    }

    #[test]
    fn read_and_ls_are_inline_and_reserved() {
        // Routed in-process (no sub-run) and shadow-proof against a tree tool.
        for t in ["read", "ls"] {
            assert!(is_inline(t));
            assert!(RESERVED_TOOLS.contains(&t));
            assert!(declarations().iter().any(|d| d["name"] == t));
        }
        // read-oid is gone — folded into `read` via `root`.
        assert!(!is_inline("read-oid"));
        assert!(!declarations().iter().any(|d| d["name"] == "read-oid"));
        // read's file-path is no longer required (a blob `root` reads with none).
        let read = declarations()
            .into_iter()
            .find(|d| d["name"] == "read")
            .unwrap();
        assert!(read["input_schema"].get("required").is_none());
    }

    #[test]
    fn arg_lines_parse() {
        let required = parse_arg("hash The record hash.").unwrap();
        assert_eq!(required.name, "hash");
        assert_eq!(required.doc, "The record hash.");
        assert!(required.required);

        let optional = parse_arg("[log] Which log.").unwrap();
        assert_eq!(optional.name, "log");
        assert!(!optional.required);

        // A name with no description is still a usable parameter.
        assert_eq!(parse_arg("bare").unwrap().name, "bare");

        // Rejected: an arg the interpreter already binds (curry errors on a
        // rebind), and anything that isn't a plain lower-case flag name.
        assert!(parse_arg("in The workspace.").is_none());
        assert!(parse_arg("worker1 The script.").is_none());
        assert!(parse_arg("Hash The record hash.").is_none());
        assert!(parse_arg("--hash The record hash.").is_none());
        assert!(parse_arg("").is_none());
    }

    #[test]
    fn declaration_matches_the_declared_args() {
        let tool = TreeTool {
            name: "test-result".to_string(),
            doc: "Print a record.".to_string(),
            args: vec![
                TreeArg {
                    name: "hash".to_string(),
                    doc: "The record hash.".to_string(),
                    required: true,
                },
                TreeArg {
                    name: "log".to_string(),
                    doc: "Which log.".to_string(),
                    required: false,
                },
            ],
            git: false,
        };
        let d = tree_tool_declaration(&tool);
        assert_eq!(d["input_schema"]["properties"]["hash"]["type"], "string");
        assert_eq!(
            d["input_schema"]["properties"]["log"]["description"],
            "Which log."
        );
        // Only the required ones are listed, so an optional arg can be omitted.
        assert_eq!(d["input_schema"]["required"], json!(["hash"]));

        // No @param tags: an empty properties object and NO `required` key —
        // an empty required array is not valid JSON Schema for the API.
        let bare = TreeTool {
            name: "build".to_string(),
            doc: "Build.".to_string(),
            args: Vec::new(),
            git: false,
        };
        let d = tree_tool_declaration(&bare);
        assert_eq!(
            d["input_schema"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn bad_calls_become_is_error_results() {
        let tool = TreeTool {
            name: "echo-arg".to_string(),
            doc: String::new(),
            args: vec![
                TreeArg {
                    name: "word".to_string(),
                    doc: String::new(),
                    required: true,
                },
                TreeArg {
                    name: "suffix".to_string(),
                    doc: String::new(),
                    required: false,
                },
            ],
            git: false,
        };
        let call = |input: Value| json!({"id": "toolu_01", "name": "echo-arg", "input": input});

        let bound = tree_tool_args(&call(json!({"word": "banana"})), &tool).unwrap();
        assert_eq!(bound, vec![("word".to_string(), "banana".to_string())]);

        // Scalars are stringified, since every arg reaches the script as a blob.
        let bound = tree_tool_args(&call(json!({"word": 7, "suffix": true})), &tool).unwrap();
        assert_eq!(
            bound,
            vec![
                ("word".to_string(), "7".to_string()),
                ("suffix".to_string(), "true".to_string())
            ]
        );

        for bad in [
            json!({}),                             // required arg missing
            json!({"word": "x", "colour": "red"}), // undeclared arg
            json!({"word": ["banana"]}),           // non-scalar value
        ] {
            let block = tree_tool_args(&call(bad), &tool).unwrap_err();
            assert_eq!(block["is_error"], true);
            assert_eq!(block["tool_use_id"], "toolu_01");
        }
    }
}
