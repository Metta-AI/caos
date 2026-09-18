# Agent publication and PR stacks

Importing is implemented. Publication builds on it in three steps:

| Step | What ships | Remaining work |
| --- | --- | --- |
| Branches | Server push endpoint, `caos push-git`, and `publish_source` in separate implementation PRs. | Merge the branch-publication implementation. |
| PRs | The stack adds `std/github` and the agent's `github` tool in separate PRs. | Validate creating, updating and reviewing a PR through the agent. |
| Stacks | The worker includes `gh-stack`. Source gitlinks already carry stack boundaries. | Validate publication, linking, updates and landing of a dependent stack. |

The GitHub worker is tested for execution, secret grants and retry handling.
Live PR creation and stack linking remain to be exercised. Those follow-ups
should start with agent instructions and integration tests; the operations
below use the existing tools.

## Branches

The server publishes code commits directly from its bare Git store, without
checking out source files.

| Layer | Interface |
| --- | --- |
| Server | `POST /git/push {destination, commit, branch, expected}` |
| Worker command | `caos push-git <https-url> <commit> <branch> --expected=<oid\|absent>` |
| Agent tool | `publish_source(source_tree, repository, branch)` |

The request fields are:

| Field | Meaning |
| --- | --- |
| `destination` | Remote repository's HTTPS URL, e.g. `https://github.com/owner/repo.git`. |
| `commit` | Full hash H of the code commit to publish, already stored in CAOS. |
| `branch` | Destination branch in that remote, e.g. `feature/parser`, without `refs/heads/`. |
| `expected` | Full hash E expected at that same remote branch, or JSON `null` if it must not exist. Required; the CLI spells `null` as `absent`. |

`refs/heads/feature/parser` is Git's full name for the branch `feature/parser`.
Ordinary branch pushes can infer this prefix from a local branch. Since CAOS
pushes a commit hash, it explicitly names the remote branch:

```text
git push <destination> H:refs/heads/feature/parser
```

