//! `log`, `show` and `diff` over a source tree's git history.
//!
//! They were built into `llm-step`, which ran them in-process against the
//! conversation's own object store. They are std tools now (`std/log`,
//! `std/show`, `std/diff`): each curries this binary with `--op=<name>`.
//!
//! A source tree is a COMMIT entry in the conversation's tree, and the history is
//! that commit's: the tool takes the tree's path (`source-tree`), finds the
//! entry as stored, and asks git about it. Git history is what the tree alone
//! cannot say, which is why these need a commit and not the `in` tree's content.
//!
//! REVISIONS are a commit hash, `HEAD` (the source tree's own commit), and
//! `~N`/`^` ancestry on either, or (for `diff`) the path of another source tree in the
//! conversation, which names that tree's own commit. Branch and tag names are not available: they
//! resolved from a snapshot of the conversation's refs, which a tool run on its
//! own does not have.
//!
//! A call the model got wrong is a VALUE (a `report` that opens `FAILED`), never
//! a failed worker.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use conversation_protocol::v3::git_store::HistoryQuery;
use conversation_protocol::v3::{GitStore, ObjectStore, Oid};
use worker_common::{arg, caos, cas_hash, cas_kind, path, read_arg, run_worker, scratch};

fn main() -> ExitCode {
    run_worker("history", run)
}

fn run() -> Result<(), String> {
    let op = read_arg("op")?;
    match history(&op) {
        Ok(text) => answer(&text),
        Err(Failure::User(message)) => failed(&message),
        Err(Failure::Infra(error)) => Err(error),
    }
}

enum Failure {
    User(String),
    Infra(String),
}

use Failure::{Infra, User};

fn answer(text: &str) -> Result<(), String> {
    let file = scratch("history-out")?.join("answer");
    fs::write(&file, text).map_err(|e| format!("writing the answer: {e}"))?;
    caos(["put", path(&file), "/cas/out"])
}

fn failed(message: &str) -> Result<(), String> {
    let dir = scratch("history-out")?;
    fs::write(dir.join("report"), format!("FAILED: {message}\n"))
        .map_err(|e| format!("writing the report: {e}"))?;
    caos(["put", path(&dir), "/cas/out"])
}

/// An argument VERBATIM, or `None` if it was not passed.
fn input(name: &str) -> Result<Option<String>, Failure> {
    let file = arg(name);
    if !Path::new(&file).exists() {
        return Ok(None);
    }
    caos(["get", &file]).map_err(Infra)?;
    fs::read_to_string(&file)
        .map(|text| Some(text.trim().to_string()).filter(|text| !text.is_empty()))
        .map_err(|e| User(format!("`{name}` is not UTF-8 text: {e}")))
}

fn split_path(raw: &str) -> Vec<String> {
    raw.trim()
        .trim_start_matches('/')
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_string)
        .collect()
}

/// The commit of the source tree named by the `source-tree` argument.
fn source_commit() -> Result<Oid, Failure> {
    let raw = input("source-tree")?.ok_or_else(|| {
        User(
            "history needs `source-tree`: the path of the source tree, such as feature/01-change"
                .into(),
        )
    })?;
    commit_at(&raw)
}

