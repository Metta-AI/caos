//! The built-in git-history tools — `log`, `show`, `diff`. Standard tools
//! shipped with the harness (not project `caos-tools/*.sh`), so every stack
//! that can run tree tools gets them.
//!
//! They are ordinary sub-run tools, launched exactly like a tree tool: the
//! script (this module's embedded shell) rides curried on the std/bash image,
//! with the `#@git` context — the workspace commit `wc` and the ref snapshot
//! `refs` — bound alongside the model's args. From `wc` the shell walks the
//! commit graph by hash and shells out to `diff`; no git binary, no new image.
//! The result is a VALUE (a text report), rendered by the same callback arm as
//! a tree tool's.
//!
//! The scripts are baked into the binary (`include_str!`) rather than published
//! into std: assembling `lib + <command>` and `caos put`ting it at launch keeps
//! the whole feature inside the harness, with nothing to plumb through
//! build-builtins.sh or the chat client.

use serde_json::Value;

use crate::tools::{tree_tool_declaration, TreeArg, TreeTool};

const LIB: &str = include_str!("githist/lib.sh");
const LOG: &str = include_str!("githist/log.sh");
const SHOW: &str = include_str!("githist/show.sh");
const DIFF: &str = include_str!("githist/diff.sh");

/// The built-in tool names, reserved against project shadowing (`tools.rs`).
pub const NAMES: [&str; 3] = ["log", "show", "diff"];

pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
}

/// The worker script for `name`: the shared library followed by the command
/// body, ready to `caos put` and curry as `worker1`.
pub fn script(name: &str) -> Option<String> {
    let body = match name {
        "log" => LOG,
        "show" => SHOW,
        "diff" => DIFF,
        _ => return None,
    };
    Some(format!("{LIB}\n{body}"))
}

fn opt(name: &str, doc: &str) -> TreeArg {
    TreeArg {
        name: name.to_string(),
        doc: doc.to_string(),
        required: false,
    }
}

/// The registry descriptor for `name` — its docs and (all-optional) args. The
/// `git` flag is set so the launch binds `wc`/`refs`, and `tree_tool_args`
/// validates the model's call against these just like a tree tool's.
pub fn tool(name: &str) -> Option<TreeTool> {
    let (doc, args): (&str, Vec<TreeArg>) = match name {
        "log" => (
            "Show the workspace's commit history newest-first (the conversation's turn/step \
             commits and the repo history beneath them): one line per commit with its short \
             hash, date, author and subject. Optionally start from a given revision and/or \
             restrict to commits that changed a path. Reads git history the tree alone can't show.",
            vec![
                opt("rev", "Where to start (default HEAD, the current workspace). A commit hash, a snapshot ref (e.g. main), or HEAD~N / ref^."),
                opt("path", "Only show commits that changed this workspace-relative path."),
                opt("count", "Maximum number of commits to show (default 20)."),
            ],
        ),
        "show" => (
            "Show one commit: its hash, parents, author, full message, and the unified diff it \
             introduced (against its first parent). Optionally scope the diff to a path.",
            vec![
                opt("rev", "The commit to show (default HEAD, the current workspace). A commit hash, a snapshot ref, or HEAD~N / ref^."),
                opt("path", "Restrict the shown diff to this workspace-relative path."),
            ],
        ),
        "diff" => (
            "Unified diff between two revisions of the workspace, optionally scoped to a path. \
             Defaults compare the previous commit to the current workspace (what the latest \
             step changed). `from`/`to` accept a commit hash, a snapshot ref (e.g. main), \
             HEAD/wc, or HEAD~N / ref^.",
            vec![
                opt("from", "The base revision (default HEAD~1, the commit before the workspace)."),
                opt("to", "The revision to compare against the base (default HEAD, the current workspace)."),
                opt("path", "Restrict the diff to this workspace-relative path."),
            ],
        ),
        _ => return None,
    };
    Some(TreeTool {
        name: name.to_string(),
        doc: doc.to_string(),
        args,
        git: true,
    })
}

/// Registry declarations for all three, for the tool registry.
pub fn declarations() -> Vec<Value> {
    NAMES
        .iter()
        .filter_map(|n| tool(n))
        .map(|t| tree_tool_declaration(&t))
        .collect()
}
