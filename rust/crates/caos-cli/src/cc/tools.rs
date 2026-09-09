//! The workspace tools, host-side.
//!
//! `std/llm-step/src/tools.rs` implements the same four tools inside a worker,
//! where the workspace is a `/cas` path materialized lazily by `caos get` and
//! rebuilt entry-by-entry with symlinks. None of that applies here: the host
//! has an ordinary git repository and a `GitTransport`, so a tree is read with
//! plumbing and rebuilt through a temporary index — `read-tree`, one
//! `update-index`, `write-tree` — which is the same job `build_level` does by
//! hand, delegated to the thing that owns it.
//!
//! Every tool takes the workspace tree as an argument and a mutation returns a
//! new one. Nothing here touches the checkout, appends an event, or knows what
//! a conversation is: that belongs to the caller, which is what lets the whole
//! read-modify-write run again unchanged when a concurrent writer wins the CAS.

use std::path::PathBuf;

use serde_json::Value;

use caos::{cli_get, eval_workspace_path, run_client_request_with_store, GitTransport, Transport};

/// Reads larger than this are truncated with a note, matching the worker's
/// inline tools so a model sees one behavior wherever it runs.
const MAX_READ_BYTES: usize = 100_000;
/// Git's mode for an ordinary file. A rewritten file keeps whatever mode it
/// had — losing an exec bit through an edit would be a silent breakage.
const REGULAR_FILE: &str = "100644";

/// The harness reserves this top-level path for its own state, and both
/// `reject_reserved_caos` and the protocol refuse a tree carrying it.
const RESERVED: &str = ".caos";

/// A tool failure the model should see and can act on, as distinct from a
/// broken repository or transport, which must stop the process. The worker
/// draws this same line (`Fail::User` vs `Fail::Infra`) and for the same
/// reason: a missing file is a normal conversational event, an unreadable
/// object store is not.
#[derive(Debug)]
pub enum ToolError {
    User(String),
    Infra(String),
}

use ToolError::{Infra, User};

impl ToolError {
    fn infra(error: String) -> ToolError {
        Infra(error)
    }
}

/// What a tool BODY produced: text, and the new workspace tree for a mutation.
///
/// Separate from [`Outcome`] because a tool body naturally computes a tree —
/// it rewrites one path in one tree — while the CONTRACT is stated in commits.
/// `execute` mints the commit once, in one place, rather than in every tool.
pub struct TreeOutcome {
    pub text: String,
    pub tree: Option<String>,
}

impl TreeOutcome {
    fn read(text: String) -> TreeOutcome {
        TreeOutcome { text, tree: None }
    }

    fn wrote(text: String, tree: String) -> TreeOutcome {
        TreeOutcome {
            text,
            tree: Some(tree),
        }
    }
}

/// What a tool produced: text for the model, and — for a mutation — the
/// workspace COMMIT that replaces the one it was given.
///
/// A COMMIT, not a tree, because that is the contract (SPEC, "Tools thread a
/// commit, not a tree"): a read returns the input commit unchanged, a mutation
/// returns `commit(new tree, parent = input commit)`, and `merge` returns a
/// two-parent commit. Threading a tree instead loses `theirs` — the merged
/// content survives and the ancestry does not, which is the one thing the
/// commit threading exists to carry.
pub struct Outcome {
    pub text: String,
    pub commit: Option<String>,
}

impl Outcome {
    fn read(text: String) -> Outcome {
        Outcome { text, commit: None }
    }

    fn wrote(text: String, commit: String) -> Outcome {
        Outcome {
            text,
            commit: Some(commit),
        }
    }
}

/// `commit(tree, parents)` with `what` as its message — a workspace commit,
/// not a conversation event.
///
/// The object is BUILT and stored rather than shelled out to `git
/// commit-tree`, so it is a pure function of its inputs: `commit-tree` stamps
/// the current time, which would give a retry of the same call a different
/// object every attempt. The parent's own author line is reused for the same
/// reason `llm-step`'s `advance_wc` reuses its timestamp.
fn mint_workspace_commit(
    t: &GitTransport,
    tree: &str,
    parents: &[&str],
    what: &str,
) -> Result<String, ToolError> {
    let (kind, parent_bytes) = t.get_object(parents[0]).map_err(ToolError::infra)?;
    if kind != "commit" {
        return Err(Infra(format!("{} is a {kind}, not a commit", parents[0])));
    }
    let parent_text = String::from_utf8_lossy(&parent_bytes);
    let ident = parent_text
        .lines()
        .find_map(|line| line.strip_prefix("author "))
        .ok_or_else(|| Infra(format!("commit {} has no author line", parents[0])))?;
    let mut body = format!("tree {tree}\n");
    for parent in parents {
        body.push_str(&format!("parent {parent}\n"));
    }
    body.push_str(&format!("author {ident}\ncommitter {ident}\n\n{what}\n"));
    t.put_object("commit", body.as_bytes())
        .map(|oid| oid.to_string())
        .map_err(ToolError::infra)
}