/// The commit of the source tree at `raw`, a path in the `in` tree: the ENTRY AS
/// STORED, not what resolving it makes of it (that walks through the commit to
/// its code tree).
fn commit_at(raw: &str) -> Result<Oid, Failure> {
    let comps = split_path(raw);
    if comps.is_empty() || comps.iter().any(|c| c == "..") {
        return Err(User(format!("`source-tree` names no source tree: {raw:?}")));
    }
    let tree = cas_hash(&arg("in")).map_err(Infra)?;
    let resolve = |relative: &str, into: &str| -> Result<PathBuf, Failure> {
        let output = std::process::Command::new("caos")
            .args(["resolve", &tree, relative, into])
            .output()
            .map_err(|e| Infra(format!("launching caos resolve: {e}")))?;
        if output.status.success() {
            Ok(PathBuf::from(into))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr.trim_end();
            Err(User(format!(
                "{relative}: {}",
                reason.strip_prefix("caos: ").unwrap_or(reason)
            )))
        }
    };
    // One landing place per lookup: `diff` resolves two paths, and `caos resolve`
    // does not overwrite.
    static LOOKUPS: AtomicUsize = AtomicUsize::new(0);
    let landing = format!("/cas/history-parent-{}", LOOKUPS.fetch_add(1, Ordering::Relaxed));
    let parent = resolve(&comps[..comps.len() - 1].join("/"), &landing)?;
    let entry = parent.join(&comps[comps.len() - 1]);
    if !entry.exists() {
        return Err(User(format!("{} is not in this tree", comps.join("/"))));
    }
    if cas_kind(path(&entry)).map_err(Infra)? != "commit" {
        return Err(User(format!(
            "{} is not a source tree (a code reference); history is a source tree's",
            comps.join("/")
        )));
    }
    let hash = cas_hash(path(&entry)).map_err(Infra)?;
    Oid::parse(&hash, "the source tree's commit").map_err(Infra)
}

/// A revision as git history takes it: `HEAD`/`wc`/`@` or a commit hash, then any
/// `~N`/`^` ancestry.
fn revision(store: &GitStore, head: &Oid, spec: &str) -> Result<Oid, Failure> {
    // A path names another source tree in the conversation (`sources/foo/99-dirty`):
    // its own commit, which may sit on a different line of history than `head`.
    if spec.contains('/') {
        let commit = commit_at(spec)?;
        store.ensure_local(&commit).map_err(Infra)?;
        return Ok(commit);
    }
    let split = spec.find(['~', '^']).unwrap_or(spec.len());
    let (base, suffix) = spec.split_at(split);
    let base = match base {
        "" | "HEAD" | "wc" | "@" => head.clone(),
        hash if hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Oid::parse(hash, "history revision").map_err(User)?
        }
        name => {
            return Err(User(format!(
                "{name:?} is not a revision this tool can resolve: pass a commit hash or HEAD, \
                 optionally with ~N or ^ (branch and tag names are not available here)"
            )))
        }
    };
    store.ancestor_revision(&base, suffix).map_err(User)
}

fn history(op: &str) -> Result<String, Failure> {
    let head = source_commit()?;
    let server = std::env::var("CAOS_SERVER_URL")
        .map_err(|_| Infra("CAOS_SERVER_URL not set".to_string()))?;
    let store = GitStore::scratch("history-git", server.trim_end_matches('/')).map_err(Infra)?;
    store.ensure_local(&head).map_err(Infra)?;
    let at = |name: &str, default: &str| -> Result<Oid, Failure> {
        revision(
            &store,
            &head,
            &input(name)?.unwrap_or_else(|| default.to_string()),
        )
    };
    let scope = input("path")?;
    let (target, query) = match op {
        "log" => {
            let count = match input("count")? {
                Some(text) => text
                    .parse()
                    .map_err(|_| User("count must be a nonnegative integer".to_string()))?,
                None => 20,
            };
            (at("rev", "HEAD")?, HistoryQuery::Log { count })
        }
        "show" => (at("rev", "HEAD")?, HistoryQuery::Show),
        "diff" => {
            let to = at("to", "HEAD")?;
            let from = match input("from")? {
                Some(spec) => revision(&store, &head, &spec)?,
                None => match store
                    .read_commit(&to)
                    .map_err(|e| Infra(String::from(e)))?
                    .parents
                    .first()
                {
                    Some(parent) => parent.clone(),
                    None => return Ok("(no earlier revision: root commit)".to_string()),
                },
            };
            (to, HistoryQuery::Diff { from })
        }
        other => return Err(Infra(format!("unknown history tool {other:?}"))),
    };
    store
        .history(&target, query, scope.as_deref())
        .map_err(User)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_like_the_file_tools_split_them() {
        assert_eq!(split_path("/feature//01-change/"), ["feature", "01-change"]);
        assert!(split_path(" . ").is_empty());
    }
}
