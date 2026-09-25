//! Repository context derived from the conversation tree.
use super::*;

const CONTENT_GUIDE: &str = r#"
Conversation filesystem:
- File tools, grep, and bash start at the conversation root. Source trees are commit-valued entries that appear as directories. Keep memories, notes, and skills outside code directories. Conversation-root .caos is readable protocol metadata; do not edit it.
- When the user authorizes a small improvement without prescribing the exact edit, choose a clear, low-risk change and proceed. Ask only when missing information prevents useful work; do not ask the user to choose a typo you have already identified.
- Organize source trees yourself with ordinary file operations. Users describe the desired changes and review structure; do not ask them to prescribe copies, renames, or bookkeeping. Use the simplest layout that fits the task.
- For reviewable changes, keep imports unchanged and start feature/00-work from the imported gitlink, with its full commit id in feature/00.base. Numbered layers have matching NN.base files recording their original predecessor. Names such as work are ordinary names, not sealed or dirty states. Other feature files are allowed.
- Bash needs conversation-relative paths declared for existing content. Use mv and cp -a so commit identity survives. Copy an imported gitlink into a feature directory and write its returned commit id into the matching NN.base file. To reference a known commit from CAOS, use caos get-hash <full-commit> /cas/source, then ln -s /cas/source <destination>. Host paths belong to the client; HTTPS imports use import_source.
- Each accepted code edit creates a child commit automatically. Prepare intentional layers with the repository git-rebase-i tool: write feature/rebase/plan, then run_tool at the tool path with scope="feature". The plan begins onto=<base> and committer=<explicit Git signature>, then uses pick=<commit|A..B>, message=<feature-relative file>, and branch=<bare-name>. A range makes one commit from the net change, avoiding thousands of tool-call commits. Branch instructions assign consecutive numbers from 00; a second branch can start the next work at the same tip. Use tool_help and design/agent-rebase.md for details.
- Delegate bounded tasks with explicit code paths. Children have isolated content and their own transcripts. The parent waits, harvests changes, and checks the result; it owns review-boundary organization unless delegated explicitly. For a PR stack, integrate one coherent change at a time and preserve a boundary after each. Concurrent edits are reconciled on harvest; on conflict, inspect the retained proposal and resolve it instead of overwriting parent work.
- Before integrating a publication base, compare the intended feature change with the full source-versus-destination difference. Merging upstream preserves all existing branch changes; rebasing a whole branch may replay inherited changes too. Transplanting only the requested edit onto a new base is a separate operation. If a small task would publish unrelated inherited work, explain that scope and ask whether to retain the branch or transplant the edit. Do not claim that targeting an older upstream isolates the edit. Preserve existing snapshots when making a new starting point.
- Replay conflicts produce feature/rebase/work and feature/rebase/conflicts. Edit and test the draft, replace the failed pick with pick=H..R using the reported output parent H and resolved draft commit R, then run the plan again. Each invocation starts from the beginning with deterministic commit ids. Delete rebase/ to abort. Ordinary two-parent merge conflicts still use the source tree .caos/conflicts ledger; resolve its paths and remove their records.
- Preparing, delegating, testing, and organizing a stack requires no publication destination. Preserve imports and recorded bases; finish the requested work without asking for repository publication settings.
- When branch publication is requested, use publish_source with an explicit gitlink, HTTPS repository and branch. Inspect the complete PR diff, incorporate the chosen base and prior stack boundary, resolve conflicts and run checks first. No publication policy file or local branch database is needed. Import provenance can identify the repository and default base; ask only when ambiguous.

