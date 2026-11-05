//! The file tools of a session tree: `read`, `ls`, `write`, `edit`, `copy`,
//! `move` and `remove`.
//!
//! They were built into `llm-step`, which registered them with the model and ran
//! them in its own process. They are std tools now: each is an entry of its own
//! (`std/read`, ...) that curries this binary with `--op=<name>`, so each has a
//! path, a `help` and a place in `tool_help`/`run_tool` like any other, and
//! `llm-step` is no longer the only way to reach them.
//!
//! THE INPUT IS `root` (design/tool-contract.md, "`root`"): the object at the
//! path the call named, bound as `/cas/args/root`, and absent when nothing is
//! there. `read`, `write`, `edit` and `remove` work on that object itself;
//! `ls`, `copy` and `move` on a directory or source tree, with `from` and `to`
//! relative to it. Every answer is a tree: `stdout`, what the model reads, and
//! `failed` on a mistake; a writer's adds `root`, the new object for the root
//! path, or `deleted`. The harness puts it back and makes the commits above it.
//!
//! A call the model got wrong is a VALUE, never a failed worker: `failed` and
//! the reason in `stdout`. Only a broken installation (a transport error)
//! fails the job.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};

use worker_common::{
    arg, caos, cas_hash, cas_kind, entries, file_name, link, path, read_arg, run_worker, scratch,
};

/// Output a read returns before it truncates, and the most entries a listing
/// shows. Both exist so one call cannot fill a model's context: `offset`/`limit`
/// and a narrower `path` are how the rest is reached.
const MAX_READ_BYTES: usize = 100_000;
const MAX_ENTRIES: usize = 1_000;

fn main() -> ExitCode {
    run_worker("files", run)
}

/// What went wrong. `User` is the model's mistake and comes back as a value it
/// can correct; `Infra` is the installation's and fails the worker.
enum Fail {
    User(String),
    Infra(String),
}

use Fail::{Infra, User};

fn run() -> Result<(), String> {
    let op = read_arg("op")?;
    let bound = arg("root");
    // Absent when nothing is at the root path: what `write` creates.
    let root = fs::symlink_metadata(&bound).is_ok().then_some(bound);
    let root = root.as_deref();
    match op.as_str() {
        "read" => reader("read", read(root)),
        "ls" => reader("ls", ls(root)),
        "write" => writer("write", write(root)),
        "edit" => writer("edit", edit(root)),
        "copy" => writer("copy", copy(root)),
        "move" => writer("move", move_entry(root)),
        "remove" => writer("remove", remove(root)),
        other => Err(format!("unknown file tool {other:?}")),
    }
}

// ---- results -----------------------------------------------------------------

