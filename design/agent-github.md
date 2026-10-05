# Importing and PRs

Imports use three layers: the server endpoint, `caos import-git`, and the
agent's `import_source` tool. PRs use `publish_source` and `std/github`.
Merge drafts come later.

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
reader:@@=git+https://github.com/Metta-AI/caos?ref=refs/heads/main&dir=std/github
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

A PR is a pushed branch plus metadata. `publish_source` pushes the commit from
the server's store ([agent-publish.md](agent-publish.md)); the std tool
`std/github` sends everything else (opening a PR, changing its base, linking a
stack) to the GitHub API. Neither checks anything out, which `gh` would: its
stack commands work on local branches.

### `std/github`

One run, `run_tool(path="caos-std/github")`, is one API call: `method`, `path`
under `https://api.github.com`, and an optional JSON `body`. The result is
the status line and the body. A run fails only when no response came back, and
a write may still have arrived; read the state with a GET before resending.

Runs are memoized by their arguments, and the tool declares `@call`, so the
harness adds the tool call's id to them (SPEC, "CaosTools"). Each call is then
its own run, while a recovered call keeps its id and gets its stored result, so
only a run that died mid-flight sends a write twice. GitHub refuses a duplicate
PR, and a repeated base change is a no-op.

The token is the same `github-token` secret imports use, not a new one; its
second `reader:` line, under Importing, grants it to `std/github`. The tool
reaches only `api.github.com` and follows no redirects, but paths are
unrestricted: the token's scope is the boundary, so grant a fine-grained token
for the repositories the agent works on.

### PRs for a stack

After squashing and pushing the stack ([stacks.md](stacks.md), "Publishing"),
for each layer, bottom first:

1. `GET /repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=open`.
2. If none, `POST /repos/{owner}/{repo}/pulls` with `head`, `base` (the trunk,
   or the branch below) and a `title` and `body` from the layer's `message=`
   lines. If one, `PATCH` its `base` when it differs, and leave its title and
   body, which a person may have edited.

Then link them with `POST /repos/{owner}/{repo}/stacks` and
`{"pull_requests": [<numbers, bottom first>]}`, or `.../stacks/{number}/add` to
extend a stack. The chained bases already make a reviewable stack without it.

### Removing `/pr`

`/pr`, `/publish-branch` and the client code behind them go: the host's
`git push` and `gh pr create`, the fetched base and history, the preview, and
its conflict checks. Nothing replaces the checks: resolving a conflict clears
its ledger entry and saving removes the emptied ledger, so a ledger left in a
published commit is an unresolved conflict, like code that does not build. No
person confirms a GitHub write; the agent makes it, as it already pushes.

## Merges

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

Merge computation remains cached by its inputs; draft edits and completion use
conditional conversation updates.
