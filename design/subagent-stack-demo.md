# Subagents to a PR stack

This tour uses a tiny Bash project so the interesting part is delegation,
workspace history, and publication. It makes two PRs with dependent bases:

```text
GitHub main <- feature + documentation PR <- tests PR
CAOS:          workspace main               workspace checks
```

Two parallel subagents contribute to the first workspace. A third starts from
their combined result; promoting its result creates the second workspace.

## Workspace commands

Names in these examples are workspace names within the current conversation.
Selecting a workspace changes the UI focus, not the checkout or an active turn.

| Command | Effect |
| --- | --- |
| `/workspace` | Open the workspace picker and inspect heads, changes, and dependencies. |
| `/workspace use main` | Select an existing workspace. |
| `/workspace attach docs REPOSITORY [BRANCH\|COMMIT]` | Import code into a new workspace. A branch establishes an upstream; a full commit hash stays pinned. Omit the revision to use the repository's default branch. |
| `/workspace create fresh` | Start from the selected workspace's incorporated upstream commit, falling back to its initial commit. Excludes its unintegrated edits. |
| `/workspace create review REVISION` | Start at a revision available to the client. |
| `/workspace copy alternative` | Copy the selected workspace's current snapshot and retain its upstream. |
| `/workspace stack next` | Copy the selected snapshot and make the selected workspace its upstream. This creates a dependency for updates and PR bases. |
| `/workspace update [NAME\|--all]` | Integrate upstream changes. Without an argument, update the selected workspace's whole stack, including dependents; `--all` updates all stacks in dependency order. |
| `/workspace branch NAME BRANCH` | Choose the destination branch for publication. Does not push it. |
| `/workspace rollback NAME COMMIT` | Restore a previously recorded workspace commit and its upstream checkpoint, retaining the publication destination. |
| `/workspace remove NAME` | Remove a workspace from the current conversation state. A workspace with dependents cannot be removed. |

Attachment accepts a pinned locator too, such as
`/workspace attach docs git+https://github.com/OWNER/REPO.git?rev=FULL_SHA`.
Currently use `/workspace use NAME` to switch; `/workspace list` and
`/workspace NAME` are not aliases.

**Attach** brings in existing code. **Stack** creates a new change that depends
on another workspace. Neither runs an agent or publishes anything.

## Prepare

On the prepared EC2 checkout:

```bash
ssh nishadsingh-box-3
cd ~/caos-model-simplify-20260907
./dev/demo-subagent-stack OWNER/NEW-REPO
```

Replace OWNER/NEW-REPO with a new repository name in an account where you can
create repositories. This creates a **private GitHub repository** and pushes
only the starter project. It prints a demo directory, a launch command, and a
PROMPT.md file. With no argument, the script only prepares local files and
prints the GitHub setup command.

Git and `gh` must be authenticated in the environment running the TUI for
publication. The TUI also needs your Anthropic key to run the subagents.

## Present the tour

1. Run the printed launch command. Show the single `main` workspace and the
   tiny greeting command. There is no build system to explain.
2. Paste [the demo prompt](../examples/subagent-stack/PROMPT.md) into the
   composer and press Ctrl+S. Watch the code and documentation children work
   independently, then watch the parent harvest both into `main`.
3. The test child starts from that combined result. The parent promotes it as
   `checks`, preserving a separate reviewable change instead of harvesting it.
4. When the parent finishes, press Ctrl+O. Inspect both workspaces:
   `main` changes greet.sh and README.md; `checks` adds test.sh relative to
   `main`. Its upstream should say workspace `main`.
5. Press Ctrl+P. Select both workspaces and inspect the proposed branches and
   bases. Confirm publication. The host prepares and publishes the parent
   before the child, then opens or reuses each PR.
6. Open the resulting GitHub PRs. The feature PR targets the repository's
   default branch. The tests PR targets the feature PR's branch, so its diff
   contains only the tests.

Publication preparation can make additional workspace commits. Wait for all
subagents and review the workspace diffs before publishing.

The key distinction: **harvest combines work in an existing workspace; promote
gives a child's result its own workspace and review boundary.** Combining every
child into one workspace would produce one PR.
