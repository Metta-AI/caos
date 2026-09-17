# Importing and PRs

Ship remote imports first, in three PRs: the server endpoint, the worker
command, then the agent tool. Keep /import for local checkouts.
PR publication, stacks, and merge drafts follow separately.

## Importing

### Agent tool

Add `import_source(source, revision?, into)` as an inline tool in std/llm-step.
It accepts an HTTPS repository and a branch name, full ref, or full commit
hash. Omitting revision selects the remote default branch. It works before
any source tree is attached.

For a request to import main:

1. Read this tool call's saved import observation, if any.
2. Otherwise run git ls-remote in the existing agent container to resolve main
   to H. Save H, the URL, requested revision, default branch when known, and
   observation time in a tool.start payload in the conversation.
3. Run `caos import-git <url> H`. The command asks the server to fetch H directly
   from the remote into its object store.
4. Once import succeeds, atomically attach the gitlink and provenance and
   record the tool result.

The saved observation precedes the import request. Restarted attempts reuse H;
concurrent attempts use whichever observation was recorded first. A completed
call reuses its result. A new tool call resolves the remote again.

Ref lookup transfers no repository objects and needs no checkout. llm-step
already runs with Git. Repository objects travel directly from the remote to
the server; no additional worker image is needed.

### Server endpoint and command

POST /git/import accepts exactly:

    {"source": "https://github.com/owner/repo.git", "commit": "<full hash>"}

It returns {"commit": H} once H and its complete history are available through
the object API. It rejects branch names, local paths, and incomplete history.
It does not resolve refs, track invocations, or edit conversations.

The server fetches into its existing bare Git repository:

    git --git-dir=/git fetch --no-tags --no-write-fetch-head --no-auto-maintenance <url> H

Only verified complete imports supply negotiation tips. A completion marker
for a repository and H skips subsequent fetches and history checks for H.
A filesystem lock coordinates imports from the same repository. A failed
transfer can be retried with the same input; commit presence alone does not
prove its trees and history are complete.

Completion markers live outside Git objects. They certify object availability,
not permission to access a remote: an already imported hash can be reused
without credentials, as with the object API. Automatic GC is disabled today.
Future GC must retain imported commits explicitly; gitlinks and completion
markers are not Git GC roots.

`caos import-git <https-url> <commit>` is a thin client that prints H. It accepts
--github-token-file=PATH and forwards the file's contents in the sensitive
X-Caos-Git-Token header. It has no invocation ID or provenance format.

### Credentials

Supply github-token through the TUI's existing secret mechanism:

    # .caos-secrets/github-token
    name=github-token
    value:@=.github-token-value
    reader=std/llm-step

Keep the value file ignored and initialize its entropy with caos secrets.
The grant mounts the token at /secret/github-token. The inline handler uses
it for ref lookup and passes it to import-git for github.com on the default
HTTPS port. Other hosts receive no automatic GitHub credential.

Ref lookup and server fetch share the same per-command Git credential helper.
Credentials stay scoped to the repository and out of URLs, saved arguments,
provenance, logs, and Git configuration. The endpoint receives the token
directly; it does not look up the calling job's secrets. Public repositories
need no token, and neither importing component needs gh.

### Attached snapshots

The tool adds two entries at an unused destination:

    imports/repo/main-2              gitlink -> H
    imports/repo/main-2.source.json  provenance

Reject an occupied gitlink or provenance path without overwriting either.
A failed import attaches nothing. Reading, building, and merging the snapshot
use H. Importing does not merge into or advance an existing source tree.

For origin/main, choose the repository from the selected source's provenance
and request main. Ask for an explicit repository when provenance is ambiguous.
Keep /import and startup --import for existing client workflows and local paths.

### Tests

The git-import suite entry starts a private HTTPS remote and a separate server.
It covers exact commits, full history, object visibility, credentials,
incremental transfer, concurrent calls, retries, and rejected inputs.
The command layer extends the same fixture. Agent tests cover ref parsing,
durable pinning, competing observations, lost acknowledgements, replay,
attachment races, and importing into an empty conversation.

## PRs

### GitHub tools

