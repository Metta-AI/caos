//! Inline file tools — `read`, `ls`, `write`, `edit` — executed in-process by
//! the step worker (design/agent-harness.md, "Tool classes"): hash-level
//! source tree operations that need no sub-run, no container, no dispatch.
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
use worker_common::{caos, entries, file_name, path, scratch};

use crate::{fresh, fresh_name, result_block};

/// Reads larger than this are truncated (with a note) unless `offset`/`limit`
/// narrow them; `ls` listings cap at [`MAX_ENTRIES`] the same way.
const MAX_READ_BYTES: usize = 100_000;
const MAX_ENTRIES: usize = 1_000;

/// True if `name` is one of the inline tools this module executes.
pub fn is_inline(name: &str) -> bool {
    matches!(
        name,
        "read" | "ls" | "write" | "edit" | "import_source" | "publish_source" | "tool_help"
    )
}

/// Help text for the built-in tools, authored exactly like a caos-tools
/// `.caos-expr`'s `HELP` here-string (SPEC, "Tools"): free description text,
/// then dashed `@param` tags. They are parsed by the same `parse_help` the tree
/// tools use, so a built-in and a project tool are described one way — the docs
/// live with the tool, not inside a hand-written JSON schema.
const READ_HELP: &str = "Read a file's contents. Paths start in the conversation tree and traverse code references, for example feature/dirty/README.md. With an explicit source tree they are relative to its code tree; pass `root` — a commit, tree, or blob hash (one printed by `log`/`show`/`diff`, or a stage oid from `.caos/conflicts`) — to read as of another revision. With a commit or tree `root`, `file-path` names the file within it; with a blob `root`, omit `file-path` to read the blob directly. Prefer this over `cat` via bash — it is immediate and needs no `paths` declaration. Large files are truncated; use `offset`/`limit` (line-based) to page.
@param [file-path] Conversation path, such as feature/dirty/README.md; code-relative when source tree is explicit.
@param [root] Optional commit/tree/blob hash to read from — an older revision, or a bare blob (e.g. a `.caos/conflicts` stage oid). Omit for the current conversation tree.
@param [offset] 1-based first line to return.
@param [limit] Number of lines to return.";

const LS_HELP: &str = "List a directory: one entry per line, directories with a trailing `/`. Paths start in the conversation tree and traverse code references, for example feature/dirty/README.md. With an explicit source tree they are relative to its code tree; pass `root` (a commit or tree hash) to list it as of another revision, and `path` to descend within that root. Prefer this over `ls` via bash.
@param [path] Directory to list (relative to `root`, or to the conversation root unless source tree is explicit); omit for the root itself.
@param [root] Optional commit or tree hash to list as of another revision. Omit for the current conversation tree.";

const WRITE_HELP: &str = "Write a file at a conversation path or beneath a code reference (creating parent directories, overwriting an existing file). Prefer this over heredocs/redirection via bash.
@param file-path Conversation path, such as feature/dirty/README.md; code-relative when source tree is explicit.
@param content The full new file content.";

const EDIT_HELP: &str = "Replace text in a conversation file or beneath a code reference. `old-string` must match the file content exactly and (unless `replace-all`) appear exactly once — include surrounding context to disambiguate. Prefer this over sed via bash.
@param file-path Conversation path, such as feature/dirty/README.md; code-relative when source tree is explicit.
@param old-string Exact text to replace.
@param new-string Replacement text.
@param [replace-all] Replace every occurrence (default false).";

const TOOL_HELP_HELP: &str = "Describe the repository tool at a conversation path: what it does and which parameters `run_tool` accepts for it. A tool is a directory carrying a `.caos-expr` that binds a `help`, such as feature/01-change/caos-tools/test. Tools are NOT listed for you; they are documented in each repository's own docs (AGENTS.md, README, and so on), and this tool is the authoritative description of what one takes. Call it before `run_tool` whenever you have not been told a tool's parameters, or the docs might be stale. It reads the tree only -- it never builds or runs the tool.
@param path Conversation-relative directory of the tool, such as feature/01-change/caos-tools/test.";

const GREP_HELP: &str = "Search the conversation tree, including code references, with a regular expression (Rust regex syntax, line-based). Returns matches as `path:linenum:line`. Scope with `path` (a directory or file) to narrow the search; results are cached per unchanged subtree, so repeated and scoped greps are cheap. Pass `root` (a commit or tree hash) to search as of another revision. Prefer this over grep/find via bash.
@param pattern The regular expression to search for.
@param [path] Directory or file to search (relative to `root`, or to the conversation root); omit for everything.
@param [root] Optional commit or tree hash to search as of another revision. Omit for the current conversation tree.";

