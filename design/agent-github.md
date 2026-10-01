# Importing and PRs

Imports use three layers: the server endpoint, `caos import-git`, and the
agent's `import_source` tool. PR publication and merge drafts come later.

## Importing

`import_source(source, revision?, into)` runs inline in `std/llm-step`.
It accepts an HTTPS repository and a branch, full ref, or full commit hash.
Omitting `revision` selects the default branch. Keep `/import` for local paths.

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

Supply the token through your secret store (SPEC.md, "Secrets"):

```text
# <your secrets directory>/github-token
value:@=values/github-token
reader:@@=git+https://github.com/Metta-AI/caos?ref=refs/heads/main&dir=std/llm-step
```

`caos-cli secrets-push --dir=<d>` adds its entropy and sends it to the server.
The agent uses `/secret/github-token` for GitHub ref lookup and passes
`--github-token-file=/secret/github-token` to `import-git`. The command
forwards it in the sensitive `X-Caos-Git-Token` header; the server does not look
up the calling job's secrets.

Ref lookup and fetch share a repository-scoped Git credential helper. Tokens
stay out of URLs, Git config, saved arguments, provenance, and logs. Automatic
GitHub credentials apply only to `github.com` on the default HTTPS port.
Public imports need no token. Importing needs neither `gh` nor another worker.

## PRs

Add `std/github` with Git, `gh`, and a pinned
[`gh-stack` extension](https://github.com/github/gh-stack). Give it access to
the same `github-token` secret, exposed to `gh` as `GH_TOKEN`.

Expose a general `gh` operation accepting arguments, repository, stdin, and
input/output files. Return exit status, stdout, stderr, and requested files.
Use a small `git_push` helper to publish a selected source commit and its history,
requiring the remote branch to match an expected head. The server can later
perform this transfer directly, as it does imports.

Create PRs with explicit repository, head, and base. Inspect existing PRs before
creating duplicates or replacing human-edited metadata. Conversation data and
merge bookkeeping stay outside published history. Once agent publication works,
remove `/pr`, `/publish-branch`, and their UI.

### Stacks

Keep each stack boundary as a source gitlink:

```text
imports/repo/base   -> H
feature/01-core     -> A   parent H
feature/02-tests    -> B   parent A
```

Start each layer by copying the preceding snapshot with `cp -a`, then editing
the copy. Git ancestry records the dependency. If A changes, merge its new
commit into B and test; the layers keep that full merge history.

#### Publishing a stack

Reviewers see one commit per layer, so the stack is collapsed before it is
pushed. The layers themselves are never rewritten:

1. Run `caos-std/collapse-stack` with `stack=feature`, `onto=H` (the base
   branch's tip that layer 1 last merged) and one `<entry> <message>` line per
   layer, bottom first. For each layer it mints
   `C_i = commit(tree(layer i tip), parent C_{i-1} or H, message_i)`, with the
   author and committer of layer i's tip, and prints `<layer> <C_i>`. It refuses,
   naming both, a layer that does not contain the one below it (or H): merge
   first. Identical input mints identical commits, so republishing an unchanged
   stack pushes nothing new.
2. Link each `C_i` into the conversation (`caos get-hash C_i /cas/c; ln -s
   /cas/c publish/<layer>`), since `publish_source` publishes a gitlink.
3. `publish_source` each, bottom to top, with `rewrite=true`. A collapsed
   commit does not descend from the one it replaces, so this is the one case for
   a rewrite; the push stays leased on the exact remote head `publish_source`
   observes, so a change someone else pushed is never overwritten.
4. The first PR targets `main`; each later one targets the branch below it.

```text
feature/01-core  -> A'  (merge history)   C1 = tree(A'), parent H    -> feature/01-core
feature/02-tests -> B'  (merge history)   C2 = tree(B'), parent C1   -> feature/02-tests
```

Link the existing PR URLs in order with
[`gh stack link`](https://docs.github.com/en/pull-requests/reference/stacked-prs-cli-commands#gh-stack-link):

```sh
GH_REPO=owner/repo gh stack link --base main \
  https://github.com/owner/repo/pull/123 \
  https://github.com/owner/repo/pull/124
```

This needs no local stack branches. Commands such as `push`, `submit`, and
`rebase` do require local branches and stack metadata. Supporting them would
mean reconstructing that local repository from gitlinks and returning any
rewritten commits to CAOS. Use CAOS's copy, edit, and merge operations initially,
and `link` to publish the relationship. `modify` additionally requires linear
history, so it cannot restructure stacks containing merge commits.

### Merges

Clean merges use the existing merge worker. On conflict, preserve the source O
and keep the attempt beside it in the conversation:

```text
feature/01-core            gitlink -> O
merges/update-main/
  ours                    gitlink -> O
  theirs                  gitlink -> T
  work                    gitlink -> D
  conflicts               Git's complete conflict report
```

D starts with Git's proposed merged tree and O as its single parent. The agent
edits this separate draft gitlink and tests it. The report stays outside the
code tree; newly created sources and drafts contain no `.caos/conflicts`.

`finish_merge(attempt="merges/update-main", target="feature/01-core")` takes
the draft's current tree R and creates `M = commit(tree=R, parents=[O,T])`.
Verify O and T against the attempt's creation record, recheck the draft, and
advance the target only if it still points to O. Otherwise retain the result
for reconciliation. Draft commits stay in conversation history, outside M's
ancestry. Test the final source before publication.

Finishing explicitly asserts resolution. Preserve Git's complete conflict
report, including messages and stage objects; deleting markers or report rows
is not proof that structural conflicts are resolved. Delegate by copying the
whole attempt with `cp -a`, harvesting the edited draft, and then finishing.
Abandoning an attempt leaves the source unchanged. Handle old source-tree
conflict ledgers before removing their compatibility cleanup.

### Retrying GitHub writes

Use the tool call's durable identity to claim an operation before executing it
and record its result afterwards. A duplicate attempt must not repeat a started
write. After a crash or partial success, inspect GitHub before continuing;
do not automatically retry arbitrary writes. Merge computation remains cached
by its inputs; draft edits and completion use conditional conversation updates.