Add `std/github` with Git, a general `gh` operation, a `git_push` helper, and a
pinned version of GitHub's
[`gh-stack` extension](https://github.com/github/gh-stack).

| Operation | Inputs and results |
| --- | --- |
| `gh` | CLI arguments, repository context, stdin, and input/output files. Returns exit status, stdout, stderr, and requested files. |
| `git_push` | Selected source commit, remote repository, branch, and expected remote head. Pushes the commit and its history. |

Add `std/github` as a reader of the same `github-token` secret. The worker sets
`GH_TOKEN` from its mounted secret and configures Git to authenticate through
`gh`. Use [`gh api`](https://cli.github.com/manual/gh_api) for operations without
dedicated CLI commands.

### Publication

Push finalized source gitlinks with their exact commit history. Reuse source
validation and fast-forward checks, and require the remote head to match the
expected value when applying the push. Conversation data and merge bookkeeping
stay outside the published history.

[Create PRs](https://cli.github.com/manual/gh_pr_create) with explicit heads and bases:

```sh
gh pr create --repo owner/repo --head feature/01-core \
  --base main --title "Describe the change" --body-file -
```

Inspect existing PRs before creating or updating them. Specify repositories
for fork branches and preserve human-edited metadata unless asked to change it.
New PRs are ready for review unless a draft was requested.

The TUI displays activity, results, and diffs. A request to review first pauses
before publication; otherwise no separate preview or slash command is required.

The server transfer operation can later support pushing a selected source commit
directly to a remote branch, with the same expected-head check. This avoids
loading the history into a publication worker; `gh` still handles PR metadata.

### Stacks

Each stack boundary is a gitlink: a Git tree entry pointing to a source commit,
displayed as a directory to the agent.

```text
imports/repo/base   -> H
feature/00-base     -> H
feature/01-core     -> A
feature/02-tests    -> B
```

Copy imported H with `cp -a` to start the feature. After editing `01-core` to
produce A, copy it to `02-tests`. Further edits advance only that copy to B.
Git ancestry records the dependency; directory names help display the order.

Publish the same commits under remote branch names:

| CAOS entry | Remote branch | PR base |
| --- | --- | --- |
| `feature/01-core -> A` | `feature/01-core -> A` | `main` |
| `feature/02-tests -> B` | `feature/02-tests -> B` | `feature/01-core` |

If A changes, merge its new commit into B, test, and push the updated commits.

### Using `gh stack`

After pushing and creating PRs, link their URLs in bottom-to-top order:

```sh
GH_REPO=owner/repo gh stack link --base main \
  https://github.com/owner/repo/pull/123 \
  https://github.com/owner/repo/pull/124
```

[`gh stack link`](https://docs.github.com/en/pull-requests/reference/stacked-prs-cli-commands#gh-stack-link)
groups existing PRs without local stack tracking. Use PR URLs: branch arguments
can push local branches and create PRs. The command can correct PR bases, so
supply the intended order and base. The worker sets
[`GH_REPO`](https://github.com/cli/go-gh/blob/trunk/pkg/repository/repository.go)
from the repository argument; this operation needs no local stack branches or
checkout. `gh stack merge` with an explicit stack or PR number also operates
remotely, when the user requests landing the changes.

Local commands such as `view`, `push`, and `submit` require branches and
[`gh-stack` metadata](https://github.com/github/gh-stack#local-tracking).
A worker could load the gitlink commits into a temporary repository, create
branch refs at those commits, and register their order with `gh stack init`.
Rewriting commands such as `rebase` and `sync` would also need to return new
commits and any interrupted operation state, then conditionally update CAOS's
gitlinks.
[`modify` requires linear history](https://docs.github.com/en/pull-requests/reference/stacked-prs-cli-commands#gh-stack-modify),
so it cannot operate on stacks containing the merge commits described here.

Use CAOS's copy, edit, and merge tools to maintain stacks initially, and `link`
to publish their relationship.
[Native GitHub stacks](https://docs.github.com/en/pull-requests/get-started/about-stacked-prs)
are in public preview and require branches in the same repository; ordinary
chained PRs remain available without that feature. GitHub may rebase remaining
branches when a parent lands. Fetch those changed heads and reconcile them
before continuing.

### Merging

The existing Git merge worker handles clean merges as before: merge commit,
fast-forward, or no-op. On conflict, leave the source at O and create a merge
attempt in the conversation:

```text
feature/01-core                  gitlink -> O

merges/update-main/
  ours                          gitlink -> O   frozen input
  theirs                        gitlink -> T   frozen input
  work                          gitlink -> D0  editable draft
  conflicts                     Git's complete conflict report
```

The draft has its own gitlink. Its conflict report and input references live
beside it, outside the code tree. New source and draft commits contain no
CAOS-generated `.caos/conflicts` file.

D0 contains Git's proposed merged tree, including any inline conflict markers,
with O as its single parent. Editing `work` creates ordinary child commits D1,
D2, and so on. File tools, diffs, and repository tests work on this draft.

After resolving the conflicts, the agent calls:

```text
finish_merge(attempt="merges/update-main", target="feature/01-core")
```

This takes the draft's current tree R and the recorded inputs O and T:

```text
M = commit(tree=R, parents=[O, T])
feature/01-core -> M
```

M retains the original merge parents. Draft commits remain reachable through
conversation history but are excluded from M's ancestry. Verify the inputs
against the attempt's creation record and recheck the draft and target when
applying the result. Advance the target only if it still points to O; otherwise
retain the result for reconciliation. Test the final source before publication.

Finishing asserts that the agent resolved the conflicts. Preserve
[Git's complete report](https://git-scm.com/docs/git-merge-tree#_mistakes_to_avoid),
including status, messages, and stage objects for structural conflicts. The
agent need not delete report rows. Neither removing text markers nor an empty
conflicted-path list proves resolution.

To delegate resolution, copy the whole attempt with `cp -a`. The child edits
`work`; the parent harvests it and finishes against its selected target.
Copying only `work` carries draft code without the attempt. Copying the original
source still carries O, and abandoning the attempt leaves it unchanged.

### Retries

Give GitHub commands invocation IDs with the same replay rules as imports.

Atomically claim an invocation before executing it, then record its exit status
and output. A duplicate worker must not repeat a started command. If a worker
crashes after a write, or a command partially succeeds, inspect GitHub before
continuing. Do not automatically retry arbitrary writes.

Merge computation remains content-addressed by its inputs. Draft edits and
completion use conversation history and conditional updates.

### Shipping

Remove `/pr`, `/publish-branch`, and their UI once agent publication works.
Handle existing source-tree conflict ledgers before removing their compatibility
cleanup. Update `SPEC.md`, [chat.md](chat.md), and tool instructions as each
change ships.