fn fresh_name(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!("{prefix}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn fresh(prefix: &str) -> String {
    format!("/cas/{}", fresh_name(prefix))
}

fn put(dir: &Path) -> Result<(), String> {
    caos(["put", path(dir), "/cas/out"])
}

/// A failed call's answer: the reason as `stdout`, and the `failed` marker.
fn failed(dir: &Path, op: &str, message: &str) -> Result<(), String> {
    fs::write(dir.join("stdout"), format!("{op}: {message}\n"))
        .map_err(|e| format!("writing stdout: {e}"))?;
    fs::write(dir.join("failed"), format!("{op} failed\n"))
        .map_err(|e| format!("writing failed: {e}"))
}

/// A reader's answer: `{stdout}`, or `{stdout, failed}` saying why.
fn reader(op: &str, outcome: Result<String, Fail>) -> Result<(), String> {
    let dir = scratch("files-out")?;
    match outcome {
        Ok(text) => {
            fs::write(dir.join("stdout"), text).map_err(|e| format!("writing stdout: {e}"))?
        }
        Err(User(message)) => failed(&dir, op, &message)?,
        Err(Infra(error)) => return Err(error),
    }
    put(&dir)
}

/// What a writer puts at its root path.
enum New {
    /// A file with these contents.
    Content(Vec<u8>),
    /// An object already in CAS: the new directory.
    Object(String),
    /// Nothing: the root is removed.
    Deleted,
}

/// A writer's answer: `{stdout, root | deleted}`, or `{stdout, failed}` with
/// nothing to apply.
fn writer(op: &str, outcome: Result<(New, String), Fail>) -> Result<(), String> {
    let dir = scratch("files-out")?;
    match outcome {
        Ok((new, out)) => {
            fs::write(dir.join("stdout"), out).map_err(|e| format!("writing stdout: {e}"))?;
            match new {
                New::Content(bytes) => fs::write(dir.join("root"), bytes)
                    .map_err(|e| format!("writing root: {e}"))?,
                New::Object(object) => link(&object, dir.join("root"))?,
                New::Deleted => fs::write(dir.join("deleted"), format!("{op}\n"))
                    .map_err(|e| format!("writing deleted: {e}"))?,
            }
        }
        Err(User(message)) => failed(&dir, op, &message)?,
        Err(Infra(error)) => return Err(error),
    }
    put(&dir)
}

// ---- arguments ---------------------------------------------------------------

/// An argument VERBATIM, or `None` if it was not passed. Not trimmed, unlike
/// `read_arg`: the text of an edit is exactly what it says, whitespace included.
fn input(name: &str) -> Result<Option<String>, Fail> {
    let file = arg(name);
    if !Path::new(&file).exists() {
        return Ok(None);
    }
    caos(["get", &file]).map_err(Infra)?;
    fs::read_to_string(&file)
        .map(Some)
        .map_err(|e| User(format!("`{name}` is not UTF-8 text: {e}")))
}

fn num(name: &str) -> Result<Option<u64>, Fail> {
    Ok(input(name)?.and_then(|text| text.trim().parse().ok()))
}

fn flag(name: &str) -> Result<bool, Fail> {
    Ok(input(name)?.is_some_and(|text| matches!(text.trim(), "true" | "1" | "yes")))
}

/// A path argument's components: a leading `/` is tolerated, and empty and `.`
/// segments drop out. Whether an empty result is an error is the CALLER's
/// question.
fn split_path(raw: &str) -> Vec<String> {
    raw.trim()
        .trim_start_matches('/')
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_string)
        .collect()
}

fn reject_parent(comps: &[String]) -> Result<(), Fail> {
    if comps.iter().any(|c| c == "..") {
        return Err(User("`..` is not allowed in tree paths".to_string()));
    }
    Ok(())
}

/// A REQUIRED path.
fn components(name: &str) -> Result<Vec<String>, Fail> {
    let raw = input(name)?.ok_or_else(|| User(format!("missing `{name}`")))?;
    let comps = split_path(&raw);
    if comps.is_empty() {
        return Err(User(format!("`{name}` names no path: {raw:?}")));
    }
    reject_parent(&comps)?;
    Ok(comps)
}

/// `.caos` at the root is protocol metadata: a model changes it through the
/// session's own commands, not by writing files.
fn reject_protocol(comps: &[String]) -> Result<(), Fail> {
    if comps.first().is_some_and(|c| c == ".caos") {
        return Err(User(
            ".caos is protocol metadata; use session commands to change it".to_string(),
        ));
    }
    Ok(())
}

// ---- lookup ------------------------------------------------------------------

