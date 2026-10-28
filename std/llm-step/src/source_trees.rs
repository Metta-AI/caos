//! Repository context derived from the conversation tree.
use super::*;

const CONTENT_GUIDE: &str = r#"
Conversation filesystem:
- File tools and grep start at the conversation root, and so does a shell: `run_tool` with path `caos-std/bash-tool`. Source trees are commit-valued entries that appear as directories. Keep memories, notes, and skills outside code directories. Conversation-root .caos is readable protocol metadata; do not edit it.
- When the user authorizes a small improvement without prescribing the exact edit, choose a clear, low-risk change and proceed. Ask only when missing information prevents useful work; do not ask the user to choose a typo you have already identified.
- Organize source trees yourself with ordinary file operations. Users describe the desired changes and review structure; do not ask them to prescribe copies, renames, or bookkeeping. Use the simplest layout that fits the task.
- One directory per repository, sources/<repo>/, holds the imported tip at sources/<repo>/00-base (never edited; its .source.json sibling records where it came from), the layers of work as sources/<repo>/NN-<name>, each a gitlink with its pull request's title and body in NN-<name>.pr.md beside it, and scratch work at sources/<repo>/99-dirty. There is no separate imports/ directory. Imports only use existing commits. A local Git checkout root accepts a hash or ref; omitting it imports HEAD only when the checkout is clean. Dirty checkouts require committing changes first or explicitly choosing a revision that excludes them. Use import_source for HTTPS repositories: a branch, full ref, full commit hash, or the remote default branch. Plain directories and individual files cannot be imported. No publishing destination is needed to import or start work. Copy 00-base to 99-dirty with the copy tool before editing, and move 99-dirty to the next NN-<name> when a chunk of work is done. Never overwrite an existing base or label an already-edited snapshot as the original base; use the supplied import commit to recover it when necessary. std/GIT.md (read it from caos-std/GIT.md) is the full guide.
- The shell needs conversation-relative paths declared in `paths` for existing content. Use mv and cp -a so commit identity survives, or the copy and move tools, which need no `paths`. To reference a known commit from CAOS, use caos get-hash <full-commit> /cas/source, then ln -s /cas/source <destination>; the next tool call exposes that gitlink as a directory. Host paths belong to the client; HTTPS imports use import_source.
- Each accepted code edit, including a deletion by the shell, records a child commit automatically. There is no separate staging or conflict-resolution registration step. Work directly in a named boundary such as 01-parser. To start the next PR, cp -a that entry to 02-errors and edit the copy, preserving the earlier boundary. Sibling gitlinks sort by filename: the first is the base and each later entry is a review boundary. Numbering is a naming convention, not a required schema. No entry has a special working or sealed state. Directory order does not merge histories.
- Delegate bounded tasks with explicit code paths. Children have isolated content and their own transcripts. The parent waits, harvests changes, and checks the result; it owns review-boundary organization unless delegated explicitly. For a PR stack, integrate one coherent change at a time and preserve a boundary after each. Concurrent edits are reconciled on harvest; on conflict, inspect the retained proposal and resolve it instead of overwriting parent work.
- Before integrating a publication base, compare the intended feature change with the full source-versus-destination difference. Merging upstream preserves all existing branch changes; rebasing a whole branch may replay inherited changes too. Transplanting only the requested edit onto a new base is a separate operation. If a small task would publish unrelated inherited work, explain that scope and ask whether to retain the branch or transplant the edit. Do not claim that targeting an older upstream isolates the edit. Preserve existing snapshots when making a new starting point.
- Resolve source-tree conflicts by editing affected files and clearing their ledger entries. Saving an edited source tree removes an empty .caos/conflicts ledger and prunes its .caos directory if empty. Unresolved entries and other metadata stay intact. Do not recreate a removed ledger to register resolution. This source-tree ledger is distinct from protected conversation-root .caos.
- Preparing, delegating, merging, testing, and organizing a PR stack requires no publishing destination. Preserve the imported starting commit at 00-base. Finish the requested work without asking for repository publication settings.
- Keep a stack (sources/<repo>/00-base, 01-core, 02-tests) current by merging, never by rewriting a layer: after changing a layer, merge it into the layer above, and so on up. To bring a stack onto a moved base, which is what a request to rebase means here, import the new base at sources/<repo>/00-base-2, remove 00-base and its .source.json, move the new pair into their place, merge it into the bottom layer and on up, and test. If the new base does not contain the old one (it was force-pushed), rebuild each layer bottom-up instead: replace it with a copy of the rebuilt layer below, then merge in its old tip with merge-base set to the old tip of the layer below. To rename or renumber a layer together with its .pr.md, use run_tool caos-std/move-sed, never two moves.
- When publication is requested, write each layer's pull request as sources/<repo>/NN-<name>.pr.md (first line the title, the rest the body) and run run_tool caos-std/publish-stack with stack=sources/<repo>. It pushes every described layer as it is, bottom first and fast-forward (nothing is squashed; the trunk squash-merges), to a branch named after the layer without its number, opens or updates each PR on the branch below (the trunk, from import provenance, for the first), updates a PR whose text differs from its .pr.md, and links the PRs as a GitHub stack; it refuses before pushing a layer that has not merged the one below, naming the merge to run, and a described layer above an undescribed one. 99-dirty has no .pr.md and is never published. Run it with dry-run=true first on a new stack. Inspect the complete PR diff, resolve conflicts and run checks before publishing. Report the PR URLs it prints. Use publish_source only for a single commit to a single branch, and caos-std/github for what the operation does not do: reading and answering review comments, and anything else on GitHub.

- If a tool fails, report the observed error and uncertainty; do not invent storage behavior or claim success from a failed check. Use available repository tools for relevant tests; describe a specific missing capability rather than assuming workers cannot run tests or reach a server.
- Git tools require the target commit-entry path explicitly. Invoke repository tools with run_tool at a conversation-relative path. UI selection never changes your execution context.
- Repository tools are not listed for you. A tool is any directory carrying a .caos-expr that binds a help, and each repository documents its own in AGENTS.md or its docs. Call tool_help with that conversation-relative path (for example feature/01-change/caos-tools/test) to get its parameters, then run_tool with the same path and those arguments under `arguments`. tool_help reads the tree only and never builds anything, so prefer it over guessing parameters from prose; the docs can be stale, the help cannot.
- The caos tools ship at caos-std/<name> and are reached the same way: caos-std/bash-tool is the shell, caos-std/caos-build compiles the caos tree, caos-std/caos-test runs its suite, caos-std/caos-test-result prints one test's full record, caos-std/github calls the GitHub API. A tool whose help lists `in` accepts a conversation path there, which is how one of these runs over a source tree other than the one holding it.

- Use import_source(source="https://github.com/owner/repo.git", revision="main", into="sources/repo/00-base-2") for remote code. Omit revision for the default branch. Public imports need no token; private imports use the granted github-token secret. Each new call observes the remote; retries of a persisted call keep its pinned commit. Choose an unused path and preserve the imported snapshot. The tool returns a full commit hash and records provenance in its .source.json sibling. It does not merge code.
- For origin/main, read the selected source's import provenance, use that repository URL and revision main with import_source, then merge the returned commit into the intended source tree. If provenance is ambiguous, use an explicit repository. Remote names are not local ref snapshots.

Client actions (give these instructions to the user, not to the shell or run_tool):
- When a task needs a client action, give the exact TUI command or keys and concrete conversation path. Do not just say "ask to import" or "trigger an import". Use supplied paths/URLs; ask only for missing information. Do not claim a client operation succeeded without its result. Subagents report client needs to their parent.
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
