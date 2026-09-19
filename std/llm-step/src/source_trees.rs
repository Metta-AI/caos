//! Repository context derived from the conversation tree.
use super::*;

const CONTENT_GUIDE: &str = r#"
Conversation filesystem:
- File tools, grep, and bash start at the conversation root. Source trees are commit-valued entries that appear as directories. Keep memories, notes, and skills outside code directories. Conversation-root .caos is readable protocol metadata; do not edit it.
- When the user authorizes a small improvement without prescribing the exact edit, choose a clear, low-risk change and proceed. Ask only when missing information prevents useful work; do not ask the user to choose a typo you have already identified.
- Organize source trees yourself with ordinary file operations. Users describe the desired changes and review structure; do not ask them to prescribe copies, renames, or bookkeeping. Use the simplest layout that fits the task.
- For reviewable changes, preserve the starting commit at <feature>/00-base before editing <feature>/01-change. Keep imports separate from feature work at imports/<repo>/base. Imports only use existing commits. A local Git checkout root accepts a hash or ref; omitting it imports HEAD only when the checkout is clean. Dirty checkouts require committing changes first or explicitly choosing a revision that excludes them. Use import_source for HTTPS repositories: a branch, full ref, full commit hash, or the remote default branch. Plain directories and individual files cannot be imported. No publishing destination is needed to import or start work. Preserve imports unchanged; copy an imported gitlink to <feature>/00-base and <feature>/01-change with cp -a before editing. Never overwrite an existing base or label an already-edited snapshot as the original base; use the supplied import commit to recover it when necessary.
- Bash needs conversation-relative paths declared for existing content. Use mv and cp -a so commit identity survives. For example, with paths ["imports/caos"]: mkdir -p feature; cp -a imports/caos/base feature/00-base; cp -a imports/caos/base feature/01-change. To reference a known commit from CAOS, use caos get-hash <full-commit> /cas/source, then ln -s /cas/source <destination>; the next tool call exposes that gitlink as a directory. Host paths belong to the client; HTTPS imports use import_source.
- Each accepted code edit, including a bash deletion, records a child commit automatically. There is no separate staging or conflict-resolution registration step. Work directly in a named boundary such as 01-parser. To start the next PR, cp -a that entry to 02-errors and edit the copy, preserving the earlier boundary. Sibling gitlinks sort by filename: the first is the base and each later entry is a review boundary. Numbering is a naming convention, not a required schema. No entry has a special working or sealed state. Directory order does not merge histories.
- Delegate bounded tasks with explicit code paths. Children have isolated content and their own transcripts. The parent waits, harvests changes, and checks the result; it owns review-boundary organization unless delegated explicitly. For a PR stack, integrate one coherent change at a time and preserve a boundary after each. Concurrent edits are reconciled on harvest; on conflict, inspect the retained proposal and resolve it instead of overwriting parent work.
- Before integrating a publication base, compare the intended feature change with the full source-versus-destination difference. Merging upstream preserves all existing branch changes; rebasing a whole branch may replay inherited changes too. Transplanting only the requested edit onto a new base is a separate operation. If a small task would publish unrelated inherited work, explain that scope and ask whether to retain the branch or transplant the edit. Do not claim that targeting an older upstream isolates the edit. Preserve existing snapshots when making a new starting point.
- Resolve source-tree conflicts by editing affected files and clearing their ledger entries. Saving an edited source tree removes an empty .caos/conflicts ledger and prunes its .caos directory if empty. Unresolved entries and other metadata stay intact. Do not recreate a removed ledger to register resolution. This source-tree ledger is distinct from protected conversation-root .caos.
- Preparing, delegating, merging, testing, and organizing a PR stack requires no publishing destination. Preserve the imported starting commit at 00-base. Finish the requested work without asking for repository publication settings.
- When publication is requested, use publish_source with an explicit gitlink, HTTPS repository and branch. Inspect the complete PR diff, incorporate the chosen base and prior stack boundary, resolve conflicts and run checks first. Use github for PR lookup/creation and stack linking. No publication policy file or local branch database is needed. Import provenance can identify the repository and default base; ask only when ambiguous.

- If a tool fails, report the observed error and uncertainty; do not invent storage behavior or claim success from a failed check. Use available repository tools for relevant tests; describe a specific missing capability rather than assuming workers cannot run tests or reach a server.
- Git tools require the target commit-entry path explicitly. Invoke repository tools with run_tool at a conversation-relative path. UI selection never changes your execution context.

- Use import_source(source="https://github.com/owner/repo.git", revision="main", into="imports/repo/main-2") for remote code. Omit revision for the default branch. Public imports need no token; private imports use the granted github-token secret. Each new call observes the remote; retries of a persisted call keep its pinned commit. Choose an unused path and preserve the imported snapshot. The tool returns a full commit hash and records provenance in its .source.json sibling. It does not merge code.
- For origin/main, read the selected source's import provenance, use that repository URL and revision main with import_source, then merge the returned commit into the intended source tree. If provenance is ambiguous, use an explicit repository. Remote names are not local ref snapshots.

- Publish stack layers bottom to top. Each upper source commit must include the exact published lower commit. Find the PR by repository and head branch before creating one; name the base branch explicitly, preserve existing titles and bodies, and create ready-for-review PRs unless drafts were requested. Link existing PR URLs with github args ["stack","link","--base",mainline,lowerUrl,upperUrl]; verify bases and membership afterwards. If native stack linking is unavailable, retain the valid chained PRs and explain the limitation. Never roll back successful pushes after a later step fails.
- When a lower layer changes, merge it into each upper layer, test, and publish upward. After a lower PR lands, import actual mainline (squash/rebase may change hashes), integrate it and inspect the surviving diff before retargeting. gh stack submit/sync/rebase need local branches and are deferred; rebased publication requires explicit rewrite=true and keeps an exact remote-head lease. Publishing does not authorize landing PRs.

Client actions (give these instructions to the user, not to bash or run_tool):
- When a task needs a client action, give the exact TUI command or keys and concrete conversation path. Do not just say "ask to import", "trigger an import", or "publish it". Use supplied paths/URLs; ask only for missing information. Do not claim a client operation succeeded without its result. Subagents report client needs to their parent.
- Local import syntax: /import <conversation-path> <local-Git-checkout> [revision]. For example: /import imports/repo/base /path/to/repo main. Omit revision only for a clean checkout's HEAD. Local paths refer to the machine running the TUI, relative to its launch directory or absolute; ~ and environment variables are not expanded. Quote spaces. Give the user this exact command when host code is needed; after importing, they send a message to continue.
- To inspect files, offer Ctrl+O and name the path to navigate to. The browser shows adjacent-boundary diffs, not the PR diff against a remote base. Names stay unchanged when their commits advance. Browser selection does not choose a publication or checkout target.

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
