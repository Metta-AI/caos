# Pull requests and GitHub commands

Part of [GitHub interactions](agent-github.md). CAOS publishes branches from the server; the `github` tool runs GitHub CLI to create and update PRs, read reviews, and work with issues and comments.

## GitHub worker

`std/github` contains Git, `gh`, and the pinned `github/gh-stack` v0.1.1 extension. It is registered as a built-in tool, available without a project-defined `caos-tools` entry.

The tool is `github(repository, args, stdin?)`. `args` is an argument array passed directly to `gh`; it is never interpolated into a shell command. Return exit status, stdout and stderr through
ordinary tool results. This covers issues, comments, PR creation/editing and stack operations without separate wrappers for each GitHub action. Initially use stdin for bodies (`--body-file -`);
commands needing local file attachments can be added when needed.

The worker sets `GH_REPO` explicitly and reads `GH_TOKEN` from its granted `/secret/github-token`. Use the [shared token setup](agent-github.md#credentials), granting it to both `std/github` and
`std/llm-step`. The latter uses it for import/push and includes that identity in the model turn’s cache key. The agent carries the GitHub worker source and evaluates it when called, so secret marking
of the child worker does not alter the agent’s own reader identity. Use an isolated temporary GitHub configuration and disable prompts. Install the extension in the image. The worker needs no source
checkout or local branches for the operations below.

## Invocation recovery

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

## PR workflow

For a single PR:

1. Integrate required updates and test the chosen source gitlink.
2. Call [`publish_source`](agent-publish.md) with its path, repository and remote branch. Continue
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

## PRs for a stack

[Stack publication](agent-stacks.md#pushing-branches) produces ordinary remote branches. For example, a two-layer stack based on remote `main` maps to:

| Source gitlink | PR head branch | PR base branch |
| --- | --- | --- |
| `feature/01-core` | `feature/01-core` | `main` |
| `feature/02-ui` | `feature/02-ui` | `feature/01-core` |

The base gitlink has no PR. The first PR targets the remote integration branch; each later PR targets the preceding layer's branch. PR numbers are assigned by GitHub and can be discovered from the
repository and branch names. Keep the returned URLs in the conversation for later review and updates.

The agent can perform these steps with the generic `github` tool after a confirmed `push_stack`. There is no dedicated operation that creates or repairs all PRs for a stack. That automation remains a
follow-up.

GitHub stack membership is also separate from branch publication. Once PRs exist, the worker can pass their URLs to `gh stack link`. Using PR URLs avoids the extension's local-branch push workflow;
CAOS remains responsible for moving the branch refs. The source gitlinks and `stack.json` remain the inputs to CAOS restacking, not GitHub's PR metadata.

Implementation: [GitHub worker](../std/github/src/main.rs) and [agent tool](../std/llm-step/src/github.rs).