/// Resolve `comps` within the input to a node, traversing code references. A
/// directory it names comes back as a directory, and a source tree (a commit
/// entry) as its code tree.
fn resolve(ws: &str, comps: &[String]) -> Result<PathBuf, Fail> {
    let hash = cas_hash(ws).map_err(Infra)?;
    let destination = fresh("resolved");
    let relative = comps.join("/");
    let output = std::process::Command::new("caos")
        .args(["resolve", &hash, &relative, &destination])
        .output()
        .map_err(|e| Infra(format!("launching caos resolve: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(User(resolve_failure(&relative, stderr.trim_end())));
    }
    Ok(PathBuf::from(destination))
}

/// What a failed lookup tells the model. A path missing from the tree is most
/// often one that only EVALUATION produces (a mount such as `caos-std/`), and the
/// raw exit status said none of that. The lookup stays a lookup: it names what
/// evaluates rather than evaluating, since that can build.
fn resolve_failure(relative: &str, stderr: &str) -> String {
    if stderr.contains("no such path") {
        format!(
            "{relative} is not in this tree. Check the path first: `ls` its parent. \
             A mount such as caos-std/ exists only in the EVALUATED tree: evaluate its path \
             and pass `<hash>/<path>` as `root`."
        )
    } else {
        let reason = stderr.strip_prefix("caos: ").unwrap_or(stderr);
        format!("resolving {relative}: {reason}")
    }
}

// ---- the readers -------------------------------------------------------------

/// The root, or the model's mistake when nothing is there.
fn present(root: Option<&str>) -> Result<&str, Fail> {
    root.ok_or_else(|| {
        User(
            "nothing is at the root path. Check it with ls on the directory above it"
                .to_string(),
        )
    })
}

/// The root as a directory: a tree as it is, a source tree as its code tree.
fn directory(root: Option<&str>, otherwise: &str) -> Result<PathBuf, Fail> {
    let dir = resolve(present(root)?, &[])?;
    if !dir.is_dir() {
        return Err(User(format!("the root is a file; {otherwise}")));
    }
    Ok(dir)
}

/// The root as a file's bytes.
fn file(root: Option<&str>, otherwise: &str) -> Result<Vec<u8>, Fail> {
    let root = present(root)?;
    if cas_kind(root).map_err(Infra)? != "blob" {
        return Err(User(format!("the root is a directory; {otherwise}")));
    }
    caos(["get", root]).map_err(Infra)?;
    fs::read(root).map_err(|e| Infra(format!("reading the root: {e}")))
}

fn read(root: Option<&str>) -> Result<String, Fail> {
    bounded(&file(root, "use ls, or name a file in it")?)
}

/// Apply `read`'s bounds to raw bytes: a line window when `offset`/`limit` is
/// set, else a head-truncation at [`MAX_READ_BYTES`].
fn bounded(bytes: &[u8]) -> Result<String, Fail> {
    let total = bytes.len();
    let text = String::from_utf8_lossy(bytes);
    let offset = num("offset")?.map(|n| n.max(1) as usize);
    let limit = num("limit")?.map(|n| n as usize);
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

fn ls(root: Option<&str>) -> Result<String, Fail> {
    let dir = directory(root, "use read")?;
    let children = entries(path(&dir)).map_err(Infra)?;
    let mut lines = Vec::with_capacity(children.len());
    for child in &children {
        let name = file_name(child);
        // A source tree is a commit entry: it shows as a directory, as it does
        // to anything that walks into it.
        let directory = child.is_dir()
            || (!child.is_symlink() && cas_kind(path(child)).map_err(Infra)? == "commit");
        lines.push(if directory { format!("{name}/") } else { name });
    }
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

// ---- the writers -------------------------------------------------------------
//
// Each returns what goes at the root path and what the model reads. The
// harness writes the commit message (the tool and the root path) unless a tool
// has something better to say.

type Written = (New, String);

fn write(root: Option<&str>) -> Result<Written, Fail> {
    let content =
        input("content")?.ok_or_else(|| User("write needs a string `content`".to_string()))?;
    if let Some(root) = root {
        if cas_kind(root).map_err(Infra)? != "blob" {
            return Err(User(
                "the root is a directory; write replaces one file".to_string(),
            ));
        }
    }
    let out = format!("wrote {} bytes", content.len());
    Ok((New::Content(content.into_bytes()), out))
}

fn edit(root: Option<&str>) -> Result<Written, Fail> {
    let old = input("old-string")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| User("edit needs a non-empty `old-string`".to_string()))?;
    let new =
        input("new-string")?.ok_or_else(|| User("edit needs a string `new-string`".to_string()))?;
    let replace_all = flag("replace-all")?;

    let bytes = file(root, "edit changes one file")?;
    let text = String::from_utf8(bytes)
        .map_err(|_| User("the root is not valid UTF-8; edit only text files".to_string()))?;

    let count = text.matches(old.as_str()).count();
    let replaced =
        match (count, replace_all) {
            (0, _) => return Err(User(
                "old-string not found in the file (it must match exactly, including whitespace)"
                    .to_string(),
            )),
            (n, false) if n > 1 => {
                return Err(User(format!(
                    "old-string appears {n} times; include more surrounding context to make it \
                 unique, or set replace-all"
                )))
            }
            (_, true) => text.replace(old.as_str(), &new),
            (_, false) => text.replacen(old.as_str(), &new, 1),
        };
    let n = if replace_all { count } else { 1 };
    let out = format!("edited ({n} replacement{})", if n == 1 { "" } else { "s" });
    Ok((New::Content(replaced.into_bytes()), out))
}

fn copy(root: Option<&str>) -> Result<Written, Fail> {
    let dir = directory(root, "copy works inside the directory that holds both paths")?;
    let from = components("from")?;
    let to = components("to")?;
    reject_protocol(&to)?;
    let new = copy_entry(path(&dir), &from, &to)?;
    let out = format!("copied {} to {}", from.join("/"), to.join("/"));
    Ok((New::Object(new), out))
}

fn move_entry(root: Option<&str>) -> Result<Written, Fail> {
    let dir = directory(root, "move works inside the directory that holds both paths")?;
    let from = components("from")?;
    let to = components("to")?;
    reject_protocol(&from)?;
    reject_protocol(&to)?;
    if from == to {
        return Err(User(format!(
            "`from` and `to` are the same path: {}",
            from.join("/")
        )));
    }
    // A copy of a directory into itself is a fine thing (it copies what was
    // there), but a MOVE into itself would delete the only copy.
    if to.len() > from.len() && to[..from.len()] == from[..] {
        return Err(User(format!(
            "cannot move {} into itself ({})",
            from.join("/"),
            to.join("/")
        )));
    }
    let copied = copy_entry(path(&dir), &from, &to)?;
    let new = remove_entry(&copied, &from)?;
    let out = format!("moved {} to {}", from.join("/"), to.join("/"));
    Ok((New::Object(new), out))
}

fn remove(root: Option<&str>) -> Result<Written, Fail> {
    present(root)?;
    Ok((New::Deleted, "removed".to_string()))
}

fn refuse_symlinks(work: &Path, ancestors: &[String], what: &str) -> Result<(), Fail> {
    let mut at = work.to_path_buf();
    for component in ancestors {
        at.push(component);
        if at.is_symlink() {
            return Err(User(format!(
                "{what} cannot follow a symlink; use bash to replace it explicitly"
            )));
        }
    }
    Ok(())
}

/// Put the entry at `from` at `to`, creating missing parent directories, and
/// refusing an existing `to`.
///
/// THE ENTRY IS LINKED, NOT READ. `from` resolves to an object in /cas and the
/// scratch tree gets a link to it, which `caos put` records by hash, so nothing
/// under `from` is fetched however large it is, and a source tree (a commit)
/// arrives as the same commit: there is no "preserve history" switch because
/// there is no copy of bytes to lose it in.
fn copy_entry(ws: &str, from: &[String], to: &[String]) -> Result<String, Fail> {
    // THE ENTRY ITSELF, NOT WHAT `resolve` MAKES OF IT. Resolving `from` walks
    // THROUGH a source tree (a commit entry) to its code tree, and a link to that
    // would copy the files but not the commit: the copy would not be a source
    // tree. The child of the parent directory is the entry as stored.
    resolve(ws, from)?; // a path that is not there is the model's mistake
    let parent = resolve(ws, &from[..from.len() - 1])?;
    let source = parent.join(&from[from.len() - 1]);
    match resolve(ws, to) {
        Ok(_) => {
            return Err(User(format!(
                "{} already exists; remove it first or choose another destination",
                to.join("/")
            )))
        }
        Err(Infra(e)) => return Err(Infra(e)),
        Err(User(_)) => {} // not there: the case this wants
    }
    let work = scratch(&fresh_name("files")).map_err(Infra)?;
    let relative = to.join("/");
    worker_common::files::materialize(ws, &work, std::slice::from_ref(&relative)).map_err(Infra)?;
    refuse_symlinks(&work, &to[..to.len() - 1], "copy/move")?;
    let target = work.join(&relative);
    fs::create_dir_all(target.parent().unwrap())
        .map_err(|e| User(format!("cannot create the parents of {relative}: {e}")))?;
    link(&source, target).map_err(Infra)?;
    let out = fresh("files-out");
    caos(["put", path(&work), &out]).map_err(Infra)?;
    Ok(out)
}

/// Drop the entry at `comps`, and everything under it, from the tree.
///
/// The way TO the entry is materialized and the entry itself is not: asking for a
/// name that cannot exist beside it makes every directory above it real (so a
/// source tree boundary is restored by `caos put`) while the entry stays a link,
/// which is then removed without ever having been fetched.
fn remove_entry(ws: &str, comps: &[String]) -> Result<String, Fail> {
    resolve(ws, comps)?; // a path that is not there is the model's mistake, not a no-op
    let work = scratch(&fresh_name("files")).map_err(Infra)?;
    let mut probe: Vec<String> = comps[..comps.len() - 1].to_vec();
    probe.push(".caos-remove-probe".to_string());
    worker_common::files::materialize(ws, &work, &[probe.join("/")]).map_err(Infra)?;
    refuse_symlinks(&work, &comps[..comps.len() - 1], "remove")?;
    let target = work.join(comps.join("/"));
    let is_dir = fs::symlink_metadata(&target)
        .map_err(|e| User(format!("{} cannot be removed: {e}", comps.join("/"))))?
        .is_dir();
    let removal = if is_dir {
        fs::remove_dir_all(&target)
    } else {
        fs::remove_file(&target)
    };
    removal.map_err(|e| Infra(format!("removing {}: {e}", target.display())))?;
    let out = fresh("files-out");
    caos(["put", path(&work), &out]).map_err(Infra)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_the_way_the_inline_tools_split_them() {
        assert_eq!(split_path("/a//b/./c"), ["a", "b", "c"]);
        assert!(split_path(" . ").is_empty());
        assert!(split_path("/").is_empty());
    }

    #[test]
    fn a_parent_component_is_refused() {
        assert!(reject_parent(&["a".into(), "..".into()]).is_err());
        assert!(reject_parent(&["a".into(), "b".into()]).is_ok());
    }

    #[test]
    fn the_protocol_directory_is_not_writable() {
        assert!(reject_protocol(&[".caos".into(), "transcript".into()]).is_err());
        assert!(reject_protocol(&["a".into(), ".caos".into()]).is_ok());
    }

    #[test]
    fn a_missing_path_names_the_way_to_a_mount() {
        let message = resolve_failure(
            "caos-std/README.md",
            "caos: no such path: caos-std/README.md",
        );
        assert!(message.starts_with("caos-std/README.md is not in this tree"));
        assert!(message.contains("EVALUATED tree"), "{message}");
        let other = resolve_failure("a", "caos: \"a\" traverses a non-directory");
        assert_eq!(other, "resolving a: \"a\" traverses a non-directory");
    }
}
