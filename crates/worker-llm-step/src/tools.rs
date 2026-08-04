//! Inline file tools — `read`, `ls`, `write`, `edit` — executed in-process by
//! the step worker (design/agent-harness.md, "Tool classes"): hash-level
//! workspace operations that need no sub-run, no container, no dispatch.
//! Reads materialize only the path they touch; writes rebuild the tree by
//! symlinking every untouched entry and `caos put`ting the result (staging
//! resolves links by recorded hash — the same surgery `mint_step` does for
//! `.caos`), so the never-materialize rule holds throughout.
//!
//! A failed call — missing file, non-unique `old_string`, a file where a
//! directory was expected — is an `is_error` tool_result the model reacts to,
//! never a worker error. Parameter shapes mirror Claude Code's file tools
//! (`file_path`, `content`, `old_string`/`new_string`/`replace_all`), which
//! models know well.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{json, Value};
use worker_common::{caos, entries, file_name, link, path, scratch};

/// The reserved workspace entry (step transcripts); refused in tool paths.
const STEP_DIR: &str = ".caos";

/// Reads larger than this are truncated (with a note) unless `offset`/`limit`
/// narrow them; `ls` listings cap at [`MAX_ENTRIES`] the same way.
const MAX_READ_BYTES: usize = 100_000;
const MAX_ENTRIES: usize = 1_000;

/// True if `name` is one of the inline tools this module executes.
pub fn is_inline(name: &str) -> bool {
    matches!(name, "read" | "ls" | "write" | "edit" | "read-oid")
}

/// The inline tools' registry entries, alongside `bash`'s.
pub fn declarations() -> Vec<Value> {
    let path_desc = "Workspace-relative path (the workspace root is the repo root).";
    vec![
        json!({
            "name": "read",
            "description": "Read a file from the workspace. Returns the raw content. Prefer \
        this over `cat` via bash — it is immediate and needs no `paths` declaration. Large files \
        are truncated; use `offset`/`limit` (line-based) to page.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": path_desc},
                    "offset": {"type": "integer", "description": "1-based first line to return."},
                    "limit": {"type": "integer", "description": "Number of lines to return."}
                },
                "required": ["file_path"]
            }
        }),
        json!({
            "name": "ls",
            "description": "List a workspace directory: one entry per line, directories with a \
        trailing `/`. Prefer this over `ls` via bash.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative directory; omit for the workspace root."}
                }
            }
        }),
        json!({
            "name": "write",
            "description": "Write a file into the workspace (creating parent directories, \
        overwriting an existing file). Prefer this over heredocs/redirection via bash.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": path_desc},
                    "content": {"type": "string", "description": "The full new file content."}
                },
                "required": ["file_path", "content"]
            }
        }),
        json!({
            "name": "edit",
            "description": "Replace text in a workspace file. `old_string` must match the file \
        content exactly and (unless `replace_all`) appear exactly once — include surrounding \
        context to disambiguate. Prefer this over sed via bash.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": path_desc},
                    "old_string": {"type": "string", "description": "Exact text to replace."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
                },
                "required": ["file_path", "old_string", "new_string"]
            }
        }),
        json!({
            "name": "read-oid",
            "description": "Read a blob by its git object hash. Bounded exactly like `read` \
        (large blobs truncate; use `offset`/`limit`). Use this to inspect content named by an \
        oid that no workspace path holds — in particular a stage oid from `.caos/conflicts` \
        (the merge base, or either side of a modify/delete, binary, or type conflict), so you \
        can see what you are choosing between.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "oid": {"type": "string", "description": "A git object hash (a blob)."},
                    "offset": {"type": "integer", "description": "1-based first line to return."},
                    "limit": {"type": "integer", "description": "Number of lines to return."}
                },
                "required": ["oid"]
            }
        }),
    ]
}

