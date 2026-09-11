//! Read-only history tools over the existing Git store.

use conversation_protocol::v3::git_store::HistoryQuery;
use conversation_protocol::v3::{GitStore, ObjectStore, Oid};
use serde_json::Value;

use crate::tools::{builtin_tool, tree_tool_declaration, TreeTool};

const LOG_HELP: &str = "Show the source tree's first-parent commit history newest-first: one line per commit with its short hash, date, author and subject. Optionally start from a given revision and/or restrict to commits that changed a path. Reads git history the tree alone can't show.
@param [rev] Where to start (default HEAD, the current source tree). A commit hash, a snapshot ref (e.g. main), or HEAD~N / ref^.
@param [path] Only show commits that changed this source-tree-relative path.
@param [count] Maximum number of commits to show (default 20).
@git";
const SHOW_HELP: &str = "Show one commit: its hash, parents, author, full message, and the unified diff it introduced (against its first parent). Optionally scope the diff to a path.
@param [rev] The commit to show (default HEAD, the current source tree). A commit hash, a snapshot ref, or HEAD~N / ref^.
@param [path] Restrict the shown diff to this source-tree-relative path.
@git";
const DIFF_HELP: &str = "Unified diff between two revisions of the source tree, optionally scoped to a path. Defaults compare the previous commit to the current source tree (what the latest step changed). `from`/`to` accept a commit hash, a snapshot ref (e.g. main), HEAD/wc, or HEAD~N / ref^.
@param [from] The base revision (default HEAD~1, the commit before the source tree).
@param [to] The revision to compare against the base (default HEAD, the current source tree).
@param [path] Restrict the diff to this source-tree-relative path.
@git";

/// The built-in tool names, reserved against project shadowing (`tools.rs`).
pub const NAMES: [&str; 3] = ["log", "show", "diff"];

pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
}

pub fn execute(
    store: &GitStore,
    refs: Option<&str>,
    head: &Oid,
    call: &Value,
    name: &str,
) -> Value {
    let id = call["id"].as_str().unwrap_or("");
    let Some(tool) = tool(name) else {
        return crate::error_block(id, "unknown history tool");
    };
    let bound = match crate::tools::tree_tool_args(call, &tool) {
        Ok(bound) => bound,
        Err(block) => return block,
    };
    let run = || -> Result<String, String> {
        let get = |key: &str| {
            bound
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let resolve = |spec: &str| -> Result<Oid, String> {
            let split = spec.find(['~', '^']).unwrap_or(spec.len());
            let (base, suffix) = spec.split_at(split);
            let base = match base {
                "" | "HEAD" | "wc" | "@" => head.clone(),
                _ => Oid::parse(&crate::lookup_theirs(refs, Some(base))?, "history revision")?,
            };
            store.ancestor_revision(&base, suffix)
        };
        let revision = resolve(get(if name == "diff" { "to" } else { "rev" }).unwrap_or("HEAD"))?;
        let query = match name {
            "log" => HistoryQuery::Log {
                count: get("count")
                    .unwrap_or("20")
                    .parse()
                    .map_err(|_| "count must be a nonnegative integer")?,
            },
            "show" => HistoryQuery::Show,
            "diff" => {
                let from = match get("from") {
                    Some(from) => resolve(from)?,
                    None => match store
                        .read_commit(&revision)
                        .map_err(String::from)?
                        .parents
                        .first()
                    {
                        Some(parent) => parent.clone(),
                        None => return Ok("(no earlier revision: root commit)".into()),
                    },
                };
                HistoryQuery::Diff { from }
            }
            _ => unreachable!(),
        };
        store.history(&revision, query, get("path"))
    };
    match run() {
        Ok(text) => crate::result_block(id, &text, false),
        Err(error) => crate::error_block(id, &error),
    }
}

/// Descriptors and validation shared with repository tools.
pub fn tool(name: &str) -> Option<TreeTool> {
    let help = match name {
        "log" => LOG_HELP,
        "show" => SHOW_HELP,
        "diff" => DIFF_HELP,
        _ => return None,
    };
    Some(builtin_tool(name, help))
}

/// Registry declarations for all three, for the tool registry.
pub fn declarations() -> Vec<Value> {
    NAMES
        .iter()
        .filter_map(|n| tool(n))
        .map(|t| tree_tool_declaration(&t))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parsed_declarations_are_byte_identical() {
        let expected = json!([
            {
                "name": "log",
                "description": "Show the source tree's first-parent commit history newest-first: one line per commit with its short hash, date, author and subject. Optionally start from a given revision and/or restrict to commits that changed a path. Reads git history the tree alone can't show.",
                "input_schema": {"type": "object", "properties": {
                    "rev": {"type": "string", "description": "Where to start (default HEAD, the current source tree). A commit hash, a snapshot ref (e.g. main), or HEAD~N / ref^."},
                    "path": {"type": "string", "description": "Only show commits that changed this source-tree-relative path."},
                    "count": {"type": "string", "description": "Maximum number of commits to show (default 20)."}
                }}
            },
            {
                "name": "show",
                "description": "Show one commit: its hash, parents, author, full message, and the unified diff it introduced (against its first parent). Optionally scope the diff to a path.",
                "input_schema": {"type": "object", "properties": {
                    "rev": {"type": "string", "description": "The commit to show (default HEAD, the current source tree). A commit hash, a snapshot ref, or HEAD~N / ref^."},
                    "path": {"type": "string", "description": "Restrict the shown diff to this source-tree-relative path."}
                }}
            },
            {
                "name": "diff",
                "description": "Unified diff between two revisions of the source tree, optionally scoped to a path. Defaults compare the previous commit to the current source tree (what the latest step changed). `from`/`to` accept a commit hash, a snapshot ref (e.g. main), HEAD/wc, or HEAD~N / ref^.",
                "input_schema": {"type": "object", "properties": {
                    "from": {"type": "string", "description": "The base revision (default HEAD~1, the commit before the source tree)."},
                    "to": {"type": "string", "description": "The revision to compare against the base (default HEAD, the current source tree)."},
                    "path": {"type": "string", "description": "Restrict the diff to this source-tree-relative path."}
                }}
            }
        ]);
        assert_eq!(
            serde_json::to_vec(&declarations()).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
}