/// Run one tool against `tree`. `args` is the tool's arguments exactly as the
/// model supplied them.
///
/// `wc` is the conversation's head COMMIT, of which `tree` is the tree. Only a
/// `@git` tool uses it — history is not readable from a tree — and it is passed
/// rather than re-derived because the caller has already resolved the head it
/// is appending to, and a second lookup could see a different one.
pub fn execute(t: &GitTransport, wc: &str, name: &str, args: &Value) -> Result<Outcome, ToolError> {
    // The tree is DERIVED, not passed: a tool's unit of work is the commit, and
    // deriving the tree from it here is what keeps the two from disagreeing.
    let tree = &rev_parse(t, &format!("{wc}^{{tree}}"))
        .ok_or_else(|| Infra(format!("workspace commit {wc} has no tree")))?;
    match name {
        "read" => wrote(t, wc, "read", read(t, tree, args)?),
        "ls" => wrote(t, wc, "ls", ls(t, tree, args)?),
        "write" => wrote(t, wc, "write", write(t, tree, args)?),
        "edit" => wrote(t, wc, "edit", edit(t, tree, args)?),
        "grep" => wrote(t, wc, "grep", grep(t, tree, args)?),
        // BEFORE the std-tool arm: `merge` is described from its help like a
        // std tool, but it is launched with `ours`/`theirs` rather than
        // declared params, and its result is a COMMIT that advances the
        // workspace rather than a report to read.
        "merge" => merge(t, wc, args),
        "bash" => wrote(t, wc, "bash", bash(t, tree, args)?),
        name if std_tool_entry(name).is_some() => {
            wrote(t, wc, name, std_tool(t, tree, name, args)?)
        }
        other => Err(User(format!("unknown tool {other:?}"))),
    }
}

/// Turn a mutation's new TREE into the commit the contract says it returns:
/// `commit(new tree, parent = input commit)`. A tool that changed nothing
/// passes through untouched -- SPEC is explicit that a read mints no no-op
/// commit.
fn wrote(
    t: &GitTransport,
    wc: &str,
    what: &str,
    outcome: TreeOutcome,
) -> Result<Outcome, ToolError> {
    match outcome.tree {
        None => Ok(Outcome::read(outcome.text)),
        Some(tree) => {
            let commit = mint_workspace_commit(t, &tree, &[wc], what)?;
            Ok(Outcome::wrote(outcome.text, commit))
        }
    }
}

fn read(t: &GitTransport, tree: &str, args: &Value) -> Result<TreeOutcome, ToolError> {
    let path = path_arg(args, "file_path")?;
    let (kind, bytes) = object_at(t, tree, &path)?;
    if kind != "blob" {
        return Err(User(format!("{path} is a directory; use ls")));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| User(format!("{path} is not valid UTF-8; it is a binary file")))?;
    Ok(TreeOutcome::read(window(&text, args)))
}

/// Apply the `offset`/`limit` window, then the byte cap. A truncated read says
/// so: silently returning a prefix would let a model conclude a file ends where
/// the cap fell.
fn window(text: &str, args: &Value) -> String {
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1);
    let limit = args.get("limit").and_then(Value::as_u64);
    let start = offset.saturating_sub(1) as usize;
    let mut lines: Vec<&str> = text.lines().skip(start).collect();
    if let Some(limit) = limit {
        lines.truncate(limit as usize);
    }
    let windowed = lines.join("\n");
    match windowed.len() > MAX_READ_BYTES {
        false => windowed,
        true => {
            let mut end = MAX_READ_BYTES;
            while end > 0 && !windowed.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}\n\n[truncated at {MAX_READ_BYTES} bytes; use offset/limit to page]",
                &windowed[..end]
            )
        }
    }
}

fn ls(t: &GitTransport, tree: &str, args: &Value) -> Result<TreeOutcome, ToolError> {
    let path = optional_path_arg(args, "path")?;
    let spec = match path.as_deref() {
        None | Some("") => tree.to_string(),
        Some(path) => format!("{tree}:{path}"),
    };
    let oid = rev_parse(t, &spec).ok_or_else(|| match path.as_deref() {
        Some(path) => User(format!("no such path: {path}")),
        None => Infra("the workspace tree does not resolve".to_string()),
    })?;
    let listing = t
        .git_capture(&["ls-tree", "--format=%(objecttype) %(path)", &oid], None)
        .map_err(|error| match error.contains("not a tree") {
            true => User(format!(
                "{} is a file, not a directory; use read",
                path.unwrap_or_default()
            )),
            false => Infra(error),
        })?;
    let mut entries = Vec::new();
    for line in listing.lines() {
        let Some((kind, name)) = line.split_once(' ') else {
            continue;
        };
        entries.push(match kind {
            "tree" => format!("{name}/"),
            _ => name.to_string(),
        });
    }
    Ok(TreeOutcome::read(match entries.is_empty() {
        true => "(empty directory)".to_string(),
        false => entries.join("\n"),
    }))
}

