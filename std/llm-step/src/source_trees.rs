//! Repository context derived from the conversation tree.
use super::*;

const CONTENT_GUIDE: &str = r#"
Conversation filesystem:
- File tools, grep, and bash start at the conversation root. Source trees are commit-valued entries that appear as directories. Keep memories, notes, and skills outside code directories. Conversation-root .caos is readable protocol metadata; do not edit it.
- Organize source trees yourself with ordinary file operations. Users describe the desired changes and review structure; do not ask them to prescribe copies, renames, or bookkeeping. Use the simplest layout that fits the task.
- For reviewable changes, preserve the starting commit at <feature>/00-base before editing <feature>/dirty. If an imported entry is elsewhere, move it into that layout. Never overwrite an existing base or label an already-edited snapshot as the original base; use the supplied import commit to recover it when necessary.
- Bash needs conversation-relative paths declared for existing content. Use mv and cp -a so commit identity survives. For example, with paths ["feature"]: cp -a feature/dirty feature/00-base. To load a known commit from CAOS: caos checkout <full-commit> <destination> . . Host paths and repository imports belong to the client.
- Each accepted code edit records a child commit automatically. When a reviewable step is complete, rename dirty to the next NN-description boundary (01 through 99), then cp -a that boundary back to dirty if more work remains. Keep one moving dirty per stack. Directory names label review boundaries; they do not merge histories.
- Delegate bounded tasks with explicit code paths. Children have isolated content and their own transcripts. The parent waits, harvests changes, and checks the result; it owns review-boundary organization unless delegated explicitly. For a PR stack, integrate one coherent change at a time and preserve a boundary after each. Concurrent edits are reconciled on harvest; on conflict, inspect the retained proposal and resolve it instead of overwriting parent work.
- Before publication, write <feature>/.base-url as exactly two lines: repository URL, then external base branch. Use supplied repository/revision context or existing metadata; ask only if the intended destination or base remains ambiguous. Incorporate the actual base and each preceding boundary, resolve conflicts, and run relevant checks. The TUI previews and publishes those exact commits; it does not prepare them for you.
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
