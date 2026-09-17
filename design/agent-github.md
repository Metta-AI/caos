# Importing and PRs

Ship agent-driven remote imports first. Keep `/import` for local paths.
PR publication, stack support, and merge drafts follow in the second change.

Based on `main` at `1e1279286` (2026-09-16).

## Importing

### Tool

Add `import_source` as a built-in tool in `std/llm-step`, executed inline like
`read` and `edit`. The user asks for code in the conversation; the agent imports
it and continues when the tool returns.

```text
import_source(source="https://github.com/owner/repo.git", revision="main",
              into="imports/repo/main-2")
```

Return the imported commit hash H and attach a gitlink at an unused path,
preserving the original commit, tree, and history. The tool and server endpoint
accept HTTPS remotes only. Keep using `/import` for local checkouts.

### Remote repositories

The inline handler calls a new `caos import-git <uri> [ref]` command from the
existing `llm-step` container. That command sends a request to a new
`POST /git/import` endpoint, which fetches directly into the server's bare Git
repository. The handler receives the result; repository objects travel directly
from the remote to the server. Importing needs no additional worker image.

For an import of `main`:

1. `import-git` sends the URI, revision, and import identity to the endpoint,
   with the optional GitHub token in a sensitive HTTP header. The import
   identity includes the URI/revision, invocation ID, and `llm-step`'s existing
   `secret-hash` scope; it excludes the token value.
2. The server serializes requests with the same import identity and returns an
   already completed import without fetching again. For a new import, it uses
   the supplied token to authenticate Git.
3. The server resolves `refs/heads/main` to H using `git ls-remote` and durably
   records H in the invocation's state before fetching. A supplied full commit
   hash is already H. A retry uses the recorded H.
4. Reuse an earlier completed import of H in this repository/credential scope.
   Otherwise Git fetches H into the existing object store, transferring its
   trees, blobs, and ancestor commits. There is no checkout. Fetch complete
   history, without shallow or blob filters. A shallow upstream is rejected;
   request-local shallow bookkeeping cannot alter the shared repository.
5. For a fetched H, verify its history and trees through `/object`. Record the
   completed result. Only completed imports supply future negotiation tips.
6. The endpoint returns H and provenance. The inline handler attaches the
   gitlink and records the tool result in the conversation; the server does
   not edit it.

The fetch is equivalent to this, with authentication and negotiation configured
for the request:

```sh
git --git-dir=/git fetch --no-tags --no-write-fetch-head \
  --no-auto-maintenance https://github.com/owner/repo.git \
  "$H"
```

The invocation record holds H and whether transfer completed. Records live in
`<git-dir>/caos-imports/<repository-and-secret-scope>/`, outside Git objects.
A filesystem lock serializes each repository/credential scope across server
processes, making complete negotiation tips stable during transfer. Records
are written with fsync and rename before transfer and after closure validation. A retry resumes
the transfer at H or replays the completed result. Concurrent imports have
separate records and share neither a mutable Git ref nor `FETCH_HEAD`.

Pass commit hashes from complete prior imports of that repository in the same
credential scope as
[negotiation tips](https://git-scm.com/docs/git-fetch#Documentation/git-fetch.txt---negotiation-tipltcommitglobgt),
instead of advertising unrelated CAOS refs. Git handles avoiding repeat transfers.
A new import still contacts the remote to observe its current tip. If it is
still H, reuse its completed import without another fetch or history walk;
if it advanced, Git negotiates the missing objects. With no prior import,
negotiate without local tips. No separate branch-tip cache is needed.

No permanent import ref is required. A ref would keep H reachable for Git GC,
which does not follow conversation gitlinks to their target commits. CAOS
currently disables automatic GC; enabling it later requires defining these roots.

An omitted revision selects the remote's default branch; a full commit hash
selects that exact commit and skips the ref lookup. Use argv, per-command
credentials, disabled prompts, and a timeout; return fetch errors as tool results.
Preserve the server's object durability and maintenance settings. Commit
presence alone is not proof that its tree and history are present.

Resolve `origin/main` from the selected source's repository provenance, then
fetch that repository's `main`. This observes the remote, even if a client has
an older `origin/main`. Use an explicit repository when provenance is ambiguous.

Supply `github-token` when launching the TUI through the same `.caos-secrets`
mechanism used for the Anthropic API key (`anthropic-api-key`):

```text
# .caos-secrets/github-token
name=github-token
value:@=.github-token-value
reader=std/llm-step
```

Keep the value file ignored and initialize its entropy with `caos secrets`.
The existing grant mechanism mounts the token at `/secret/github-token` in
`std/llm-step`. For `github.com` on the default HTTPS port, the inline handler
passes that file to the new command. Other HTTPS hosts receive no automatic
GitHub token:

```sh
caos import-git https://github.com/owner/repo.git main \
  --github-token-file=/secret/github-token --invocation=<64-hex-call-identity>
```

`import-git` reads the file and forwards the token in the request header. It
reads the existing secret scope from `/cas/args/secret-hash`. Without an explicit
invocation it starts a fresh call; the inline handler always supplies one.
The default output is H; `--json` also returns provenance. The
endpoint uses it for ref lookup and fetch through a temporary Git credential
helper, scoped to the intended GitHub host and repository. It does not look up the
calling job's secrets. Keep the token out of stored arguments, import records,
URLs, logs, and shared Git configuration. Reader matching and `secret-hash`
isolation remain part of ordinary worker dispatch.

Public imports can omit the token. The server needs Git but no `gh` installation.
Add `std/github` when implementing PR operations below.

### Imported state

After the objects are available, conditionally add the gitlink and provenance
and record the tool result in the conversation:

```text
imports/repo/main-2              gitlink -> H
imports/repo/main-2.source.json  provenance
```

Record the repository URL, default branch when known, requested revision, fetched
commit, and observation time. Omit credentials from provenance. Reject an
occupied destination without overwriting it.

Reading, building, and merging the snapshot use H without fetching again.
Another request for newer main creates another snapshot. Importing alone does
not update a feature or merge into it; the agent uses the existing merge tool
when integration is requested.

Derive the invocation ID from the persisted conversation request, declaring
round, and tool-call ID. A new tool call gets a new identity; retrying an inline
call keeps its identity and recorded H. Completed calls replay their result.
Reapplying the same completed import must not create a second attachment or
transcript entry. A failed transfer leaves no attached gitlink.

### Shipping

Ship the server endpoint, `caos import-git`, inline tool, attachment logic, and
tool instructions together. Register `import_source` as a reserved built-in,
available before any source tree is attached. Keep `/import` and startup
`--import` for existing client-side workflows, including local paths.
No new local-path handler is needed.
Update `SPEC.md`, [chat.md](chat.md), and command help to direct remote imports to
the agent tool and local imports to `/import`.

Verify branch freshness, private fetches, explicit commit hashes, complete history
and object visibility, incremental transfer, concurrent imports, retries before
and after transfer completes, destination races, and rejection of local paths.
Exercise the inline tool in an empty conversation and without a GitHub token
for public imports; report fetch failures as ordinary tool results.
Keep the existing merge and publication flows in this release.

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