fn write(t: &GitTransport, tree: &str, args: &Value) -> Result<TreeOutcome, ToolError> {
    let path = path_arg(args, "file_path")?;
    let content = string_arg(args, "content")?;
    let tree = put_file(t, tree, &path, content.as_bytes())?;
    Ok(TreeOutcome::wrote(
        format!("wrote {} bytes to {path}", content.len()),
        tree,
    ))
}

fn edit(t: &GitTransport, tree: &str, args: &Value) -> Result<TreeOutcome, ToolError> {
    let path = path_arg(args, "file_path")?;
    let old = string_arg(args, "old_string")?;
    let new = string_arg(args, "new_string")?;
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if old.is_empty() {
        return Err(User("edit needs a non-empty old_string".to_string()));
    }
    let (kind, bytes) = object_at(t, tree, &path)?;
    if kind != "blob" {
        return Err(User(format!("{path} is a directory")));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| User(format!("{path} is not valid UTF-8; it cannot be edited")))?;
    let hits = text.matches(old).count();
    let (updated, replaced) = match (hits, replace_all) {
        (0, _) => return Err(User(format!("old_string does not appear in {path}"))),
        (n, false) if n > 1 => {
            return Err(User(format!(
                "old_string appears {n} times in {path}; \
                 include surrounding context to make it unique, or pass replace_all"
            )))
        }
        (n, true) => (text.replace(old, new), n),
        (_, false) => (text.replacen(old, new, 1), 1),
    };
    let tree = put_file(t, tree, &path, updated.as_bytes())?;
    let times = match replaced {
        1 => "1 occurrence".to_string(),
        n => format!("{n} occurrences"),
    };
    Ok(TreeOutcome::wrote(
        format!("replaced {times} in {path}"),
        tree,
    ))
}

/// Search the workspace by running the `grep` std tool.
///
/// This is the DISPATCHED tool path: unlike read/ls/write/edit it does not
/// compute an answer locally, it builds an ArgTree and runs it. Nothing here is
/// grep-specific below the argument names — `std/rgrep-tool` resolves the
/// scope, drives the `std/rgrep` fold, and renders the result itself, returning
/// the ordinary `{report}` tree. That is what lets `bash` and every
/// `caos-tools/<name>` entry reuse `run_std_tool` unchanged.
fn grep(t: &GitTransport, tree: &str, args: &Value) -> Result<TreeOutcome, ToolError> {
    let pattern = string_arg(args, "pattern")?;
    let mut kvs = vec![format!("--pattern={pattern}")];
    if let Some(path) = optional_path_arg(args, "path")? {
        kvs.push(format!("--path={path}"));
    }
    run_std_tool(t, tree, "std/rgrep-tool", &kvs).map(TreeOutcome::read)
}

/// Run a shell command through `std/bash-tool`.
///
/// Unlike every other tool here this one MUTATES: its result carries the
/// workspace the command left behind, under `tree`, and that becomes the
/// conversation's new workspace. `exit` decides whether the model sees an
/// error. Both are data the caller consumes; the text it shows comes from the
/// tool's own `report`, so bash reads identically here and in `llm-step`.
fn bash(t: &GitTransport, tree: &str, args: &Value) -> Result<TreeOutcome, ToolError> {
    let cmd = string_arg(args, "cmd")?;
    let paths = match args.get("paths") {
        None | Some(Value::Null) => String::new(),
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|entry| match entry.as_str() {
                Some(path) => Ok(path.to_string()),
                None => Err(User("every entry in `paths` must be a string".to_string())),
            })
            .collect::<Result<Vec<_>, ToolError>>()?
            .join("\n"),
        Some(Value::String(single)) => single.clone(),
        Some(_) => return Err(User("`paths` must be an array of strings".to_string())),
    };

    let input = bash_input(t, tree, cmd, &paths)?;
    let result = run_tool(t, &input, "std/bash-tool", &[])?;
    let workspace = result
        .entry("tree")
        .ok_or_else(|| Infra("bash result carries no `tree` entry".to_string()))?;
    let exit = result.leaf("exit").unwrap_or_default();
    let text = result.report()?;
    match exit.trim() {
        "0" => Ok(TreeOutcome::wrote(text, workspace)),
        // A non-zero exit is a value, not a failure: the model must read stderr
        // and react. The workspace still advances — the command may have written
        // files before it failed, exactly as `llm-step` treats it.
        _ => Err(User(text)),
    }
}