/// Build a built-in tool's registry entry from its help text, through the very
/// same `parse_help` → `tree_tool_declaration` path a discovered caos-tools
/// tool takes. A std entry that declares `@git` gets the same builder.
pub(crate) fn builtin_tool(name: &str, help: &str) -> TreeTool {
    TreeTool::new(name, parse_help(&format!("built-in {name}"), help))
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
        ("import_source", crate::import_source::HELP),
        ("tool_help", TOOL_HELP_HELP),
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

// ---- Repository tools (SPEC "CaosTools") -----------------------------------
//
// A repository tool is ANY directory in the conversation whose `.caos-expr`
// binds a `help` -- there is no registry and no reserved directory name. The
// model names its path: `tool_help` describes it, `run_tool` invokes it.
//
// Nothing enumerates them, and that is the design. Enumeration meant injecting
// every source tree's tools into the system prompt, so a conversation that
// gains a tree mid-run re-keyed the prompt and grew it per tree. Naming a path
// instead makes the tool list tree-INDEPENDENT (see `mcp::declarations`), at
// the price of discovery being documentation: an undocumented tool is
// invisible, which is the trade.

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
    /// The tool declared `@git`: bind the source tree commit (`wc`) and the
    /// turn's ref snapshot (`refs`) so it can walk history. Off by default —
    /// `wc` changes every step, so binding it into a tool that doesn't need it
    /// (build/test) would turn every cache hit into a miss.
    pub git: bool,
    /// The tool declared `@writer`: its result is a PROPOSED CHANGE to the tree
    /// it was run on, not a value. Nothing reads this yet — the harness still
    /// decides writer-ness by tool name (`callback_result`); this carries the
    /// declaration so that match can be replaced by it.
    pub writer: bool,
    /// The tool declared `@in`: bind the tree it was run on. See
    /// [`Help::wants_in`] for why absence is the default.
    pub wants_in: bool,
}

impl TreeTool {
    /// A named tool from its parsed help.
    fn new(name: &str, help: Help) -> TreeTool {
        TreeTool {
            name: name.to_string(),
            doc: help.doc,
            args: help.args,
            git: help.git,
            writer: help.writer,
            wants_in: help.wants_in,
        }
    }

    /// The same, with a placeholder description when the help gave none — a
    /// tool with no prose still has to be describable.
    fn named(name: &str, mut help: Help, placeholder: &str) -> TreeTool {
        if help.doc.is_empty() {
            help.doc = placeholder.to_string();
        }
        TreeTool::new(name, help)
    }
}

/// One `@param` tag: `@param <name> <description>` is required, `@param
/// [<name>] <description>` optional. The name becomes the script's `--<name>`
/// arg, readable at `/cas/args/<name>`.
pub struct TreeArg {
    pub name: String,
    pub doc: String,
    pub required: bool,
    pub ty: ArgType,
}

/// The JSON type a parameter accepts, declared as `@param {<type>} <name>`.
///
/// A bare `@param <name>` is a STRING, which is what nearly everything wants:
/// a curried arg reaches the script as a blob whatever it left the model as.
/// The type exists for the cases where narrowing the model to a string would
/// narrow the TOOL — `bash`'s `paths` is a list, and saying "string" would
/// have quietly taken that away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgType {
    Text,
    /// An array of strings. Curried as a blob of NEWLINE-SEPARATED elements,
    /// which is the encoding `bash` already gave its `paths` by hand.
    Lines,
}

impl ArgType {
    fn parse(name: &str) -> Option<ArgType> {
        match name {
            "string" => Some(ArgType::Text),
            "array" => Some(ArgType::Lines),
            _ => None,
        }
    }

    /// This type's JSON Schema fragment, with `doc` as the description.
    fn schema(self, doc: &str) -> Value {
        match self {
            ArgType::Text => json!({"type": "string", "description": doc}),
            ArgType::Lines => {
                json!({"type": "array", "items": {"type": "string"}, "description": doc})
            }
        }
    }
}

/// Parse one `@param` tag's payload (everything after the tag) into a
/// parameter. `None` — reported by the caller — for a malformed name, so a
/// typo costs a visible skip rather than an arg the model can't use.
fn parse_arg(payload: &str) -> Option<TreeArg> {
    // An optional leading `{type}`. An UNKNOWN type fails the whole tag rather
    // than falling back to a string: a typo that silently narrowed an array
    // parameter would take a capability away with no error anywhere.
    let (ty, payload) = match payload.strip_prefix('{') {
        Some(rest) => {
            let (name, rest) = rest.split_once('}')?;
            (ArgType::parse(name.trim())?, rest.trim_start())
        }
        None => (ArgType::Text, payload),
    };
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
        ty,
    })
}

