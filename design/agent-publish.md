# Agent publication

CAOS publishes exact code commits from its server. The GitHub worker manages PR metadata. See [agent-stacks.md](agent-stacks.md) for stack registration, restacking, conflict resolution and branch
publication.

## Branches

The server publishes code commits directly from its bare Git store, without checking out source files.

| Layer | Interface | | --- | --- | | Server | `POST /git/push {destination, commit, branch, expected, rewrite?}` | | Worker command | `caos push-git <https-url> <commit> <branch>
--expected=<oid\|absent>` | | Agent tool | `publish_source(source_tree, repository, branch, rewrite?)` |

The request fields are:

| Field | Meaning | | --- | --- | | `destination` | Remote repository's HTTPS URL, e.g. `https://github.com/owner/repo.git`. | | `commit` | Full hash H of the code commit to publish, already stored in
CAOS. | | `branch` | Destination branch in that remote, e.g. `feature/parser`, without `refs/heads/`. | | `expected` | Full hash E expected at that same remote branch, or JSON `null` if it must not
exist. Required; the CLI spells `null` as `absent`. | | `rewrite` | Optional boolean, default false. Permit a non-fast-forward update while still requiring the exact expected remote head. |

`refs/heads/feature/parser` is Git's full name for the branch `feature/parser`. Ordinary branch pushes can infer this prefix from a local branch. Since CAOS pushes a commit hash, it explicitly names
the remote branch:

```text
git push <destination> H:refs/heads/feature/parser
```