/// Build the `{tree, cmd, paths}` input `std/bash-tool` expects.
///
/// This is the shape `llm-step` builds, byte for byte, and that is the point:
/// the ArgTree is the cache key, so a command run from the tui and the same
/// command run from Claude Code are ONE cached job rather than two. Passing
/// bash-tool's direct `--cmd`/`--tree` arguments instead would work and would
/// silently fork the cache.
///
/// `paths` is always written, empty included, for the same reason — an absent
/// entry and an empty one are different trees.
fn bash_input(t: &GitTransport, tree: &str, cmd: &str, paths: &str) -> Result<String, ToolError> {
    let index = scratch_path(t, "bash-index")?;
    let _ = std::fs::remove_file(&index);
    // `read-tree --prefix` mounts the workspace under `tree/` without reading a
    // single blob: the entries are copied by oid.
    t.git_capture(&["read-tree", "--prefix=tree/", tree], Some(&index))
        .map_err(ToolError::infra)?;
    for (name, content) in [("cmd", cmd), ("paths", paths)] {
        let blob = t
            .put_object("blob", content.as_bytes())
            .map_err(ToolError::infra)?
            .to_string();
        let cacheinfo = format!("{REGULAR_FILE},{blob},{name}");
        t.git_capture(
            &["update-index", "--add", "--cacheinfo", &cacheinfo],
            Some(&index),
        )
        .map_err(ToolError::infra)?;
    }
    let input = t
        .git_capture(&["write-tree"], Some(&index))
        .map_err(ToolError::infra)?
        .trim()
        .to_string();
    let _ = std::fs::remove_file(&index);
    Ok(input)
}

/// The harness's own std tools, offered always, exactly as `llm-step` offers
/// them (its `registry` binds their images through its `.caos-expr`). Named by
/// path here for the same reason `grep` is — the client resolves an ordinary
/// tree path rather than reaching into anyone's `DEEP-DEPS`.
///
/// `grep` is in this list too: it IS one of these, and having arrived first it
/// simply has its own dispatch arm above.
pub fn std_tool_entry(name: &str) -> Option<&'static str> {
    match name {
        "caos-build" => Some("std/caos-build"),
        "caos-test" => Some("std/caos-test"),
        "caos-test-result" => Some("std/caos-test-result"),
        "merge" => Some("std/merge"),
        // The history tools. `@git` in their help is what makes the launch
        // hand them the conversation's commit; nothing else here differs.
        "log" => Some("std/log-tool"),
        "show" => Some("std/show-tool"),
        "diff" => Some("std/diff-tool"),
        _ => None,
    }
}

/// Run a std tool with the model's declared arguments.
///
/// The arguments are whatever the tool's own `help` declares, so nothing here
/// knows what `caos-test` takes — adding a std tool to the list above is the
/// whole change.
fn std_tool(
    t: &GitTransport,
    tree: &str,
    name: &str,
    args: &Value,
) -> Result<TreeOutcome, ToolError> {
    let entry = std_tool_entry(name).ok_or_else(|| User(format!("unknown tool {name:?}")))?;
    // The same check `declarations` makes, for the same reason: the name map
    // says yes in any repository, and describing the entry would push this
    // workspace to caos before discovering that the entry is not in it.
    if !t.work_dir().join(entry).is_dir() {
        return Err(User(format!(
            "{name} needs {entry}, which is not in this workspace \
             (it is a caos repository tool)"
        )));
    }
    let declared = describe_std_tool(t, entry)?;
    let mut kvs = Vec::new();
    for param in &declared.params {
        match args.get(&param.name) {
            None | Some(Value::Null) => {
                if param.required {
                    return Err(User(format!("{name} needs `{}`", param.name)));
                }
            }
            Some(Value::String(value)) => kvs.push(format!("--{}={value}", param.name)),
            // Every arg reaches a worker as a blob whatever JSON type it left
            // the model as, so a scalar is rendered rather than refused.
            Some(Value::Bool(value)) => kvs.push(format!("--{}={value}", param.name)),
            Some(Value::Number(value)) => kvs.push(format!("--{}={value}", param.name)),
            Some(_) => return Err(User(format!("{name}'s `{}` must be a string", param.name))),
        }
    }
    // The `@git` context, for a tool whose help asked for it. `:commit=` is
    // what passes the commit UNPEELED -- the default forms peel a commit to its
    // tree, and a tree has no parents to walk.
    if declared.git {
        // The workspace commit this tree belongs to. Derived rather than
        // threaded: `execute` already turned the commit into this tree, and
        // asking git for the commit again would be a second source of truth.
        let wc = rev_parse(t, &format!("{tree}^{{commit}}")).unwrap_or_default();
        kvs.push(format!("--wc:commit={wc}"));
        // The same snapshot `llm-step` binds, so a revision can be named
        // (`main`) and not only hashed. Absent is not an error: without it the
        // tools still take HEAD, HEAD~N and a hash.
        match crate::snapshot_merge_refs(t) {
            Ok(refs) if !refs.is_empty() => kvs.push(format!("--refs={refs}")),
            Ok(_) => {}
            Err(error) => return Err(Infra(error)),
        }
    }
    run_std_tool(t, tree, entry, &kvs).map(TreeOutcome::read)
}