/// One tool's parsed `help`: what it does, what it takes, and the flag tags.
pub struct Help {
    /// Free text before the first block tag; empty for the caller to
    /// placeholder.
    pub doc: String,
    pub args: Vec<TreeArg>,
    /// `@git`: bind the source tree commit and the turn's ref snapshot.
    pub git: bool,
    /// `@writer`: this tool PROPOSES A CHANGE to the tree it is run on, rather
    /// than returning a value. Absent means read-only, which is the default
    /// precisely because it is the safe answer for a tool that forgot to say.
    pub writer: bool,
    /// `@in`: bind the tree this tool was run on as `in`.
    ///
    /// Absent means DO NOT BIND IT, and that is the point: the tree is the
    /// biggest thing that can land in an ArgTree, and the ArgTree is the cache
    /// key. `caos-test-result`, whose entire input is a hash, carried the whole
    /// source tree and so re-keyed on every edit to it — asking twice for the
    /// same test record could not hit. `grep` has always done this right by
    /// binding only the scope it searches, which is why a scoped grep is cheap.
    pub wants_in: bool,
}

/// If `line` opens a javadoc BLOCK TAG, its name without the `@`.
///
/// Tag-shaped means the first token is `@` followed only by lowercase letters
/// and hyphens, which is what keeps an `@` inside prose from reading as a tag:
/// `caos@caos` and `github.com@evil.example` both fail it, and both appear in
/// help text today.
fn block_tag(line: &str) -> Option<&str> {
    let tag = line.split_whitespace().next()?.strip_prefix('@')?;
    let shaped = !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '-');
    shaped.then_some(tag)
}

