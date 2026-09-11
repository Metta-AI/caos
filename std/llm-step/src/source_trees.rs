//! Repository context derived from the conversation tree.
use super::*;

const CONTENT_GUIDE: &str = r#"
Conversation filesystem:
- File tools, grep, and bash start at the conversation root. Source trees are commit-valued entries that appear as directories. Keep memories, notes, and skills outside code directories. Conversation-root .caos is readable protocol metadata; do not edit it.
- Organize source trees yourself with ordinary file operations. Users describe the desired changes and review structure; do not ask them to prescribe copies, renames, or bookkeeping. Use the simplest layout that fits the task.
- For reviewable changes, preserve the starting commit at <feature>/00-base before editing <feature>/dirty. Keep imports separate from feature work: ask the client to import at imports/<repo>/base. Local paths import current disk content as ordinary files/folders; URLs and explicit revisions import gitlinks. For a PR stack that needs repository ancestry, request an explicit revision, e.g. /import imports/caos/base /path/to/caos HEAD. Do not treat a plain folder as a gitlink. Preserve imports unchanged; copy an imported gitlink to <feature>/00-base and <feature>/dirty with cp -a before editing. Never overwrite an existing base or label an already-edited snapshot as the original base; use the supplied import commit to recover it when necessary.
- Bash needs conversation-relative paths declared for existing content. Use mv and cp -a so commit identity survives. For example, with paths ["imports/caos"]: mkdir -p feature; cp -a imports/caos/base feature/00-base; cp -a imports/caos/base feature/dirty. To load a known commit from CAOS: caos checkout <full-commit> <destination> . . Host paths and repository imports belong to the client.
- Each accepted code edit records a child commit automatically. When a reviewable step is complete, rename dirty to the next NN-description boundary (01 through 99), then cp -a that boundary back to dirty if more work remains. Keep one moving dirty per stack. Directory names label review boundaries; they do not merge histories.
- Delegate bounded tasks with explicit code paths. Children have isolated content and their own transcripts. The parent waits, harvests changes, and checks the result; it owns review-boundary organization unless delegated explicitly. For a PR stack, integrate one coherent change at a time and preserve a boundary after each. Concurrent edits are reconciled on harvest; on conflict, inspect the retained proposal and resolve it instead of overwriting parent work.
- A publishing destination is optional while working. Creating, delegating, merging, testing, and organizing review boundaries do not require .base-url. If the user asks to prepare a PR stack without publishing, finish that work without asking for a destination. Preserve the imported starting commit at 00-base.
- Only when the user wants to publish, set <feature>/.base-url to exactly two lines: repository URL, then external base branch. Read imports/<repo>/base.source.json when present: repository and optional default_branch record the imported repository's origin. Reuse those values unless the user chose a different destination; do not ask the user to repeat them. This is provenance, not publication configuration: write .base-url explicitly. Ask only for missing or ambiguous information at publication time. Incorporate the actual destination base and each preceding boundary, resolve conflicts, and run relevant checks before previewing. The TUI publishes the exact previewed commits.
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