/// Three-way merge another commit into the conversation's workspace.
///
/// Unlike every other tool here the ANCESTRY advances: the result is a merge
/// commit with two parents, and the workspace this returns is that commit's
/// tree. The conversation's own event commit is then built on it by the caller,
/// so the merge is recorded as the step it was.
fn merge(t: &GitTransport, wc: &str, args: &Value) -> Result<Outcome, ToolError> {
    let entry = std_tool_entry("merge").expect("merge is a std tool entry");
    if !t.work_dir().join(entry).is_dir() {
        return Err(User(format!(
            "merge needs {entry}, which is not in this workspace"
        )));
    }
    let theirs = string_arg(args, "theirs")?.trim().to_string();
    if theirs.is_empty() {
        return Err(User("merge needs `theirs`".to_string()));
    }
    let refs = crate::snapshot_merge_refs(t).map_err(Infra)?;
    let resolved = resolve_theirs(&refs, &theirs)?;

    // `:commit=` on both sides: the default arg forms peel a commit to its
    // tree, and a merge needs the commits themselves to find a merge base.
    let image = std_tool_image(t, entry)?;
    let curried = caos::curry_client_object(
        t,
        &image,
        &[
            format!("--ours:commit={wc}"),
            format!("--theirs:commit={resolved}"),
        ],
    )
    .map_err(ToolError::infra)?
    .to_string();
    let (kind, result) =
        run_client_request_with_store(t, &curried, &[], &[]).map_err(ToolError::infra)?;
    if kind != "commit" {
        return Err(Infra(format!("merge returned a {kind}, not a commit")));
    }
    // The merged TREE, only to read the conflict list out of. What the tool
    // RETURNS is the commit: it carries `theirs` as a second parent, and a
    // tree does not.
    let merged = commit_tree_of(t, &result)?;

    // The conflict list is the whole difference between a clean merge and one
    // the model has to finish, so it is read from the merged tree rather than
    // inferred from an exit status.
    let conflicts = match object_at(t, &merged, ".caos/conflicts") {
        Ok((_, bytes)) => Some(String::from_utf8_lossy(&bytes).trim_end().to_string()),
        Err(_) => None,
    };
    let text = match conflicts {
        Some(body) => format!(
            "merge produced conflicts. The workspace now carries git's inline conflict markers \
             in the affected files, plus .caos/conflicts (git's unmerged notation, richer than \
             markers). Resolve each path — edit the file, reading a stage's content with `read` \
             (pass the stage oid as `root`) — then delete that path's rows from .caos/conflicts. \
             Build and test when done.\n\n.caos/conflicts:\n{body}"
        ),
        None => "merge completed cleanly; the workspace is the merged result.".to_string(),
    };
    Ok(Outcome::wrote(text, result))
}

/// `theirs` against the turn's ref snapshot (SPEC "Resolving `--theirs`"): a
/// known name becomes its hash, a bare hash is itself, and anything else is a
/// user error listing what the snapshot does offer.
fn resolve_theirs(refs: &str, theirs: &str) -> Result<String, ToolError> {
    if theirs.len() == 40 && theirs.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(theirs.to_string());
    }
    for line in refs.lines() {
        if let Some((name, hash)) = line.rsplit_once(char::is_whitespace) {
            if name.trim() == theirs {
                return Ok(hash.trim().to_string());
            }
        }
    }
    let names: Vec<&str> = refs
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    Err(User(match names.is_empty() {
        true => format!("cannot resolve {theirs:?}: this turn has no ref snapshot, so `theirs` must be a full commit hash"),
        false => format!("cannot resolve {theirs:?}: give a full commit hash or one of {}", names.join(", ")),
    }))
}

/// The `tree` line of a commit the server holds.
pub fn commit_tree_of(t: &GitTransport, commit: &str) -> Result<String, ToolError> {
    let (kind, bytes) = t.get_object(commit).map_err(ToolError::infra)?;
    if kind != "commit" {
        return Err(Infra(format!("{commit} is a {kind}, not a commit")));
    }
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .find_map(|line| line.strip_prefix("tree "))
        .map(str::to_string)
        .ok_or_else(|| Infra(format!("commit {commit} has no tree line")))
}

/// A std tool's docs and parameters, read from the `help` its image carries.
///
/// The same source `llm-step`'s `std_tool` reads, in the same format, so a tool
/// is described one way wherever it is offered and rewording it needs no change
/// here. Parsing mirrors `parse_help`/`parse_arg`: free description text, then
/// `@param [name] doc` lines, with brackets meaning optional.
pub struct StdToolHelp {
    pub doc: String,
    pub params: Vec<StdToolParam>,
    /// The tool declared `@git`: it reads history, so the launch owes it the
    /// workspace commit (`wc`) and the turn's ref snapshot (`refs`).
    pub git: bool,
}

pub struct StdToolParam {
    pub name: String,
    pub doc: String,
    pub required: bool,
}

