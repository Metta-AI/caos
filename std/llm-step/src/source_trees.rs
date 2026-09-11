//! Repository context derived from the conversation tree.
use super::*;

const CONTENT_GUIDE: &str = r#"
Conversation filesystem:
- File tools, grep, and bash start at the conversation root. Source trees are commit-valued entries that appear as directories. Keep memories, notes, and skills outside code directories. Conversation-root .caos is readable protocol metadata; do not edit it.
- When the user authorizes a small improvement without prescribing the exact edit, choose a clear, low-risk change and proceed. Ask only when missing information prevents useful work; do not ask the user to choose a typo you have already identified.
- Organize source trees yourself with ordinary file operations. Users describe the desired changes and review structure; do not ask them to prescribe copies, renames, or bookkeeping. Use the simplest layout that fits the task.
- For reviewable changes, preserve the starting commit at <feature>/00-base before editing <feature>/01-change. Keep imports separate from feature work at imports/<repo>/base. Local imports snapshot a Git checkout root as a gitlink: unchanged content reuses HEAD, disk changes create a child commit. URLs and explicit revisions import the requested commit. Plain directories and individual files cannot be imported. No publishing destination is needed to import or start work. Preserve imports unchanged; copy an imported gitlink to <feature>/00-base and <feature>/01-change with cp -a before editing. Never overwrite an existing base or label an already-edited snapshot as the original base; use the supplied import commit to recover it when necessary.
- Bash needs conversation-relative paths declared for existing content. Use mv and cp -a so commit identity survives. For example, with paths ["imports/caos"]: mkdir -p feature; cp -a imports/caos/base feature/00-base; cp -a imports/caos/base feature/01-change. To reference a known commit from CAOS, use caos get-hash <full-commit> /cas/source, then ln -s /cas/source <destination>; the next tool call exposes that gitlink as a directory. Host paths and repository imports belong to the client.
- Each accepted code edit, including a bash deletion, records a child commit automatically. There is no separate staging or conflict-resolution registration step. Work directly in a named boundary such as 01-parser. To start the next PR, cp -a that entry to 02-errors and edit the copy, preserving the earlier boundary. Sibling gitlinks sort by filename: the first is the base and each later entry is a review boundary. Numbering is a naming convention, not a required schema. No entry has a special working or sealed state. Directory order does not merge histories.
- Delegate bounded tasks with explicit code paths. Children have isolated content and their own transcripts. The parent waits, harvests changes, and checks the result; it owns review-boundary organization unless delegated explicitly. For a PR stack, integrate one coherent change at a time and preserve a boundary after each. Concurrent edits are reconciled on harvest; on conflict, inspect the retained proposal and resolve it instead of overwriting parent work.
- Before integrating a publication base, compare the intended feature change with the full source-versus-destination difference. Merging upstream preserves all existing branch changes; rebasing a whole branch may replay inherited changes too. Transplanting only the requested edit onto a new base is a separate operation. If a small task would publish unrelated inherited work, explain that scope and ask whether to retain the branch or transplant the edit. Do not claim that targeting an older upstream isolates the edit. Preserve existing snapshots when making a new starting point.
- Resolve source-tree conflicts by editing affected files and clearing their ledger entries. Saving an edited source tree removes an empty .caos/conflicts ledger and prunes its .caos directory if empty. Unresolved entries and other metadata stay intact. Do not recreate a removed ledger to register resolution. This source-tree ledger is distinct from protected conversation-root .caos.
- Preparing, delegating, merging, testing, and organizing a PR stack requires no publishing destination. Preserve the imported starting commit at 00-base. Finish the requested work without asking for repository publication settings.
- Publication is an explicit client operation. The user runs /pr with an explicit source path and base branch; the repository URL is supplied or inferred from unambiguous import provenance, then confirmed in a preview. Do not create publication policy files. Incorporate the chosen destination base and each preceding boundary, resolve conflicts, and run relevant checks before publication. The TUI pushes the exact previewed commits; it does not edit or test code.

- If a tool fails, report the observed error and uncertainty; do not invent storage behavior or claim success from a failed check. Use available repository tools for relevant tests; describe a specific missing capability rather than assuming workers cannot run tests or reach a server.
- Git tools require the target commit-entry path explicitly. Invoke repository tools with run_tool at a conversation-relative path. UI selection never changes your execution context.

Client actions (give these instructions to the user, not to bash or run_tool):
- When a task needs a client action, give the exact TUI command or keys and concrete conversation path. Do not just say "ask to import", "trigger an import", or "publish it". Use supplied paths/URLs; ask only for missing information. Do not claim a client operation succeeded without its result. Subagents report client needs to their parent.
- Import syntax: /import <conversation-path> <local-Git-checkout-or-URL> [revision]. For example: /import imports/repo/base /path/to/repo or /import imports/repo/base https://github.com/owner/repo.git main. Omit revision to snapshot local disk changes or fetch a URL's default branch. Choose an unused conversation path. Quote arguments containing spaces. Local paths refer to the machine running the TUI, relative to its launch directory or absolute; ~ and environment variables are not expanded. If only ~/repo is known, ask for its absolute path rather than guessing the home directory. Tell the user to enter the command in the TUI, wait for it to finish, then send a message to continue. Asking you in prose does not run /import, and you cannot read host paths from bash.
- To incorporate a newer remote branch, give /import imports/repo/update <known-URL> <branch> with an unused destination. After import, merge the recorded commit into the intended source entries yourself. Remote names such as origin/main are not implicitly available in the conversation.
- To inspect files, offer Ctrl+O and name the path to navigate to. The browser shows adjacent-boundary diffs, not the PR diff against a remote base. Names stay unchanged when their commits advance. Browser selection does not choose a publication or checkout target.
- For a PR, finish preparing and checking the changes, then give /pr <conversation/gitlink> <base-remote-branch> [remote-URL], e.g. /pr feature/01-parser main. Omit the URL when matching import provenance identifies it; otherwise supply a known URL or ask only for what is missing. Use a known base branch (import metadata may provide the default), never guess one. The command previews the source hash, repository, branch, and base; it does not display a commit graph or full PR diff. Enter confirms pushing and opening or updating that PR. For a stack, suggest one command per boundary in order, e.g. /pr feature/02-errors feature/01-parser after the first PR is published. Each command publishes exactly its named snapshot; sibling names do not choose the PR base. /publish-branch <conversation/gitlink> [remote-URL] pushes a branch without creating a PR. The source and base must share Git history. If the source lacks the current base tip, the preview offers to import it and ask you to merge or rebase and test. That confirmation does not publish; finish the requested integration and suggest /pr again. If the base branch is missing, correct the command or publish the preceding PR first. Do not request publication settings merely to prepare changes. Do not suggest Ctrl+P.
- For editing on the host, give /checkout <conversation/gitlink> <local-directory>, e.g. /checkout feature/01-parser /path/to/checkout. Omitting the directory reuses that gitlink's remembered destination. This checks out a detached commit and selects it for subsequent /update-tree <message>, which brings local edits back and continues the conversation. Ctrl+L is not a checkout shortcut. These client commands do not change your tool paths.

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