/// The grep tool's registry entry (present only when a `grep-image` is
/// curried — see `Config`). It runs as the rgrep fold sub-run; this module
/// contributes the declaration, the pre-launch validation, and the
/// transcript-boundary rendering of its sparse result tree.
pub fn grep_declaration() -> Value {
    json!({
        "name": "grep",
        "description": "Search the workspace with a regular expression (Rust regex syntax, \
    line-based). Returns matches as `path:linenum:line`. Scope with `path` (a directory or \
    file) to narrow the search; results are cached per unchanged subtree, so repeated and \
    scoped greps are cheap. Prefer this over grep/find via bash.",
        "input_schema": {
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "The regular expression to search for."},
                "path": {"type": "string", "description": "Workspace-relative directory or file to search; omit for the whole workspace."}
            },
            "required": ["pattern"]
        }
    })
}

// ---- Tree tools (caos-tools/*.sh — design/cargo-workers.md) -----------------
/// Reserved built-in tool names a tree tool may not shadow: the model's
/// primitives (including the repair path for a broken tool edit — bash and
/// the file tools) must stay stable whatever the tree carries.
const RESERVED_TOOLS: &[&str] = &["bash", "grep", "read", "ls", "write", "edit", "read-oid"];

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
const RESERVED_ARGS: &[&str] = &["in", "worker1", "image", "std", "salt"];

/// One tree tool as the registry sees it: its name, its `#@doc` description,
/// and the `#@arg` parameters it accepts.
pub struct TreeTool {
    pub name: String,
    pub doc: String,
    pub args: Vec<TreeArg>,
}

/// One `#@arg` line: `#@arg <name> <description>` is required, `#@arg
/// [<name>] <description>` optional. The name becomes the script's `--<name>`
/// arg, readable at `/cas/args/<name>`.
pub struct TreeArg {
    pub name: String,
    pub doc: String,
    pub required: bool,
}

/// Parse one `#@arg` line's payload (everything after the marker) into a
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

/// Read one tool script into its registry shape: the `#@doc` lines joined as
/// the description, the `#@arg` lines as parameters.
fn read_tool(name: &str, path: &str) -> Result<TreeTool, String> {
    caos(["get", path])?;
    let text = fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let mut doc: Vec<&str> = Vec::new();
    let mut args = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("#@doc") {
            doc.push(rest.trim());
        } else if let Some(rest) = line.strip_prefix("#@arg") {
            match parse_arg(rest.trim()) {
                Some(a) => args.push(a),
                None => eprintln!("caos-tools/{name}.sh: unusable #@arg line: {line}"),
            }
        }
    }
    let doc = if doc.is_empty() {
        format!("Project tool caos-tools/{name}.sh (no #@doc description).")
    } else {
        doc.join(" ")
    };
    Ok(TreeTool {
        name: name.to_string(),
        doc,
        args,
    })
}

/// Discover the tree-defined tools: `caos-tools/*.sh`, each with the
/// description from its `#@doc ` lines and the parameters from its `#@arg`
/// lines. Resolved fresh from the CURRENT workspace every round, so an agent
/// that adds, edits, or removes a tool sees the change on its next request.
/// Reserved names are skipped loudly; subdirectories are helpers, not tools.
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
    for fname in names {
        let Some(name) = fname.strip_suffix(".sh") else {
            continue;
        };
        let p = format!("{dir}/{fname}");
        if !Path::new(&p).is_file() {
            continue;
        }
        if RESERVED_TOOLS.contains(&name) {
            eprintln!("caos-tools/{fname} shadows the built-in {name:?} tool — ignored");
            continue;
        }
        out.push(read_tool(name, &p)?);
    }
    Ok(out)
}

/// One discovered tool's registry entry. A tool with no `#@arg` lines takes
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

/// Resolve tool `name` in the CURRENT workspace — invocation-time lookup, so
/// a call made right after an edit runs the edited script. `None` when the
/// tree doesn't define it (or the name is reserved / not a clean filename).
pub fn tree_tool_script(ws: &str, name: &str) -> Result<Option<(String, TreeTool)>, String> {
    if RESERVED_TOOLS.contains(&name) || name.contains('/') || name.contains("..") {
        return Ok(None);
    }
    let Some(dir) = tree_tools_dir(ws)? else {
        return Ok(None);
    };
    let p = format!("{dir}/{name}.sh");
    if !Path::new(&p).is_file() {
        return Ok(None);
    }
    let tool = read_tool(name, &p)?;
    Ok(Some((p, tool)))
}