pub fn describe_std_tool(t: &GitTransport, entry: &str) -> Result<StdToolHelp, ToolError> {
    let image = std_tool_image(t, entry)?;
    // Top level when a tool is curried onto a plain image, under `args/` when
    // its base is itself a curry node — which is what a rustc-built worker is.
    // `llm-step`'s `std_tool` looks in both for the same reason.
    let help = ["help", "args/help"]
        .iter()
        .find_map(|at| {
            let oid = rev_parse(t, &format!("{image}:{at}"))?;
            let (_, bytes) = t.get_object(&oid).ok()?;
            String::from_utf8(bytes).ok()
        })
        .ok_or_else(|| Infra(format!("{entry} carries no help")))?;
    Ok(parse_help(&help))
}

fn parse_help(text: &str) -> StdToolHelp {
    let mut doc: Vec<&str> = Vec::new();
    let mut params = Vec::new();
    let mut in_tags = false;
    let mut git = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "@git" {
            in_tags = true;
            git = true;
            continue;
        }
        match trimmed.strip_prefix("@param") {
            Some(rest) => {
                in_tags = true;
                if let Some(param) = parse_param(rest.trim()) {
                    params.push(param);
                }
            }
            // Any other block tag ends the description without becoming one.
            None if trimmed.starts_with('@') => in_tags = true,
            None if !in_tags => doc.push(trimmed),
            None => {}
        }
    }
    StdToolHelp {
        doc: doc.join(" ").trim().to_string(),
        params,
        git,
    }
}

fn parse_param(payload: &str) -> Option<StdToolParam> {
    let (token, doc) = match payload.split_once(char::is_whitespace) {
        Some((token, doc)) => (token, doc.trim()),
        None => (payload, ""),
    };
    let (name, required) = match token.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        Some(inner) => (inner, false),
        None => (token, true),
    };
    let ok = !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then(|| StdToolParam {
        name: name.to_string(),
        doc: doc.to_string(),
        required,
    })
}

/// Run a std tool over `tree` and return its report.
fn run_std_tool(
    t: &GitTransport,
    input: &str,
    entry: &str,
    kvs: &[String],
) -> Result<String, ToolError> {
    run_tool(t, input, entry, kvs)?.report()
}

/// A finished tool result, checked out so its parts can be read.
///
/// Checking out is what makes the result readable at all: it lives on the
/// server, and `cli_get` is the host form of fetching it. NOT
/// `fetch_and_materialize` — that is the worker's CAS form and writes
/// hash-tagged placeholders for a later `caos get` to fill, so on the host every
/// file arrives zero-length and a correct result reads as an empty one.
struct ToolResult {
    dir: PathBuf,
    /// Entry name to object id, for the parts that are OBJECTS rather than text
    /// — `bash`'s `tree` is a workspace, not something to read.
    entries: Vec<(String, String)>,
    kind: String,
}

impl Drop for ToolResult {
    fn drop(&mut self) {
        // Only the checkout is temporary; the objects it fetched stay in the
        // repo, which is what makes a repeated call cheap.
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_file(&self.dir);
    }
}

impl ToolResult {
    fn entry(&self, name: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(entry, _)| entry == name)
            .map(|(_, oid)| oid.clone())
    }

    fn leaf(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(name)).ok()
    }

    /// The text a model reads, by the convention every caos caller follows: a
    /// tree carrying a `report` blob IS that report, a tree without one is named
    /// by its entries, and a blob result is its own text. `llm-step`'s
    /// `tree_tool_result_block` and `run-tool`'s `report_conventions` apply the
    /// same three rules.
    fn report(&self) -> Result<String, ToolError> {
        if self.kind != "tree" && !self.dir.is_dir() {
            return std::fs::read_to_string(&self.dir)
                .map(|text| text.trim_end().to_string())
                .map_err(|error| Infra(format!("reading tool result: {error}")));
        }
        if let Some(report) = self.leaf("report") {
            let text = report.trim_end().to_string();
            // The one signal a tool has to say the CALL was wrong, as opposed to
            // the work failing: `tree_tool_result_block` marks it is_error too.
            return match text.contains("FAILED") {
                true => Err(User(text)),
                false => Ok(text),
            };
        }
        let mut names: Vec<&str> = self.entries.iter().map(|(name, _)| name.as_str()).collect();
        names.sort();
        Ok(format!("result tree: {}", names.join(" ")))
    }
}

/// Run `entry` with `input` as its `--in`.
///
/// For most tools `input` IS the workspace tree, which is what
/// `launch_std_tool` hands a std tool. `bash-tool` wants a `{tree, cmd, paths}`
/// bundle instead, and reads `--in` in preference to its direct arguments — so
/// the caller decides what `in` means, and this stays the one dispatch path.
fn run_tool(
    t: &GitTransport,
    input: &str,
    entry: &str,
    kvs: &[String],
) -> Result<ToolResult, ToolError> {
    let image = std_tool_image(t, entry)?;
    let curried = caos::curry_client_object(t, &image, kvs)
        .map_err(ToolError::infra)?
        .to_string();
    let (kind, result) =
        run_client_request_with_store(t, &curried, &[format!("--in:hash={input}")], &[])
            .map_err(ToolError::infra)?;
    ToolResult::open(t, &result, &kind)
}