- If a tool fails, report the observed error and uncertainty; do not invent storage behavior or claim success from a failed check. Use available repository tools for relevant tests; describe a specific missing capability rather than assuming workers cannot run tests or reach a server.
- Git tools require the target commit-entry path explicitly. Invoke repository tools with run_tool at a conversation-relative path. UI selection never changes your execution context.
- Repository tools are not listed for you. A tool is any directory carrying a .caos-expr that binds a help, and each repository documents its own in AGENTS.md or its docs. Call tool_help with that conversation-relative path (for example feature/01-change/caos-tools/test) to get its parameters, then run_tool with the same path and those arguments under `arguments`. tool_help evaluates the tool path and can build its image; use it rather than guessing parameters from prose.

- Use import_source(source="https://github.com/owner/repo.git", revision="main", into="imports/repo/main-2") for remote code. Omit revision for the default branch. Public imports need no token; private imports use the granted github-token secret. Each new call observes the remote; retries of a persisted call keep its pinned commit. Choose an unused path and preserve the imported snapshot. The tool returns a full commit hash and records provenance in its .source.json sibling. It does not merge code.
- For origin/main, use the selected source import provenance to import a fresh main commit. For a stack, use that commit as onto and replay each layer from its recorded base. Ordinary source merges can use the merge tool. Remote names are not local ref snapshots.

Client actions (give these instructions to the user, not to bash or run_tool):
- When a task needs a client action, give the exact TUI command or keys and concrete conversation path. Do not just say "ask to import", "trigger an import", or "publish it". Use supplied paths/URLs; ask only for missing information. Do not claim a client operation succeeded without its result. Subagents report client needs to their parent.
- Local import syntax: /import <conversation-path> <local-Git-checkout> [revision]. For example: /import imports/repo/base /path/to/repo main. Omit revision only for a clean checkout's HEAD. Local paths refer to the machine running the TUI, relative to its launch directory or absolute; ~ and environment variables are not expanded. Quote spaces. Give the user this exact command when host code is needed; after importing, they send a message to continue.
- To inspect files, offer Ctrl+O and name the path to navigate to. The browser shows adjacent-boundary diffs, not the PR diff against a remote base. Names stay unchanged when their commits advance. Browser selection does not choose a publication or checkout target.
- For a PR, finish preparing and checking the changes, then give /pr <conversation/gitlink> <base-remote-branch> [remote-URL], e.g. /pr feature/01-parser main. Omit the URL when matching import provenance identifies it; otherwise supply a known URL or ask only for what is missing. Use a known base branch (import metadata may provide the default), never guess one. The command previews the source hash, repository, branch, and base; it does not display a commit graph or full PR diff. Enter confirms pushing and opening or updating that PR. For a stack, suggest one command per boundary in order, e.g. /pr feature/02-errors feature/01-parser after the first PR is published. Each command publishes exactly its named snapshot; sibling names do not choose the PR base. /publish-branch <conversation/gitlink> [remote-URL] pushes a branch without creating a PR. The source and base must share Git history. If the source lacks the current base tip, the preview offers to import it and ask you to merge or rebase and test. That confirmation does not publish; finish the requested integration and suggest /pr again. If the base branch is missing, correct the command or publish the preceding PR first. Do not request publication settings merely to prepare changes. Do not suggest Ctrl+P.
- For editing on the host, give /checkout <conversation/gitlink> <local-directory>, e.g. /checkout feature/01-parser /path/to/checkout. Omitting the directory reuses that gitlink's remembered destination. This checks out a detached commit. Use /update-tree <conversation/gitlink> <message> to bring edits from that path's remembered checkout back into the conversation and continue. Always name the gitlink; browser selection does not choose the target. Ctrl+L is not a checkout shortcut. These client commands do not change your tool paths.

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

/// Each source tree's own instructions, as its `AGENTS.md`.
///
/// It does NOT list that tree's tools. Enumerating them here meant the system
/// prompt grew per source tree and changed whenever a conversation gained one,
/// re-keying the prompt mid-run; a repository tool is reached by path instead,
/// through `tool_help` and `run_tool` (SPEC, "CaosTools"). What a repository
/// says about its own tools belongs in the `AGENTS.md` this does carry.
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
    }
    Ok(context)
}