/// Bind a tree-tool call's inputs to the parameters the script declared,
/// returning the `--<name>=<value>` pairs for the curry. A missing required
/// arg, an undeclared one, or a non-scalar value is the model's mistake, so it
/// comes back as a ready-made `is_error` tool_result rather than a worker
/// error — the same contract `grep_precheck` uses.
pub fn tree_tool_args(call: &Value, tool: &TreeTool) -> Result<Vec<(String, String)>, Value> {
    let id = call["id"].as_str().unwrap_or("");
    let fail = |msg: String| Err(block(id, &msg, true));
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
    Ok(block(id, text.trim_end(), is_err))
}

/// Validate a grep call before its sub-run launches: the pattern must compile
/// and the scope must exist. Returns the scope's CAS path and its
/// workspace-relative prefix (`""` for the root) — or, on a user mistake, the
/// ready-made `is_error` tool_result.
pub fn grep_precheck(call: &Value, ws: &str) -> Result<(String, String), Value> {
    let id = call["id"].as_str().unwrap_or("");
    let fail = |msg: String| Err(block(id, &msg, true));
    let Some(pattern) = call["input"]["pattern"].as_str() else {
        return fail("grep needs a string `pattern`".to_string());
    };
    if let Err(e) = regex::Regex::new(pattern) {
        return fail(format!("invalid pattern: {e}"));
    }
    match call["input"]["path"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
    {
        None => Ok((ws.to_string(), String::new())),
        Some(_) => {
            let comps = match components(call, "path") {
                Ok(c) => c,
                Err(User(msg)) => return fail(msg),
                Err(Infra(e)) => return fail(e),
            };
            match materialize(ws, &comps) {
                Ok(p) => Ok((p.to_string_lossy().into_owned(), comps.join("/"))),
                Err(User(msg)) => fail(msg),
                Err(Infra(e)) => fail(e),
            }
        }
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
            return Ok(block(id, "no matches", false));
        }
        let rendered: String = text.lines().map(|l| format!("{scope}:{l}\n")).collect();
        return Ok(block(id, rendered.trim_end(), false));
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
        return Ok(block(id, "no matches", false));
    }
    let mut text = render.out;
    if render.overflow_files > 0 {
        text += &format!(
            "\n[truncated — {} more matching file(s); narrow the pattern or grep a \
             subdirectory]",
            render.overflow_files
        );
    }
    Ok(block(id, text.trim_end(), false))
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
        "read-oid" => read_oid(call).map(|text| (text, None)),
        "ls" => ls(call, ws).map(|text| (text, None)),
        "write" => write(call, ws).map(|(text, new_ws)| (text, Some(new_ws))),
        "edit" => edit(call, ws).map(|(text, new_ws)| (text, Some(new_ws))),
        other => Err(User(format!("unknown inline tool {other:?}"))),
    };
    match outcome {
        Ok((text, new_ws)) => Ok((block(id, &text, false), new_ws)),
        Err(User(msg)) => Ok((block(id, &msg, true), None)),
        Err(Infra(e)) => Err(e),
    }
}

fn block(id: &str, text: &str, is_error: bool) -> Value {
    let mut b = json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": [{"type": "text", "text": text}],
    });
    if is_error {
        b["is_error"] = Value::Bool(true);
    }
    b
}

// ---------------------------------------------------------------------------
// The four tools.
// ---------------------------------------------------------------------------

fn read(call: &Value, ws: &str) -> Result<String, Fail> {
    let comps = components(call, "file_path")?;
    let p = materialize(ws, &comps)?;
    if p.is_dir() {
        return Err(User(format!("{} is a directory; use ls", comps.join("/"))));
    }
    let bytes = fs::read(&p).map_err(|e| Infra(format!("reading {}: {e}", p.display())))?;
    bounded(&bytes, call)
}