This is [standard Git refspec syntax](https://git-scm.com/docs/git-push). No corresponding branch is needed or created in CAOS. This endpoint publishes branches only; it does not publish tags or
delete refs.

The endpoint performs one push:

1. Validate the HTTPS destination, branch and full hashes; require H to be a
   stored commit. Trust the complete history verified at ingestion and startup.
2. If E is non-null, require E to be a stored ancestor of H. If H contains E,
   CAOS already has E. A missing E is a rejection. An explicit rewrite: true skips
   the ancestry check for an intentional rebased-history update.
   Reject H if its tree contains paths matched by its own .gitignore rules.
3. Push H to the destination branch with
   --force-with-lease=refs/heads/<branch>:<E>, disabling tag following.
   Empty E requires creation. The default permits only creates and fast-forwards;
   rewrite: true allows a history rewrite while retaining that exact lease.
   Duplicate requests to the same branch are serialized.
4. Return complete, conflict, or uncertain, with a reason. Known validation and
   per-ref receiver rejections are definite failures. Unconfirmed transport
   failures are uncertain. The CLI preserves these results for llm-step.

The endpoint does not fetch, import, merge, rebase, rewrite commits, or perform a follow-up remote lookup. Objects transfer directly from CAOS to the destination through Git. Reuse import
authentication: token-file option, sensitive header and repository-scoped credential helper.

llm-step owns the workflow. It selects a source gitlink, resolves conflicts, tests and inspects the diff, then reads its commit H and the remote head E. Before sending, it records H, E, destination
and branch under the tool-call identity. Every attempt may send that same pinned intent; recovery never substitutes a newer commit or refreshes the lease. Publishing leaves the source gitlink
unchanged.

On a lease conflict, llm-step can separately import the remote head, merge or rebase, test, and make a new publication call. Other rejections carry their reason through the CLI to the tool result.
Importing and integration are never hidden inside a push.

A remote can accept a push before the connection drops. Git's HTTP retry can then report a stale lease. After a receiver conflict or uncertain result, llm-step reads the branch: H confirms completion.
Otherwise it preserves a definite rejection; for uncertainty, another value than E is a conflict, while E or a failed lookup remains uncertain because a push may still be running. The endpoint itself
does no recovery. A success receipt records the original push even if the branch later advances.

Publication transfers the exact commit. Before pushing, the server checks H's tree against its versioned .gitignore files, including nested rules and negations. A match returns HTTP 422 with code
`ignored-files`; the CLI and agent retain this as a definite rejection without remote reconciliation. Git performs the check using a private index; no source files are checked out.

This is deliberately stricter than ordinary Git: a tracked file matching an ignore rule is rejected too. Global excludes and .git/info/exclude do not apply. This checks the requested snapshot only,
not earlier commits; a file added and deleted in its history is outside this check. The server never strips files or rewrites history.

Local Git staging still respects .gitignore for untracked files. Imports keep their exact commits, and agent tools continue to capture files as they do today. Ignored scratch files can therefore
remain in a source during work; the agent must remove them or adjust the rules before publication.

Registered stacks keep conflicts in a separate draft gitlink and report, outside source history. Older merge operations can still create a .caos/conflicts ledger; resolve it before publishing. The
generic endpoint does not scan for that ledger, inline markers, or conversation ancestry.

## PRs

`std/github` contains Git, `gh`, and the pinned `github/gh-stack` v0.1.1 extension. It is registered as a built-in tool, available without a project-defined `caos-tools` entry.

The tool is `github(repository, args, stdin?)`. `args` is an argument array passed directly to `gh`; it is never interpolated into a shell command. Return exit status, stdout and stderr through
ordinary tool results. This covers issues, comments, PR creation/editing and stack operations without separate wrappers for each GitHub action. Initially use stdin for bodies (`--body-file -`);
commands needing local file attachments can be added when needed.

The worker sets `GH_REPO` explicitly and reads `GH_TOKEN` from its granted `/secret/github-token`. Add `reader=std/github` to the existing secret; `std/llm-step` also needs the grant: it uses the
token for import/push and includes that identity in the model turn’s cache key. The agent carries the GitHub worker source and evaluates it when called, so secret marking of the child worker does not
alter the agent’s own reader identity. Use an isolated temporary GitHub configuration and disable prompts. Install the extension in the image. The worker needs no source checkout or local branches for
the operations below.

### Invocation recovery

Identical GitHub commands can observe different remote state, and comments must not be posted again when a worker retries. The harness therefore binds a stable, unique invocation ID into every GitHub
request. A new tool call gets a new ID; resuming the same call retains it. Normal result caching then belongs to that observation, rather than to the command arguments indefinitely.

A unique cache key alone does not prevent duplicate execution. Before running `gh`, the worker atomically claims a small record under `refs/caos/github/<invocation-id>` using the existing Git
compare-and-swap transport. The record binds the entire pinned ArgTree (arguments, worker image, secret identity and salt) and a unique attempt ID. Recovery dispatches the stored task, including its
original worker image. Reusing an invocation with a different ArgTree is an error; changed worker or secret identity requires a new invocation, after reconciling any earlier write. This preserves
result-cache identity and prevents sharing results across credentials.

A failed claim with no retained ref is a retryable job failure. Once gh has run, retry output storage and result-CAS bookkeeping up to three times, without rerunning gh. Exhausted bookkeeping leaves
the claim uncertain.

Only the attempt that owns the claim executes the command. Record the exit status and output hashes afterwards; completed duplicates return that result.

If a claim exists without a recorded result, a duplicate reports pending or uncertain and does not execute `gh`. Never expire or steal that claim based on elapsed time. A crash between claiming and
execution can therefore require inspection even when nothing happened. This provides at most one wrapper execution per invocation, not a transaction or exactly-once guarantee at GitHub.

Apply this rule to all commands, including reads, to avoid classifying arbitrary `gh api` requests as safe or unsafe. After an uncertain write, the agent uses a new read invocation to inspect GitHub,
then decides the remaining action. An absent PR or comment is not proof that a still-running command cannot create it. If reconciliation cannot establish the outcome, leave it uncertain rather than
repeat the write. Failure of a multi-step command can leave partial changes. The tool's exit status and transcript must not claim that nothing happened.

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

Later source edits advance the same branch through `publish_source`, updating the existing PR. Preserve human-edited titles and descriptions unless an edit was requested; use `gh pr edit` for
requested metadata or base changes. Use `gh pr view`, `gh pr checks`, and `gh api` to read discussion, review threads and checks, and the corresponding CLI/API calls for requested replies. Create
ready-for-review PRs by default. A failed PR creation leaves the successful branch push intact; recovery follows the invocation rules above.

## Stacks

A registered stack consists of ordered source gitlinks and a manifest recording each layer's old predecessor. CAOS updates these commits using Git's object-based merge engine and retains paused
conflicts outside source history. See [the stack workflow](agent-stacks.md) for the representation and tool calls.

`push_stack(path, repository, rewrite?)` pins all source commits and remote-head leases, then pushes branches bottom to top using the same publication path as `publish_source`. It records each outcome
before proceeding and reports partial progress if a later branch fails. Recovery retains the original pins and skips completed branches.

Pushing branches is independent of PR creation and GitHub stack membership. The conversation manifest records the intended order; the remote receives branch refs and commit ancestry. Automated PR
submission is a separate follow-up.

## Interfaces

The agent uses publication and GitHub tools for PR operations. The TUI's publication commands and preview UI are removed. Existing publication records remain readable. Local imports keep their TUI
command.

Implementation: [server push endpoint](../rust/crates/server/src/push.rs), [agent publication](../std/llm-step/src/publish_source.rs), [GitHub worker](../std/github/src/main.rs), and [stack
tools](../std/llm-step/src/stack.rs).
