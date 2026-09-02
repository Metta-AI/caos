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

use std::path::{Path, PathBuf};

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

/// What a tool produced: text for the model, and — for a mutation — the
/// workspace tree that replaces the one it was given.
pub struct Outcome {
    pub text: String,
    pub tree: Option<String>,
}

impl Outcome {
    fn read(text: String) -> Outcome {
        Outcome { text, tree: None }
    }

    fn wrote(text: String, tree: String) -> Outcome {
        Outcome {
            text,
            tree: Some(tree),
        }
    }
}

/// Run one tool against `tree`. `args` is the tool's arguments exactly as the
/// model supplied them.
pub fn execute(
    t: &GitTransport,
    tree: &str,
    name: &str,
    args: &Value,
) -> Result<Outcome, ToolError> {
    match name {
        "read" => read(t, tree, args),
        "ls" => ls(t, tree, args),
        "write" => write(t, tree, args),
        "edit" => edit(t, tree, args),
        "grep" => grep(t, tree, args),
        other => Err(User(format!("unknown tool {other:?}"))),
    }
}

fn read(t: &GitTransport, tree: &str, args: &Value) -> Result<Outcome, ToolError> {
    let path = path_arg(args, "file_path")?;
    let (kind, bytes) = object_at(t, tree, &path)?;
    if kind != "blob" {
        return Err(User(format!("{path} is a directory; use ls")));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| User(format!("{path} is not valid UTF-8; it is a binary file")))?;
    Ok(Outcome::read(window(&text, args)))
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

fn ls(t: &GitTransport, tree: &str, args: &Value) -> Result<Outcome, ToolError> {
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
    Ok(Outcome::read(match entries.is_empty() {
        true => "(empty directory)".to_string(),
        false => entries.join("\n"),
    }))
}

fn write(t: &GitTransport, tree: &str, args: &Value) -> Result<Outcome, ToolError> {
    let path = path_arg(args, "file_path")?;
    let content = string_arg(args, "content")?;
    let tree = put_file(t, tree, &path, content.as_bytes())?;
    Ok(Outcome::wrote(
        format!("wrote {} bytes to {path}", content.len()),
        tree,
    ))
}

fn edit(t: &GitTransport, tree: &str, args: &Value) -> Result<Outcome, ToolError> {
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
    Ok(Outcome::wrote(format!("replaced {times} in {path}"), tree))
}

/// Search the workspace by running the `grep` std tool.
///
/// This is the DISPATCHED tool path: unlike read/ls/write/edit it does not
/// compute an answer locally, it builds an ArgTree and runs it. Nothing here is
/// grep-specific below the argument names — `std/rgrep-tool` resolves the
/// scope, drives the `std/rgrep` fold, and renders the result itself, returning
/// the ordinary `{report}` tree. That is what lets `bash` and every
/// `caos-tools/<name>` entry reuse `run_std_tool` unchanged.
fn grep(t: &GitTransport, tree: &str, args: &Value) -> Result<Outcome, ToolError> {
    let pattern = string_arg(args, "pattern")?;
    let mut kvs = vec![format!("--pattern={pattern}")];
    if let Some(path) = optional_path_arg(args, "path")? {
        kvs.push(format!("--path={path}"));
    }
    run_std_tool(t, tree, "std/rgrep-tool", &kvs).map(Outcome::read)
}

/// Run a std tool over `tree` and return its report.
///
/// The tool's own image carries what it needs (rgrep-tool binds the fold it
/// drives), so a caller supplies only the workspace and the model's arguments.
fn run_std_tool(
    t: &GitTransport,
    tree: &str,
    entry: &str,
    kvs: &[String],
) -> Result<String, ToolError> {
    let image = std_tool_image(t, entry)?;
    let curried = caos::curry_client_object(t, &image, kvs)
        .map_err(ToolError::infra)?
        .to_string();
    let (kind, result) =
        run_client_request_with_store(t, &curried, &[format!("--in:hash={tree}")], &[])
            .map_err(ToolError::infra)?;
    read_report(t, &result, &kind)
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
    // Named by PATH, not `DEEP-DEPS/<name>`: this repo has no root `DEPS`, so
    // `DEEP-DEPS` does not exist at its root. The walk still starts at the tree
    // root, so the root expression deepens the tree before the descent reaches
    // the entry and its own mounts.
    let image = eval_workspace_path(t, entry, &[]).map_err(Infra)?;
    cache.push((entry.to_string(), image.clone()));
    Ok(image)
}

/// Render a tool result the way every caos caller does: a tree carrying a
/// `report` blob IS that report, a tree without one is named by its entries, and
/// a blob result is its own text. `llm-step`'s `tree_tool_result_block` and
/// `run-tool`'s `report_conventions` apply the same three rules, and a report
/// containing `FAILED` is the tool telling the model it got the call wrong.
///
/// `cli_get` is what fetches it, NOT `fetch_and_materialize`: the latter is the
/// worker's CAS form and writes hash-tagged placeholders for a later `caos get`
/// to fill, so on the host every file arrives zero-length and a correct result
/// reads as an empty one.
fn read_report(t: &GitTransport, result: &str, kind: &str) -> Result<String, ToolError> {
    let target = scratch_path(t, "tool")?;
    let path = target
        .to_str()
        .ok_or_else(|| Infra(format!("scratch path is not UTF-8: {}", target.display())))?;
    cli_get(t, result, path).map_err(ToolError::infra)?;
    let text = report_text(&target, kind);
    // Only the checkout is temporary; the objects it fetched stay in the repo.
    let _ = std::fs::remove_dir_all(&target);
    let _ = std::fs::remove_file(&target);
    let text = text?;
    match text.contains("FAILED") {
        true => Err(User(text)),
        false => Ok(text),
    }
}

fn report_text(target: &Path, kind: &str) -> Result<String, ToolError> {
    if kind != "tree" && !target.is_dir() {
        return std::fs::read_to_string(target)
            .map(|text| text.trim_end().to_string())
            .map_err(|error| Infra(format!("reading tool result: {error}")));
    }
    let report = target.join("report");
    if report.is_file() {
        return std::fs::read_to_string(&report)
            .map(|text| text.trim_end().to_string())
            .map_err(|error| Infra(format!("reading tool report: {error}")));
    }
    let mut names: Vec<String> = std::fs::read_dir(target)
        .map_err(|error| Infra(format!("reading tool result: {error}")))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    Ok(format!("result tree: {}", names.join(" ")))
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