/// A git object hash: hex, 40 (sha1) or 64 (sha256) chars.
fn valid_oid(oid: &str) -> bool {
    (oid.len() == 40 || oid.len() == 64) && oid.chars().all(|c| c.is_ascii_hexdigit())
}

/// Read a blob by hash. The load-bearing companion to `.caos/conflicts`: a
/// stage oid there (the merge base, or a side of a modify/delete, binary, or
/// type conflict) names content reachable through no workspace path, so the
/// agent has no other way to see what it is choosing between. Bounded exactly
/// like `read`. An unknown/malformed oid is the model's mistake — an is_error
/// result to react to, not a worker failure.
fn read_oid(call: &Value) -> Result<String, Fail> {
    let oid = call["input"]["oid"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| User("read-oid needs a string `oid`".to_string()))?;
    if !valid_oid(oid) {
        return Err(User(format!("{oid:?} is not a git object hash")));
    }
    let dst = fresh("oid");
    caos(["get-hash", oid, &dst]).map_err(|e| User(format!("cannot read {oid}: {e}")))?;
    let p = Path::new(&dst);
    if p.is_dir() {
        return Err(User(format!(
            "{oid} names a tree, not a blob; read-oid reads blobs"
        )));
    }
    let _ = caos(["get", &dst]);
    let bytes = fs::read(p).map_err(|e| Infra(format!("reading {}: {e}", p.display())))?;
    bounded(&bytes, call)
}

/// Apply `read`'s bounds to raw bytes: a line window when `offset`/`limit` is
/// set, else a head-truncation at [`MAX_READ_BYTES`]. Shared by `read` and
/// `read-oid` so the two present content identically.
fn bounded(bytes: &[u8], call: &Value) -> Result<String, Fail> {
    let total = bytes.len();
    let text = String::from_utf8_lossy(bytes);
    let offset = call["input"]["offset"].as_u64().map(|n| n.max(1) as usize);
    let limit = call["input"]["limit"].as_u64().map(|n| n as usize);
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
    let dir = match call["input"]["path"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
    {
        None => PathBuf::from(ws),
        Some(_) => materialize(ws, &components(call, "path")?)?,
    };
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
    let comps = components(call, "file_path")?;
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
    let comps = components(call, "file_path")?;
    let old = call["input"]["old_string"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| User("edit needs a non-empty `old_string`".to_string()))?;
    let new = call["input"]["new_string"]
        .as_str()
        .ok_or_else(|| User("edit needs a string `new_string`".to_string()))?;
    let replace_all = call["input"]["replace_all"].as_bool().unwrap_or(false);

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
                "old_string not found in the file (it must match exactly, including \
                 whitespace)"
                    .to_string(),
            ))
        }
        (n, false) if n > 1 => {
            return Err(User(format!(
                "old_string appears {n} times; include more surrounding context to make it \
                 unique, or set replace_all"
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
    let dir = scratch(&format!("inline-{}", counter())).map_err(Fail::from_infra)?;
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

/// Fresh single-assignment CAS paths, distinct from `main.rs`'s prefixes.
fn fresh(prefix: &str) -> String {
    format!("/cas/{prefix}-{}", counter())
}

fn counter() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn read_oid_is_inline_and_reserved() {
        // Routed in-process (no sub-run) and shadow-proof against a tree tool.
        assert!(is_inline("read-oid"));
        assert!(RESERVED_TOOLS.contains(&"read-oid"));
        // It is offered to the model in every turn (no image gate).
        assert!(declarations().iter().any(|d| d["name"] == "read-oid"));
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
        };
        let d = tree_tool_declaration(&tool);
        assert_eq!(d["input_schema"]["properties"]["hash"]["type"], "string");
        assert_eq!(
            d["input_schema"]["properties"]["log"]["description"],
            "Which log."
        );
        // Only the required ones are listed, so an optional arg can be omitted.
        assert_eq!(d["input_schema"]["required"], json!(["hash"]));

        // No #@arg lines: an empty properties object and NO `required` key —
        // an empty required array is not valid JSON Schema for the API.
        let bare = TreeTool {
            name: "build".to_string(),
            doc: "Build.".to_string(),
            args: Vec::new(),
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
