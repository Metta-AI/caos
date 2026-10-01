# GitHub interactions

Import source commits, prepare their history with a replay plan, and publish
the resulting branches. GitHub PRs describe how those branches should be reviewed.

## Importing

`import_source(source, revision?, into)` runs inline in `std/llm-step`.
It accepts an HTTPS repository and a branch, full ref, or full commit hash.
Omitting `revision` selects the default branch. `/import` handles local paths.

For each tool call:

1. Resolve the revision with `git ls-remote`, using the agent's existing Git
   binary. A full commit hash needs no lookup.
2. Save the chosen hash H and provenance in the conversation's `tool.start`
   payload. Resumed attempts reuse H; concurrent attempts use the first saved
   observation. A new call resolves the remote again.
3. Run `caos import-git <source> H`. This sends
   `POST /git/import {"source": "<https-url>", "commit": "H"}`.
4. After the server returns `{"commit": "H"}`, atomically attach the snapshot,
   its provenance, and the tool result:

   ```text
   imports/repo/main              gitlink -> H
   imports/repo/main.source.json  provenance
   ```

The destination and provenance path must both be unused. An import creates
an unchanged snapshot; it does not merge into or advance another source.
Provenance records the repository, requested revision, commit, observation
time, and default branch when known. For `origin/main`, choose the repository
from the selected source's provenance and import `main` at a fresh path.

The [server endpoint](git-import.md) fetches H and its full history into private
staging, verifies them, and publishes the complete pack into the server store. It uses verified complete imports as
negotiation tips; standalone trees and blobs may still be downloaded again.
A completion marker for the same URL and H skips fetch and verification.
The endpoint handles object availability; callers handle ref resolution and
conversation state.

Supply the token through the existing secret store:

```text
# .caos-secrets/github-token
name=github-token
value:@=.github-token-value
reader=std/llm-step
```

Keep the value file ignored and run `caos secrets` to initialize its entropy.
The agent uses `/secret/github-token` for GitHub ref lookup and passes
`--github-token-file=/secret/github-token` to `import-git`. The command
forwards it in the `X-Caos-Git-Token` header. The token authenticates HTTPS
requests to GitHub; public imports need no token.

## PRs

Add `std/github` with Git, `gh`, and a pinned
[`gh-stack` extension](https://github.com/github/gh-stack). Give it access to
the same `github-token` secret, exposed to `gh` as `GH_TOKEN`.

Expose a general `gh` operation accepting arguments, repository, stdin, and
input/output files. Return exit status, stdout, stderr, and requested files.
Use [branch publication](agent-publish.md) to push source commits directly
from the server, requiring the remote branch to match an expected head.

Create PRs with explicit repository, head, and base. Inspect existing PRs before
creating duplicates or replacing human-edited metadata. Conversation data and
merge bookkeeping stay outside published history.

### Stacks

CAOS represents a stack as numbered source gitlinks and recorded `.base` files.
A replay plan prepares its commits and rebuilds later layers after a predecessor
changes. See [agent stacks and replay](agent-rebase.md) for the representation,
plan commands, and conflict workflow.

Push each prepared tip to its remote branch. PR metadata is separate: the first
PR targets the base branch, and each later PR targets the preceding branch.
Link existing PR URLs in order with `gh stack link --base main <first-PR> <second-PR>`.
This requires no local branches. CAOS replay manages the commit history; GitHub
stack linking records the review relationships.

The ordinary `merge` tool remains available for a two-parent source merge.
Stack replay instead applies selected tree differences and keeps conflict drafts
under the feature's `rebase/` directory, outside the source tree.

### Retrying GitHub writes

Use the tool call's durable identity to claim an operation before executing it
and record its result afterwards. A duplicate attempt must not repeat a started
write. After a crash or partial success, inspect GitHub before continuing;
do not automatically retry arbitrary writes. Merge computation remains cached
by its inputs; draft edits and completion use conditional conversation updates.
