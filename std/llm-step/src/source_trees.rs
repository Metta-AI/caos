//! Repository context derived from the conversation tree.
use super::*;

const CONTENT_GUIDE: &str = r#"
Conversation filesystem:
- File tools, grep, and bash start at the conversation root. Source trees are commit-valued entries that appear as directories. Keep memories, notes, and skills outside code directories. Conversation-root .caos is readable protocol metadata; do not edit it.
- Organize source trees yourself with ordinary file operations. Users describe the desired changes and review structure; do not ask them to prescribe copies, renames, or bookkeeping. Use the simplest layout that fits the task.
- For reviewable changes, preserve the starting commit at <feature>/00-base before editing <feature>/01-change. Keep imports separate from feature work: ask the client to import at imports/<repo>/base. Local imports snapshot a Git checkout root as a gitlink: unchanged content reuses HEAD, disk changes create a child commit. URLs and explicit revisions import the requested commit. Plain directories and individual files cannot be imported. No publishing destination is needed to import or start work. Preserve imports unchanged; copy an imported gitlink to <feature>/00-base and <feature>/01-change with cp -a before editing. Never overwrite an existing base or label an already-edited snapshot as the original base; use the supplied import commit to recover it when necessary.
- Bash needs conversation-relative paths declared for existing content. Use mv and cp -a so commit identity survives. For example, with paths ["imports/caos"]: mkdir -p feature; cp -a imports/caos/base feature/00-base; cp -a imports/caos/base feature/01-change. To reference a known commit from CAOS, use caos get-hash <full-commit> /cas/source, then ln -s /cas/source <destination>; the next tool call exposes that gitlink as a directory. Host paths and repository imports belong to the client.
- Each accepted code edit records a child commit automatically. Work directly in a named boundary such as 01-parser. To start the next PR, cp -a that entry to 02-errors and edit the copy, preserving the earlier boundary. Sibling gitlinks sort by filename: the first is the base and each later entry is a review boundary. Numbering is a naming convention, not a required schema. No entry has a special working or sealed state. Directory order does not merge histories.
- Delegate bounded tasks with explicit code paths. Children have isolated content and their own transcripts. The parent waits, harvests changes, and checks the result; it owns review-boundary organization unless delegated explicitly. For a PR stack, integrate one coherent change at a time and preserve a boundary after each. Concurrent edits are reconciled on harvest; on conflict, inspect the retained proposal and resolve it instead of overwriting parent work.
- Preparing, delegating, merging, testing, and organizing a PR stack requires no publishing destination. Preserve the imported starting commit at 00-base. Finish the requested work without asking for repository publication settings.
- Publication is an explicit client operation. The TUI asks the user to confirm the repository and external base branch, with optional import provenance as a suggestion. Do not create publication policy files. Incorporate the chosen destination base and each preceding boundary, resolve conflicts, and run relevant checks before publication. The TUI pushes the exact previewed commits; it does not edit or test code.
- Git tools require the target commit-entry path explicitly. Invoke repository tools with run_tool at a conversation-relative path. UI selection never changes your execution context.
"#;

pub(super) fn context(view: &Conversation<'_>) -> Result<String, String> {
    let entries = view
        .source_trees()?
        .into_iter()
        .map(|(path, entry)| json!({"path":path,"commit":entry.commit}))
        .collect::<Vec<_>>();
    Ok(format!(
        "\n\n{CONTENT_GUIDE}\nCurrent commit entries: {}",
        serde_json::to_string(&entries).map_err(|e| e.to_string())?
    ))
}

pub(super) fn repository_context(
    view: &Conversation<'_>,
    roots: &[String],
) -> Result<String, String> {
    let mut context = String::new();
    for ((name, _), root) in view.source_trees()?.iter().zip(roots) {
        let rules = Path::new(root).join("AGENTS.md");
        if rules.exists() {
            caos(["get", path(&rules)])?;
            let text =
                fs::read_to_string(&rules).map_err(|e| format!("reading {name}/AGENTS.md: {e}"))?;
            let bounded: String = text.chars().take(64_000).collect();
            context.push_str(&format!("

Repository instructions for source tree {name:?} only (follow applicable nested AGENTS.md when editing):
{bounded}"));
        }
        let tools = tools::tree_tools(root)?
            .iter()
            .map(tools::tree_tool_declaration)
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            context.push_str(&format!(
                "

Repository tools for source tree {name:?}; invoke using run_tool with path {name}/caos-tools/<tool-name>: {}",
                serde_json::to_string(&tools).map_err(|e| e.to_string())?
            ));
        }
    }
    Ok(context)
}
