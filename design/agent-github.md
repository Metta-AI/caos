# Importing and publishing

Imports are implemented through the server endpoint, `caos import-git`, and
the agent's `import_source` tool. Publication proceeds through branches, PRs
and stacks. Separate merge drafts remain a follow-up.

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

Supply the token through the existing secret store:

```text
# .caos-secrets/github-token
name=github-token
value:@=.github-token-value
reader=std/llm-step
reader=std/github
```

Keep the value file ignored and run `caos secrets` to initialize its entropy.
The agent uses `/secret/github-token` for GitHub ref lookup and passes
`--github-token-file=/secret/github-token` to `import-git`. The command
forwards it in the sensitive `X-Caos-Git-Token` header; the server does not look
up the calling job's secrets.

Ref lookup and fetch share a repository-scoped Git credential helper. Tokens
stay out of URLs, Git config, saved arguments, provenance, and logs. Automatic
GitHub credentials apply only to `github.com` on the default HTTPS port.
Public imports need no token. Importing needs neither `gh` nor another worker.

## Publishing

The [publication design](agent-publish.md) separates the remaining work:

- **Branches:** `POST /git/push`, `caos push-git` and `publish_source` push
  exact code commits from the server with a lease. These are implemented in
  [#247](https://github.com/Metta-AI/caos/pull/247).
- **PRs:** that PR also supplies the general `github` tool and `std/github`
  worker. Next, validate the agent's create, update and review workflow using
  `gh`, with explicit repositories and branch names.
- **Stacks:** keep code boundaries in gitlinks, publish bottom to top, and
  use `gh stack link` with existing PR URLs. Validate linking, propagation
  and landing without a source checkout in the GitHub worker.

The TUI's `/pr` and `/publish-branch` commands are removed. The follow-ups
start with workflow instructions and integration tests using these tools.

### Merges

This section is a follow-up proposal. Current merges still use source-tree
conflict ledgers and the publication guards described in SPEC.md.

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