/// Parse a tool's `help` string as a JAVADOC comment (SPEC, "CaosTools"): the
/// free text before the first block tag is the description, `@param <name>` /
/// `@param [<name>]` tags declare the parameters, and the bare tags `@git` and
/// `@writer` set flags.
///
/// **An unrecognized tag is reported, never absorbed.** A misspelled `@writer`
/// would otherwise leave a writer silently read-only — and then its proposal is
/// discarded as a value, so the edit simply does not happen, with no error
/// anywhere. Same reasoning as the `@param` message below: a tag this parser
/// cannot act on is a defect in the tool, and the only place it can be noticed
/// is here.
fn parse_help(ctx: &str, text: &str) -> Help {
    let mut doc: Vec<&str> = Vec::new();
    let mut args = Vec::new();
    let mut git = false;
    let mut writer = false;
    let mut wants_in = false;
    let mut in_tags = false;
    for line in text.lines() {
        let trimmed = line.trim();
        // `@param` must END there: `strip_prefix` alone would read `@params x`
        // as `@param` with `s x` after it and mint an arg named `s`, which is
        // exactly the silent mis-parse the tags below are guarded against.
        let param = trimmed
            .strip_prefix("@param")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
        if let Some(rest) = param {
            in_tags = true;
            match parse_arg(rest.trim()) {
                Some(a) => args.push(a),
                None => eprintln!("{ctx}: unusable @param tag: {line}"),
            }
        } else if trimmed == "@git" {
            in_tags = true;
            git = true;
        } else if trimmed == "@writer" {
            in_tags = true;
            writer = true;
        } else if trimmed == "@in" {
            in_tags = true;
            wants_in = true;
        } else if let Some(tag) = block_tag(trimmed) {
            in_tags = true;
            eprintln!(
                "{ctx}: unknown block tag @{tag} — ignored \
                 (known: @param, @git, @writer, @in)"
            );
        } else if !in_tags {
            // Description text — everything before the first block tag.
            doc.push(trimmed);
        }
    }
    Help {
        doc: doc.join(" ").trim().to_string(),
        args,
        git,
        writer,
        wants_in,
    }
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

/// Why a path is not a tool — distinct from "no such path", which `caos
/// resolve` reports, and from a CAS failure, which is the worker's problem.
/// The model gets told which, because the three have different fixes.
pub enum NotATool {
    /// Not a directory, or a directory carrying no `.caos-expr`: either way,
    /// ordinary content.
    NoExpression,
    /// It has an expression, but the expression binds no `--help`. An
    /// evaluable entry, not a tool.
    NoHelp,
}

/// Read one tool directory into its described shape: the `help` its
/// `.caos-expr` binds, parsed into a description and parameters. `name` is the
/// path the caller used, for messages only.
fn read_tool(name: &str, dir: &str) -> Result<Result<TreeTool, NotATool>, String> {
    // Materialize the directory's own entries first. A `caos get` is SHALLOW:
    // the parent fetch made this directory exist, but nothing inside it —
    // without this, every tool looks like a directory with no `.caos-expr`
    // (every one did, and the whole registry came back empty).
    caos(["get", dir])?;
    let path = format!("{dir}/.caos-expr");
    if !Path::new(&path).is_file() {
        return Ok(Err(NotATool::NoExpression));
    }
    caos(["get", &path])?;
    let text = fs::read_to_string(&path).map_err(|e| format!("reading {path}: {e}"))?;
    let Some(help) = expr_help(&text) else {
        return Ok(Err(NotATool::NoHelp));
    };
    Ok(Ok(TreeTool::named(
        name,
        parse_help(&format!("{name}/.caos-expr"), &help),
        &format!("Repository tool {name} (no description)."),
    )))
}

/// A harness-provided std tool (std/caos-build, std/caos-test), described by the
/// `help` its curried IMAGE carries — read the same way a tree tool's help is,
/// so a built-in std tool and a project caos-tools tool are one mechanism, just
/// sourced differently. `dir` is the materialized arg-tree path
/// (`/cas/args/<name>-image`).
///
/// THE HELP IS AT `args/help`, not `help`. That path is the curry node's own
/// layout — `{base, args/<name>…, .caos-curry}` (`caos::caos_curry`) — and the
/// tool's `.caos-expr` binds `--help=` like any other argument. Looking at the
/// top level finds nothing, which is exactly what happened: `registry` skipped
/// a tool it could not describe, so every std tool silently vanished from the
/// registry and the harness offered six tools where it meant to offer thirteen.
///
/// `None` when the image carries no `help` — a configuration error, since the
/// harness itself curried the image, and its callers now say so.
pub fn std_tool(name: &str, dir: &str) -> Result<Option<TreeTool>, String> {
    caos(["get", dir])?;
    let args = format!("{dir}/args");
    if !Path::new(&args).exists() {
        return Ok(None);
    }
    caos(["get", &args])?;
    let help_path = format!("{args}/help");
    if !Path::new(&help_path).exists() {
        return Ok(None);
    }
    caos(["get", &help_path])?;
    let help = fs::read_to_string(&help_path).map_err(|e| format!("reading {help_path}: {e}"))?;
    Ok(Some(TreeTool::named(
        name,
        parse_help(&format!("std/{name}"), &help),
        &format!("Built-in tool {name} (no description)."),
    )))
}

/// One discovered tool's registry entry. A tool with no `@param` tags takes
/// no parameters — the source tree IS its input — and one with them takes
/// them as strings, since every arg reaches the script as a `/cas/args/<name>`
/// blob whatever JSON type it left the model as.
pub fn tree_tool_declaration(tool: &TreeTool) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for a in &tool.args {
        props.insert(a.name.clone(), a.ty.schema(&a.doc));
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

/// Resolve a tool by path in its captured input snapshot and read its schema.
///
/// `relative` is resolved with `caos resolve`, which TRAVERSES COMMIT ENTRIES
/// (`caos::gitlinks::resolve`) — so a conversation-relative path reaches into a
/// source tree without the caller unwrapping the gitlink. `Err` carries the
/// resolve failure (`no such path: …`) as well as CAS trouble; the inner `Err`
/// says the path exists but is not a tool.
pub fn tool_at(ws: &str, relative: &str) -> Result<Result<TreeTool, NotATool>, String> {
    let resolved = fresh("tool-path");
    caos([
        "resolve",
        &worker_common::cas_hash(ws)?,
        relative,
        &resolved,
    ])?;
    if !Path::new(&resolved).is_dir() {
        return Ok(Err(NotATool::NoExpression));
    }
    read_tool(relative, &resolved)
}

/// `tool_help`: describe the tool at a conversation-relative path.
///
/// READS THE TREE ONLY — it resolves the path, reads the `.caos-expr` and parses
/// the `help` here-string it binds. It never evaluates, so describing a compiled
/// tool does not build it, and the answer is immediate (this is why `tool_help`
/// is inline and `run_tool` is not).
fn tool_help(call: &Value, ws: &str) -> Result<String, Fail> {
    let raw = call["input"]["path"].as_str().unwrap_or("").trim();
    let path = raw.trim_matches('/');
    if path.is_empty() {
        return Err(User(
            "tool_help needs a `path`: the conversation-relative directory of a tool, \
             such as feature/01-change/caos-tools/test"
                .to_string(),
        ));
    }
    let outcome = tool_at(ws, path);
    if let Ok(Ok(tool)) = &outcome {
        return Ok(describe(tool, path));
    }
    // The directory names beside `path`, used TWICE: to prove absence, and to
    // suggest the name the model meant.
    let (parent, leaf) = path.rsplit_once('/').unwrap_or(("", path));
    let nearby = nearby_dirs(ws, parent);
    // A wrong path is the model's to fix, so every answer here is an is_error
    // tool_result rather than a worker failure -- the same choice
    // `grep_precheck` makes for a scope the model named.
    let what = match outcome {
        Ok(Ok(_)) => unreachable!("described above"),
        Ok(Err(reason)) => not_a_tool_message(reason, path),
        // `worker_common::caos` gives back only an exit status; the REASON went
        // to this worker's stderr. So do not quote that error at the model --
        // it would read `caos resolve <hash> … exited with exit status: 1`,
        // which names an internal command line and not the problem.
        //
        // Instead, decide on EVIDENCE: a parent that resolves and does not list
        // this name proves the path is absent. Without that proof this is
        // reported as the CAS trouble it may be — distinguishing absent from
        // broken, rather than assuming the cheerful one.
        Err(error) => match &nearby {
            Some(names) if !names.iter().any(|n| n == leaf) => {
                format!("no such path: {path}")
            }
            _ => format!("cannot read {path}: {error}"),
        },
    };
    Err(User(format!("{what}{}", suggestion(parent, nearby))))
}

/// Why `path` is not a tool, in the model's words. Shared by `tool_help` and
/// `run_tool` so one wrong path reads the same whichever tool met it.
pub fn not_a_tool_message(reason: NotATool, path: &str) -> String {
    match reason {
        NotATool::NoExpression => {
            format!("{path} is not a tool: it is not a directory carrying a `.caos-expr`")
        }
        NotATool::NoHelp => format!(
            "{path} has a `.caos-expr` but it binds no `--help`, so it is an evaluable \
             entry rather than a tool"
        ),
    }
}

/// `tool_help`'s description of one tool, as prose rather than JSON: the model
/// reads this, and a schema dump is the registry we just stopped keeping.
fn describe(tool: &TreeTool, path: &str) -> String {
    let mut out = format!("{path}\n\n{}\n", tool.doc);
    // Stated for readers too, not just writers. "Will this change my tree?" is
    // the second most important fact about a tool, and an absent line would
    // leave the model unable to tell a reader from a description that simply
    // did not mention it.
    out.push_str(if tool.writer {
        "\nWrites: proposes a change to the tree it is run on.\n"
    } else {
        "\nRead-only: returns a value and changes nothing.\n"
    });
    out.push_str(if tool.wants_in {
        "Reads the source tree it is run on.\n"
    } else {
        "Does not read the tree: its arguments are its whole input.\n"
    });
    if tool.args.is_empty() {
        out.push_str("\nParameters: none.\n");
        return out;
    }
    out.push_str("\nParameters (pass under `arguments`, every value a string):\n");
    for a in &tool.args {
        let need = if a.required { "required" } else { "optional" };
        out.push_str(&format!("  {} ({need}) — {}\n", a.name, a.doc));
    }
    out
}

/// The directory names inside `parent` (`""` for the conversation root), or
/// `None` when `parent` itself could not be read as a directory.
///
/// One `caos resolve` and one `caos get` — never a per-child fetch. `None` is
/// load-bearing: it is the difference between "that name is not there" and "I
/// could not look", and [`tool_help`] reports those differently.
fn nearby_dirs(ws: &str, parent: &str) -> Option<Vec<String>> {
    let resolved = fresh("tool-parent");
    let hash = worker_common::cas_hash(ws).ok()?;
    caos(["resolve", &hash, parent, &resolved]).ok()?;
    if !Path::new(&resolved).is_dir() {
        return None;
    }
    caos(["get", &resolved]).ok()?;
    let mut names: Vec<String> = fs::read_dir(&resolved)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    Some(names)
}

/// Where else to look, appended to a failed `tool_help`. Discovery is
/// documentation now, so a wrong path is the ordinary mistake, and naming the
/// candidates turns it into a self-correcting one instead of a round trip.
fn suggestion(parent: &str, nearby: Option<Vec<String>>) -> String {
    let Some(mut names) = nearby.filter(|names| !names.is_empty()) else {
        return String::new();
    };
    names.truncate(50);
    let where_ = if parent.is_empty() {
        "the conversation root".to_string()
    } else {
        parent.to_string()
    };
    format!(". Directories in {where_}: {}", names.join(" "))
}

/// A WRITER tool's result value: `{prop, out, message?, failed?}` (SPEC,
/// "CaosTools").
pub struct WriterResult {
    /// CAS path of the proposal. A TREE — the harness mints the commit — or a
    /// COMMIT, used as-is, which only a tool needing a second parent (merge)
    /// has any reason to build.
    pub prop: String,
    /// `"tree"` or `"commit"`, from the entry's own kind rather than its name:
    /// one entry that says what it is beats two entries named after kinds,
    /// where `prop/tree` holding a commit would be a lie.
    pub kind: String,
    /// The text the model reads.
    pub out: String,
    /// An optional commit message, used when the harness mints. Without it the
    /// mint falls back to the tool's path, which is why a conversation's source
    /// history is otherwise a column of identical one-word messages.
    pub message: Option<String>,
    /// The `failed` entry is PRESENT, so the call is an error. A marker rather
    /// than a banner in `out`, because `out` is arbitrary logs and a build that
    /// printed `FAILED` in passing would condemn a perfectly good proposal.
    pub failed: bool,
}

/// Read a writer's result.
///
/// Every failure here is the TOOL's defect, and it is reported to the model
/// naming the tool rather than failing the worker: a repository tool is code
/// the model may have just written, so a malformed result has to be
/// correctable within the turn instead of taking the turn down.
pub fn writer_result(result: &str) -> Result<WriterResult, String> {
    caos(["get", result])?;
    let dir = Path::new(result);
    if !dir.is_dir() {
        return Err("a writer's result must be a tree of {prop, out}, not a blob".to_string());
    }
    let blob = |name: &str| -> Result<Option<String>, String> {
        let file = dir.join(name);
        if !file.exists() {
            return Ok(None);
        }
        caos(["get", path(&file)])?;
        let bytes = fs::read(&file).map_err(|e| format!("reading {name}: {e}"))?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    };
    let prop = dir.join("prop");
    if !prop.exists() {
        return Err(
            "a writer's result has no `prop` entry — the proposed tree or commit".to_string(),
        );
    }
    let prop = path(&prop).to_string();
    let kind = worker_common::cas_kind(&prop)?;
    if kind != "tree" && kind != "commit" {
        return Err(format!("a writer's `prop` must be a tree or a commit, not a {kind}"));
    }
    // `out` is REQUIRED. Substituting a generic line for a tool that forgot it
    // would hide the tool's silence behind the harness's voice, and the whole
    // point of the entry is that the model learns what happened.
    let out = blob("out")?
        .ok_or("a writer's result has no `out` entry — the text for the model")?;
    Ok(WriterResult {
        prop,
        kind,
        out,
        message: blob("message")?,
        failed: dir.join("failed").exists(),
    })
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
            // An `{array}` parameter arrives as a list and is curried as one
            // NEWLINE-SEPARATED blob, because that is all an arg can be by the
            // time the script reads it. A lone string is accepted as a
            // one-element list rather than refused on a technicality.
            Some(Value::Array(items)) if a.ty == ArgType::Lines => {
                let mut lines = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(line) => lines.push(line.to_string()),
                        None => {
                            return fail(format!(
                                "every entry of {}'s {:?} must be a string",
                                tool.name, a.name
                            ))
                        }
                    }
                }
                lines.join("\n")
            }
            Some(_) => {
                let wanted = match a.ty {
                    ArgType::Text => "a string",
                    ArgType::Lines => "an array of strings",
                };
                return fail(format!("{}'s {:?} must be {wanted}", tool.name, a.name));
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
/// source-tree-relative prefix (`""` for the root) — or, on a user mistake, the
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
    // `resolve` handles all four cases: no root + no path is the source tree
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

/// Execute one inline call against the source tree at CAS path `ws`. Returns the
/// tool_result block and, for a mutation, the new source tree CAS path.
pub fn execute(call: &Value, ws: &str) -> Result<(Value, Option<String>), String> {
    let id = call["id"].as_str().unwrap_or("");
    let name = call["name"].as_str().unwrap_or("");
    let outcome = match name {
        "read" => read(call, ws).map(|text| (text, None)),
        "ls" => ls(call, ws).map(|text| (text, None)),
        "write" => write(call, ws).map(|(text, new_ws)| (text, Some(new_ws))),
        "edit" => edit(call, ws).map(|(text, new_ws)| (text, Some(new_ws))),
        "tool_help" => tool_help(call, ws).map(|text| (text, None)),
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
/// current source tree `ws`; a `root` hash may name a TREE (navigate into
/// it), a COMMIT (navigate into its tree), or a BLOB (a leaf — valid only with
/// no `path`). This is the one place history reads root elsewhere; `read` and
/// `ls` share it, then each checks the node is the kind it wants.
fn resolve(root: Option<&str>, ws: &str, comps: &[String]) -> Result<PathBuf, Fail> {
    let hash = match root {
        Some(hash) => hash.to_string(),
        None => worker_common::cas_hash(ws).map_err(Infra)?,
    };
    let destination = fresh("resolved");
    caos(["resolve", &hash, &comps.join("/"), &destination]).map_err(User)?;
    Ok(PathBuf::from(destination))
}

/// An optional hash-valued input (`root`): trimmed, empty treated as absent.
fn opt_hash(call: &Value, key: &str) -> Option<String> {
    call["input"][key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
            let directory = c.is_dir()
                || (!c.is_symlink()
                    && worker_common::cas_kind(path(c)).map_err(Infra)? == "commit");
            Ok(if directory { format!("{name}/") } else { name })
        })
        .collect::<Result<Vec<_>, Fail>>()?;
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
// SourceTree plumbing.
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

/// Validate and split a source-tree-relative path argument. A leading `/` is
/// tolerated (treated as the source tree root); `..` and the reserved `.caos`
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
        return Err(User("`..` is not allowed in source tree paths".to_string()));
    }
    Ok(comps)
}

/// Walk `comps` down from the source tree root, materializing each level (`caos
/// get` — a no-op when already fetched, hence the ignored result) and
/// returning the leaf path. Missing entries and file-as-directory are user
/// errors.
fn materialize(ws: &str, comps: &[String]) -> Result<PathBuf, Fail> {
    resolve(None, ws, comps)
}

/// Rebuild the source tree with `comps` holding `content` (mode `mode`, default
/// 0644): at each level every untouched entry is symlinked (staging resolves
/// links by recorded hash — nothing else materializes) and the target
/// component is descended into or written. Returns the new source tree CAS path.
fn rebuild(ws: &str, comps: &[String], content: &[u8], mode: Option<u32>) -> Result<String, Fail> {
    let work = scratch(&fresh_name("inline")).map_err(Infra)?;
    let relative = comps.join("/");
    worker_common::files::materialize(ws, &work, std::slice::from_ref(&relative)).map_err(Infra)?;
    let target = work.join(&relative);
    let mut ancestor = work.clone();
    for component in comps {
        ancestor.push(component);
        if ancestor.is_symlink() {
            return Err(User(
                "write/edit cannot follow a symlink; use bash to replace it explicitly".into(),
            ));
        }
    }
    fs::create_dir_all(target.parent().unwrap()).map_err(|e| Infra(e.to_string()))?;
    fs::write(&target, content).map_err(|e| Infra(e.to_string()))?;
    if let Some(mode) = mode {
        fs::set_permissions(&target, fs::Permissions::from_mode(mode))
            .map_err(|e| Infra(e.to_string()))?;
    }
    let out = fresh("files-inline");
    caos(["put", path(&work), &out]).map_err(Infra)?;
    Ok(out)
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
        let h = parse_help("t", help);
        assert_eq!(h.doc, "Print one test's record. A second description line.");
        assert!(!h.git);
        assert!(!h.writer, "a tool is read-only unless it says otherwise");
        assert_eq!(h.args.len(), 2);
        assert_eq!(h.args[0].name, "hash");
        assert!(h.args[0].required);
        assert_eq!(h.args[0].doc, "The record hash.");
        assert_eq!(h.args[1].name, "log");
        assert!(!h.args[1].required);
    }

    #[test]
    fn parse_help_git_tag_and_no_params() {
        let h = parse_help("t", "Just a description.\n@git");
        assert_eq!(h.doc, "Just a description.");
        assert!(h.git);
        assert!(h.args.is_empty());

        // Empty help → empty description (the caller placeholders it) and no args.
        let h = parse_help("t", "");
        assert!(h.doc.is_empty());
        assert!(h.args.is_empty());
        assert!(!h.git);
        assert!(!h.writer);
    }

    #[test]
    fn in_is_opt_in_and_described() {
        // Absent means the tree is NOT bound, which is what keeps it out of the
        // cache key for a tool whose whole input is its arguments.
        let reader = TreeTool::new(
            "caos-test-result",
            parse_help("t", "Print a record.\n@param hash The hash."),
        );
        assert!(!reader.wants_in);
        let text = describe(&reader, "std/caos-test-result");
        assert!(text.contains("Does not read the tree"));

        let tester = TreeTool::new("caos-test", parse_help("t", "Run the suite.\n@in"));
        assert!(tester.wants_in);
        assert!(describe(&tester, "std/caos-test").contains("Reads the source tree"));
        // `@in` is a flag, not a parameter: it must not become an argument the
        // model is asked to supply.
        assert!(tester.args.is_empty());
    }

    #[test]
    fn a_declared_type_reaches_the_schema_and_the_binding() {
        let h = parse_help(
            "t",
            "Run a command.\n@param cmd The command.\n\
             @param {array} [paths] Paths to materialize.\n\
             @param {string} [cwd] Working directory.",
        );
        assert_eq!(h.args.len(), 3);
        // Bare is a string; `{string}` is the same thing said out loud.
        assert_eq!(h.args[0].ty, ArgType::Text);
        assert_eq!(h.args[2].ty, ArgType::Text);
        assert_eq!(h.args[1].ty, ArgType::Lines);
        assert_eq!(h.args[1].name, "paths");
        assert!(!h.args[1].required, "the bracket still marks it optional");
        assert_eq!(h.args[1].doc, "Paths to materialize.");

        let tool = TreeTool::new("bash", h);
        let d = tree_tool_declaration(&tool);
        assert_eq!(d["input_schema"]["properties"]["cmd"]["type"], "string");
        assert_eq!(d["input_schema"]["properties"]["paths"]["type"], "array");
        assert_eq!(
            d["input_schema"]["properties"]["paths"]["items"]["type"],
            "string"
        );
        assert_eq!(d["input_schema"]["required"], json!(["cmd"]));

        // An array binds as ONE newline-separated blob -- all an arg can be by
        // the time a script reads it -- and a lone string is one element.
        let call = json!({"id":"c","input":{"cmd":"ls","paths":["a","b/c"]}});
        let bound = tree_tool_args(&call, &tool).expect("array accepted");
        assert!(bound.contains(&("paths".to_string(), "a\nb/c".to_string())));
        let call = json!({"id":"c","input":{"cmd":"ls","paths":"solo"}});
        let bound = tree_tool_args(&call, &tool).expect("lone string accepted");
        assert!(bound.contains(&("paths".to_string(), "solo".to_string())));

        // A non-string entry is the model's mistake, named as such.
        let call = json!({"id":"c","input":{"cmd":"ls","paths":[1]}});
        let block = tree_tool_args(&call, &tool).expect_err("number refused");
        assert_eq!(block["is_error"], true);

        // An UNKNOWN type fails the whole tag rather than defaulting to a
        // string: silently narrowing an array parameter removes a capability.
        let h = parse_help("t", "d\n@param {nope} x The x.");
        assert!(h.args.is_empty());
    }

    #[test]
    fn writer_is_opt_in_and_independent_of_git() {
        let h = parse_help("t", "Merges a branch in.\n@writer");
        assert!(h.writer);
        assert!(!h.git);
        assert_eq!(h.doc, "Merges a branch in.");

        // Both tags, in either order, and the description still stops at the
        // first one.
        let h = parse_help("t", "Walks history and edits.\n@writer\n@git");
        assert!(h.writer && h.git);
        assert_eq!(h.doc, "Walks history and edits.");
        let h = parse_help("t", "d\n@git\n@writer");
        assert!(h.writer && h.git);
    }

    #[test]
    fn a_tag_this_parser_cannot_act_on_is_never_absorbed() {
        // A misspelled `@writer` must not leave a writer silently read-only:
        // the proposal would be discarded as a value and the edit would just
        // not happen. It is reported (stderr) and kept OUT of the description,
        // which is where an unknown tag would otherwise land.
        let h = parse_help("t", "A tool.\n@wrter\n@param x The x.");
        assert!(!h.writer);
        assert_eq!(h.doc, "A tool.");
        assert_eq!(h.args.len(), 1, "parsing continues past the unknown tag");

        // `@params x` is NOT `@param` with `s x` after it — that once minted an
        // arg named `s` and silently lost the real one.
        let h = parse_help("t", "d\n@params hash The hash.");
        assert!(h.args.is_empty(), "got {:?}", h.args.first().map(|a| &a.name));

        // An `@` inside prose is not a tag. Both of these appear in help and
        // commit text today.
        let h = parse_help("t", "Mail caos@caos or see github.com@evil.example");
        assert_eq!(h.doc, "Mail caos@caos or see github.com@evil.example");
    }

    #[test]
    fn parse_help_skips_reserved_and_malformed_params() {
        // `in` is reserved; `Bad` is not a lowercase name → both skipped, the
        // good one survives.
        let h = parse_help("t", "d\n@param in nope\n@param Bad nope\n@param ok yes");
        assert_eq!(h.args.len(), 1);
        assert_eq!(h.args[0].name, "ok");
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
    fn describe_renders_the_help_a_model_reads() {
        let tool = TreeTool::new(
            "hello",
            parse_help(
                "t",
                "Say hello from the tree.\n@param word The word to echo.\n\
                 @param [suffix] An optional suffix.",
            ),
        );
        let text = describe(&tool, "main/caos-tools/hello");
        // Read-only is STATED, not left to an absent line.
        assert!(text.contains("Read-only"));
        assert!(text.starts_with("main/caos-tools/hello\n"));
        assert!(text.contains("Say hello from the tree."));
        // Requiredness is stated in words, not a JSON `required` array: this is
        // read by a model, and a schema dump is the registry we stopped keeping.
        assert!(text.contains("word (required) — The word to echo."));
        assert!(text.contains("suffix (optional) — An optional suffix."));

        // No `@param` means the source tree IS the input, and saying so is the
        // whole answer — an empty parameter list reads like a missing one.
        let bare = TreeTool::new("t", parse_help("t", "Test everything."));
        let text = describe(&bare, "main/caos-tools/t");
        assert!(text.contains("Parameters: none."));

        // A writer says so, in the one place a model is guaranteed to look.
        let w = TreeTool::new("m", parse_help("t", "Merge a branch.\n@writer"));
        let text = describe(&w, "main/caos-tools/m");
        assert!(text.contains("Writes:"));
        assert!(!text.contains("Read-only"));
    }

    #[test]
    fn not_a_tool_distinguishes_no_expression_from_no_help() {
        // Different fixes, so different messages: add an expression, versus add
        // a `--help` to the one that is already there.
        let a = not_a_tool_message(NotATool::NoExpression, "main/x");
        let b = not_a_tool_message(NotATool::NoHelp, "main/x");
        assert!(a.contains("not a directory carrying a `.caos-expr`"));
        assert!(b.contains("binds no `--help`"));
        assert_ne!(a, b);
    }

    #[test]
    fn read_and_ls_are_inline_and_declared() {
        // Routed in-process (no sub-run). A repository tool cannot shadow them:
        // it is named by path, never by name.
        for t in ["read", "ls", "import_source", "tool_help"] {
            assert!(is_inline(t));
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
        assert!(parse_arg("in The source tree.").is_none());
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
                    ty: ArgType::Text,
                },
                TreeArg {
                    name: "log".to_string(),
                    doc: "Which log.".to_string(),
                    required: false,
                    ty: ArgType::Text,
                },
            ],
            git: false,
            writer: false,
            wants_in: false,
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
            writer: false,
            wants_in: false,
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
                    ty: ArgType::Text,
                },
                TreeArg {
                    name: "suffix".to_string(),
                    doc: String::new(),
                    required: false,
                    ty: ArgType::Text,
                },
            ],
            git: false,
            writer: false,
            wants_in: false,
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