impl ToolResult {
    /// Check a finished result out so its parts can be read.
    fn open(t: &GitTransport, result: &str, kind: &str) -> Result<ToolResult, ToolError> {
        let dir = scratch_path(t, "tool")?;
        let path = dir
            .to_str()
            .ok_or_else(|| Infra(format!("scratch path is not UTF-8: {}", dir.display())))?;
        cli_get(t, result, path).map_err(ToolError::infra)?;
        // `ls-tree` only sees local objects, and the checkout above is what
        // brought them down — reading the entries before it would fail on a
        // result that is perfectly fine.
        let mut entries = Vec::new();
        if kind == "tree" {
            let listing = t
                .git_capture(&["ls-tree", "--format=%(objectname) %(path)", result], None)
                .map_err(ToolError::infra)?;
            for line in listing.lines() {
                if let Some((oid, name)) = line.split_once(' ') {
                    entries.push((name.to_string(), oid.to_string()));
                }
            }
        }
        Ok(ToolResult {
            dir,
            entries,
            kind: kind.to_string(),
        })
    }
}

/// A std tool's image, resolved once per server process per entry.
///
/// Resolving walks and hashes the whole worktree, which is not something to
/// repeat per call in a process that stays alive for a session. A failure is
/// deliberately not cached: a server that started before `nix build` finished
/// should recover on the next call rather than stay broken for the session.
fn std_tool_image(t: &GitTransport, entry: &str) -> Result<String, ToolError> {
    static IMAGES: std::sync::Mutex<Option<Vec<(String, String)>>> = std::sync::Mutex::new(None);
    let mut cache = IMAGES.lock().map_err(|error| Infra(error.to_string()))?;
    let cache = cache.get_or_insert_with(Vec::new);
    if let Some((_, image)) = cache.iter().find(|(name, _)| name == entry) {
        return Ok(image.clone());
    }
    // An ordinary tree path. `eval_path` walks it, evaluating any `.caos-expr`
    // it meets and continuing inside the result — so the root expression deepens
    // the tree before the descent reaches the entry, and the entry's own
    // `DEEP-DEPS/` mounts exist by the time its expression names them.
    let image = eval_workspace_path(t, entry, &[]).map_err(Infra)?;
    cache.push((entry.to_string(), image.clone()));
    Ok(image)
}

// ---------------------------------------------------------------------------
// Git plumbing
// ---------------------------------------------------------------------------

/// Write `content` at `path` in `tree`, returning the new tree.
///
/// The temporary index is what makes this a few lines rather than a recursive
/// per-level rebuild: `read-tree` loads the whole tree, one `update-index`
/// replaces exactly one entry, and `write-tree` rewrites only the parent chain
/// that changed. Every untouched entry keeps its existing object, so the new
/// tree shares all of its unchanged structure with the old one.
fn put_file(t: &GitTransport, tree: &str, path: &str, content: &[u8]) -> Result<String, ToolError> {
    let mode = existing_mode(t, tree, path).unwrap_or_else(|| REGULAR_FILE.to_string());
    let blob = t
        .put_object("blob", content)
        .map_err(ToolError::infra)?
        .to_string();
    let index = index_path(t)?;
    t.git_capture(&["read-tree", tree], Some(&index))
        .map_err(ToolError::infra)?;
    let cacheinfo = format!("{mode},{blob},{path}");
    t.git_capture(
        &["update-index", "--add", "--cacheinfo", &cacheinfo],
        Some(&index),
    )
    .map_err(|error| match error.contains("not a directory") {
        // A path whose parent is an existing FILE. Git reports this as a
        // cache-entry conflict, which is a user error, not a broken repo.
        true => User(format!("a parent of {path} is a file, not a directory")),
        false => Infra(error),
    })?;
    let new_tree = t
        .git_capture(&["write-tree"], Some(&index))
        .map_err(ToolError::infra)?
        .trim()
        .to_string();
    let _ = std::fs::remove_file(&index);
    Ok(new_tree)
}

/// A private scratch path inside the git dir, named per process so two servers
/// in one checkout cannot collide.
fn scratch_path(t: &GitTransport, what: &str) -> Result<PathBuf, ToolError> {
    let git_dir = t
        .git_capture(&["rev-parse", "--absolute-git-dir"], None)
        .map_err(ToolError::infra)?
        .trim()
        .to_string();
    Ok(PathBuf::from(git_dir).join(format!("caos-cc-{what}-{}", std::process::id())))
}

/// A private index file, never git's own: `update-index` here must not disturb
/// the user's staged changes. Named per process so two concurrent tools in the
/// same checkout cannot corrupt each other's rebuild.
fn index_path(t: &GitTransport) -> Result<PathBuf, ToolError> {
    scratch_path(t, "index")
}

fn existing_mode(t: &GitTransport, tree: &str, path: &str) -> Option<String> {
    let listing = t.git_capture(&["ls-tree", tree, "--", path], None).ok()?;
    listing
        .split_whitespace()
        .next()
        .filter(|mode| mode.len() == 6)
        .map(str::to_string)
}

