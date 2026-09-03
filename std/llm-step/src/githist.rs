//! The built-in git-history tools — `log`, `show`, `diff`. Standard tools
//! shipped with the harness (not project `caos-tools/<name>/`), so every stack
//! that can run tree tools gets them.
//!
//! They are ordinary sub-run tools, launched exactly like a tree tool: the
//! script (this module's embedded shell) rides curried on the std/bash image,
//! with the `@git` context — the workspace commit `wc` and the ref snapshot
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

use crate::tools::{parse_help, tree_tool_declaration, TreeTool};

const LIB: &str = include_str!("githist/lib.sh");
/// The docs, as DATA beside the scripts rather than string literals here.
///
/// `caos cc`'s tool server offers these same three tools and needs the same
/// descriptions; reading one file each is what keeps a tool described one way
/// wherever it is offered, instead of two copies drifting apart. Same format as
/// a std tool's `HELP` here-string, parsed by the same `parse_help`.
const LOG_HELP: &str = include_str!("githist/log.help");
const SHOW_HELP: &str = include_str!("githist/show.help");
const DIFF_HELP: &str = include_str!("githist/diff.help");
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

/// The registry descriptor for `name` — its docs and (all-optional) args, from
/// the help file beside its script. The `git` flag comes from that file's
/// `@git` tag, and the launch binds `wc`/`refs` because of it.
pub fn tool(name: &str) -> Option<TreeTool> {
    let help = match name {
        "log" => LOG_HELP,
        "show" => SHOW_HELP,
        "diff" => DIFF_HELP,
        _ => return None,
    };
    let (doc, args, git) = parse_help(&format!("githist/{name}"), help);
    Some(TreeTool {
        name: name.to_string(),
        doc,
        args,
        git,
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