This is [standard Git refspec syntax](https://git-scm.com/docs/git-push).
No corresponding branch is needed or created in CAOS. This endpoint publishes
branches only; it does not publish tags or delete refs.

Before pushing, the agent reads the source gitlink for H and the remote branch
for E, then records both with the destination and branch under a fixed
tool-call identity. Concurrent attempts reuse that intent. Recovery observes
the outcome; it never substitutes a newer source commit or refreshes E.

The server:

1. Validates the request and checks H is a code commit. Rejects conversation
   ancestry, reserved source-tree `.caos` content and unresolved markers at
   the tip. Trusts the complete history verified at ingestion and startup.
2. Reads the destination branch. H means success; a value other than E means
   conflict.
3. If E is non-null, require it to be an ancestor of H. If CAOS does not
   have E, import and integrate that history first.
4. Pushes `H:refs/heads/<branch>` with
   `--force-with-lease=refs/heads/<branch>:<E>`, disabling tag following.
   An empty E requires branch creation. The ancestry check permits only
   fast-forward updates; the remote enforces the lease against concurrent writes.
5. Returns a receipt for H. After a lost reply, read the branch again:
   H means success, E or a failed lookup means uncertain, another value means
   conflict. E alone does not prove an earlier push cannot still finish.

Objects transfer directly to the destination using Git's push negotiation.
Reuse import authentication: token-file option, sensitive header and
repository-scoped credential helper. Tokens stay outside Git objects and receipts.

On conflict, import the remote head, integrate, test and make a new call.
On uncertainty, inspect before continuing. A success receipt describes the
original push even if the remote later advances. Publishing leaves source
gitlinks unchanged.

## PRs

`std/github` contains Git, `gh`, and the pinned `github/gh-stack` v0.1.1
extension. It is registered as a built-in tool, available without a project-defined `caos-tools` entry.

The tool is `github(repository, args, stdin?)`. `args` is an argument array passed
directly to `gh`; it is never interpolated into a shell command. Return exit
status, stdout and stderr through ordinary tool results. This covers issues,
comments, PR creation/editing and stack operations without separate wrappers
for each GitHub action. Initially use stdin for bodies (`--body-file -`);
commands needing local file attachments can be added when needed.

The worker sets `GH_REPO` explicitly and reads `GH_TOKEN` from its granted
`/secret/github-token`. Add `reader=std/github` to the existing secret;
`std/llm-step` also needs the grant: it uses the token for import/push and
includes that identity in the model turn’s cache key. The agent carries the
GitHub worker source and evaluates it when called, so secret marking of the
child worker does not alter the agent’s own reader identity. Use an isolated temporary
GitHub configuration and disable prompts. Install the extension in the image.
The worker needs no source checkout or local branches for the operations below.

### Invocation recovery

Identical GitHub commands can observe different remote state, and comments
must not be posted again when a worker retries. The harness therefore binds
a stable, unique invocation ID into every GitHub request. A new tool call gets
a new ID; resuming the same call retains it. Normal result caching then belongs
to that observation, rather than to the command arguments indefinitely.

A unique cache key alone does not prevent duplicate execution. Before running
`gh`, the worker atomically claims a small record under
`refs/caos/github/<invocation-id>` using the existing Git compare-and-swap
transport. The record binds the command/request digest and a unique attempt ID.
Only the attempt that owns the claim executes the command. Record the exit
status and output hashes afterwards; completed duplicates return that result.

If a claim exists without a recorded result, a duplicate reports pending or
uncertain and does not execute `gh`. Never expire or steal that claim based on
elapsed time. A crash between claiming and execution can therefore require
inspection even when nothing happened. This provides at most one wrapper
execution per invocation, not a transaction or exactly-once guarantee at GitHub.

Apply this rule to all commands, including reads, to avoid classifying arbitrary
`gh api` requests as safe or unsafe. After an uncertain write, the agent uses a
new read invocation to inspect GitHub, then decides the remaining action.
An absent PR or comment is not proof that a still-running command cannot create
it. If reconciliation cannot establish the outcome, leave it uncertain rather
than repeat the write. Failure of a multi-step command can leave partial changes.
The tool's exit status and transcript must not claim that nothing happened.

### PR workflow

For a single PR:

1. Integrate required updates and test the chosen source gitlink.
2. Call `publish_source` with its path, repository and remote branch. Continue
   after a confirmed push.
3. Find an open PR with `gh pr list --head <branch>`; select explicitly if
   several match. If absent, use `gh pr create --repo <repository>
   --head <branch> --base <base> --title <title> --body-file -`, passing the
   body through the tool's stdin. Supply these arguments explicitly so
   creation needs no local repository.
4. Check the PR's URL, head commit and base with `gh pr view <url> --json ...`,
   and retain the result in the conversation.

Later source edits advance the same branch through `publish_source`, updating
the existing PR. Preserve human-edited titles and descriptions unless an edit
was requested; use `gh pr edit` for requested metadata or base changes. Use
`gh pr view`, `gh pr checks`, and `gh api` to read discussion, review threads
and checks, and the corresponding CLI/API calls for requested replies.
Create ready-for-review PRs by default. A failed PR creation leaves the
successful branch push intact; recovery follows the invocation rules above.

The PR follow-up should exercise this through the actual agent in a test
repository: publish and open a PR, advance its head, preserve an edited body,
read and answer review feedback, and reconcile an interrupted operation
before proceeding. Keep repeatable failure cases in automated fixtures.
This needs no additional server endpoint or tool for each PR action.

## Stacks

Keep boundaries as ordinary source gitlinks:

```text
imports/repo/base   -> H
feature/01-core     -> A   parent H
feature/02-tests    -> B   parent A
```

Create B by copying A with `cp -a` and editing the copy. Git ancestry establishes
that B includes A. Publication receipts map each gitlink's published commit to
a remote branch; GitHub holds PR URLs, bases and stack membership.

Run the PR workflow bottom to top. The first PR targets the chosen mainline
branch; each later PR targets the preceding layer's branch. Before publishing
an upper layer, verify that it contains the exact lower commit just published.
Initial stacks use branches in one GitHub repository.

Once the PRs exist, link their URLs in order:

```sh
GH_REPO=owner/repo gh stack link --base main \
  https://github.com/owner/repo/pull/123 \
  https://github.com/owner/repo/pull/124
```

With `GH_REPO`, an explicit `--base`, and existing PR URLs, the pinned
[`link` implementation](https://github.com/github/gh-stack/blob/v0.1.1/cmd/link.go)
can use GitHub's API without a checkout or local stack tracking. Branch
arguments take a different path that can push local branches and create PRs.
The worker receives command arguments and PR identifiers; it does not fetch
the source tree.

`link` can change PR bases. Check the resulting bases and membership even when
it exits with warnings. If GitHub stacks are unavailable, retain the correctly
chained PRs and report that linking was unavailable.

There is no atomic transaction spanning several pushes, PRs and stack linking.
Record each completed step and reconcile the remainder after a partial failure;
do not roll back successful pushes or delete PRs automatically.

### Updating and landing

If A changes to A1, merge A1 into B to make B1, test, then publish A1 and B1
in that order. Both remote branches advance normally; the PRs and their URLs
stay the same. Apply this propagation upward for additional layers.

After lower PRs merge, import the actual resulting mainline tip and integrate
it into the next surviving layer. Do not assume squash or rebase merges preserve
A's hash. Inspect the resulting diff so already-landed changes are not proposed
again, retarget the surviving PR if needed, and propagate the update upward.

Keep clean merges and today's conflict handling initially. The separate merge
draft gitlinks in [agent-github.md](agent-github.md#merges) remain a follow-up;
publication still rejects unresolved source state until that design lands.

`gh stack link` extends existing stacks; it does not remove or reorder their
members. Restructuring requires explicit GitHub stack changes and, when code
dependencies change, new source commits. Do not treat another `link` call as a
rebase or a replacement of the entire stack.

### What we reuse and what remains

Use `gh-stack` for GitHub's stack membership operations. The agent composes
CAOS's existing copy, merge, test and publish operations to maintain code
dependencies. This requires no separate stack manifest or persistent local
branch database.

[`gh stack submit`, `sync` and `rebase`](https://docs.github.com/en/pull-requests/reference/stacked-prs-cli-commands)
operate on local branches and tracking state; rebasing also needs a worktree
for code and conflicts. A bare repo containing branch refs alone would not
make those workflows fit. Defer that adapter and history rewrites.

The stack follow-up should exercise a two-layer stack in a test repository:
publish and link it, change the lower layer and propagate upward, append a
third layer, then land a lower PR and update the survivors. Include partial
failure and unavailable-stack cases. Verify the actual pinned extension with
no source checkout; version/help tests do not establish this.

Start with that workflow and its tests. If an operation is missing, add the
smallest helper it needs after checking `gh`, `gh stack` and `gh api`.
Reordering, dropping and rebasing arbitrary layers can follow once there is
a concrete need. Landing remains a separately requested action.

## Interfaces and compatibility

The agent uses publication and GitHub tools for PR operations.
The TUI's publication commands and preview UI are removed.
Existing publication records remain readable. Local imports keep their TUI command.
The separate merge-draft design is deferred.

Implementation: [server push endpoint](../rust/crates/server/src/push.rs),
[agent publication](../std/llm-step/src/publish_source.rs),
[GitHub worker](../std/github/src/main.rs), and
[publication records](../rust/crates/conversation-protocol/src/v3/records.rs).
External behavior: [Git push leases](https://git-scm.com/docs/git-push),
[GitHub stack commands](https://docs.github.com/en/pull-requests/reference/stacked-prs-cli-commands),
[gh-stack implementation](https://github.com/github/gh-stack).