fn object_at(t: &GitTransport, tree: &str, path: &str) -> Result<(String, Vec<u8>), ToolError> {
    let oid = rev_parse(t, &format!("{tree}:{path}"))
        .ok_or_else(|| User(format!("no such path: {path}")))?;
    t.get_object(&oid).map_err(ToolError::infra)
}

fn rev_parse(t: &GitTransport, spec: &str) -> Option<String> {
    let oid = t
        .git_capture(&["rev-parse", "--verify", "--quiet", spec], None)
        .ok()?
        .trim()
        .to_string();
    match oid.is_empty() {
        true => None,
        false => Some(oid),
    }
}

// ---------------------------------------------------------------------------
// Argument handling
// ---------------------------------------------------------------------------

fn string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| User(format!("{key} is required and must be a string")))
}

fn path_arg(args: &Value, key: &str) -> Result<String, ToolError> {
    let raw = string_arg(args, key)?;
    normalize(raw)?.ok_or_else(|| User(format!("{key} must name a file, not the workspace root")))
}

fn optional_path_arg(args: &Value, key: &str) -> Result<Option<String>, ToolError> {
    match args.get(key).and_then(Value::as_str) {
        None => Ok(None),
        Some(raw) => normalize(raw),
    }
}

/// Reduce a model-supplied path to a workspace-relative one, refusing anything
/// that could escape the tree or claim the harness's reserved state.
///
/// This runs before the path reaches git, because `rev-parse tree:../x` and an
/// absolute path are both things git will happily interpret — just not as the
/// workspace-relative path the tool contract promises.
fn normalize(raw: &str) -> Result<Option<String>, ToolError> {
    let trimmed = raw.trim().trim_start_matches("./");
    if trimmed.starts_with('/') {
        return Err(User(format!(
            "{raw:?} is absolute; paths are relative to the workspace root"
        )));
    }
    let mut parts = Vec::new();
    for part in trimmed.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                return Err(User(format!(
                    "{raw:?} leaves the workspace; paths may not contain `..`"
                )))
            }
            part => parts.push(part),
        }
    }
    if parts.first().is_some_and(|first| *first == RESERVED) {
        return Err(User(format!(
            "{RESERVED} is reserved for harness state and cannot be read or written"
        )));
    }
    match parts.is_empty() {
        true => Ok(None),
        false => Ok(Some(parts.join("/"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `@git` is a FLAG, not a parameter. Dropped, a history tool launches
    /// without `wc` and fails inside its container with no workspace commit;
    /// treated as a param, the model is offered an argument it must not pass.
    #[test]
    fn a_git_tag_is_a_flag_and_not_a_parameter() {
        let help = parse_help("Reads history.\n@param [rev] Where to start.\n@git");
        assert!(help.git, "@git did not set the flag");
        assert_eq!(help.doc, "Reads history.");
        let names: Vec<&str> = help.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["rev"], "@git leaked into the parameters");
    }

    #[test]
    fn a_tool_without_a_git_tag_gets_no_git_context() {
        let help = parse_help("Builds.\n@param [only] Which.");
        assert!(!help.git);
    }

    #[test]
    fn paths_are_reduced_to_workspace_relative() {
        assert_eq!(
            normalize("src/lib.rs").unwrap().as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            normalize("./src//lib.rs").unwrap().as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(normalize("  a/b  ").unwrap().as_deref(), Some("a/b"));
        assert_eq!(normalize(".").unwrap(), None);
        assert_eq!(normalize("").unwrap(), None);
    }

    /// A path is the one tool argument that can reach outside the workspace, so
    /// each of these is refused before git gets a chance to interpret it.
    #[test]
    fn escaping_and_reserved_paths_are_refused() {
        for hostile in [
            "/etc/passwd",
            "../outside",
            "a/../../outside",
            ".caos",
            ".caos/conflicts",
        ] {
            assert!(normalize(hostile).is_err(), "accepted path {hostile:?}");
        }
    }

    /// `.caosmeta` and `.caos-secrets` merely start with the reserved name;
    /// only the exact top-level `.caos` entry is refused.
    #[test]
    fn only_the_exact_reserved_entry_is_refused() {
        assert!(normalize("foo.caosmeta").unwrap().is_some());
        assert!(normalize("a/.caos").unwrap().is_some());
    }

    #[test]
    fn a_read_window_pages_by_line() {
        let text = "one\ntwo\nthree\nfour";
        assert_eq!(window(text, &json!({})), text);
        assert_eq!(window(text, &json!({"offset": 2})), "two\nthree\nfour");
        assert_eq!(
            window(text, &json!({"offset": 2, "limit": 2})),
            "two\nthree"
        );
    }

    #[test]
    fn an_oversized_read_says_it_was_truncated() {
        let text = "x".repeat(MAX_READ_BYTES + 10);
        let out = window(&text, &json!({}));
        assert!(out.contains("truncated"), "no truncation note: {out:.80}");
        assert!(out.len() < text.len() + 100);
    }
}
