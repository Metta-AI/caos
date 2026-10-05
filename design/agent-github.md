# Importing and PRs

Imports use three layers: the server endpoint, `caos import-git`, and the
agent's `import_source` tool. PRs use `publish_source` and the agent's `github`
tool. Merge drafts come later.

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

A PR reaches GitHub in two halves, and neither checks anything out:

- **Commits** go with `publish_source` ([agent-publish.md](agent-publish.md)):
  the server pushes the exact commit from its bare store.
- **Everything else** is metadata: opening a PR, retargeting its base, linking
  a stack, commenting. It names branches, PR numbers and text, never file
  content, so it needs no repository on either side. The agent's `github` tool
  sends it to the GitHub API.

### The `github` tool

`github(method, path, body?)` runs inline in `std/llm-step`, as
`import_source` and `publish_source` do.

| Parameter | Meaning |
| --- | --- |
| `method` | `GET`, `POST`, `PATCH`, `PUT` or `DELETE`. |
| `path` | Path under `https://api.github.com`, with any query string, e.g. `/repos/owner/repo/pulls?head=owner:feature&state=open`. GraphQL is `POST /graphql`. |
| `body` | Optional JSON request body. |

It returns the HTTP status and the response body, truncated if long; a status
outside 2xx is an error result. Requests carry the `github-token` secret that
llm-step is already granted for imports, as `Authorization: Bearer`. Without
it, only public reads work. The host is fixed: the tool reaches
`api.github.com` and nothing else, and follows no redirects, so the token never
goes to another host.

Paths are not restricted, so the token's scope is the boundary: with this tool
the agent can do anything the token allows, not only import and push. Grant a
fine-grained token limited to the repositories the agent works on, with the
permissions it needs (contents and pull requests).

It runs inline rather than as a `std/github` worker running `gh`:

- A worker's result is memoized by its ArgTree. A GitHub call depends on remote
  state and may write, so its record belongs in the conversation, under the
  tool call, where `publish_source` keeps its own.
- The secret's `reader:` names `std/llm-step`. A separate worker would need its
  own grant in every user's secret file.
- `gh` adds nothing the API lacks. With PR numbers, `gh stack link` is the
  stacks endpoint below (gh-stack's `internal/github/github.go`). The other
  [gh-stack](https://github.com/github/gh-stack) commands (`push`, `submit`,
  `rebase`) need local branches and stack metadata, which would mean rebuilding
  a repository from gitlinks and bringing rewritten commits back into caos;
  `modify` also needs linear history, which a stack with merge commits lacks.
  `create-squashed-stack` and `publish_source` do that job without either.

### Writes

A `GET` is a read: it runs, and a retry runs it again. Every other method is a
write, including a GraphQL query, which is a POST.

Before sending a write, the tool pins it in a `tool.start` whose payload is the
exact request. Only the attempt that appended that pin sends the request. An
attempt that finds the call already started and unfinished never sends it
again; this covers a step resumed after a crash and any other writer. It
completes the call as uncertain, and the agent inspects GitHub before doing
anything else. A response of any status is a definite outcome. A transport
failure is uncertain, since the request may have arrived.

### PRs for a stack

Once a stack is squashed and its layers pushed ([stacks.md](stacks.md),
"Publishing"), each layer gets a PR, bottom first:

1. Find the layer's open PR:
   `GET /repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=open`.
2. If there is none, `POST /repos/{owner}/{repo}/pulls` with `head`, `base`, and
   a `title` and `body` taken from the layer's `message=` lines in the plan. The
   first layer's base is the trunk; each later layer's base is the branch below
   it. If there is one, `PATCH /repos/{owner}/{repo}/pulls/{number}` with
   `base` when it differs, and leave its title and body alone: a person may have
   edited them.
3. Link them: `POST /repos/{owner}/{repo}/stacks` with
   `{"pull_requests": [<numbers, bottom first>]}`, or
   `POST /repos/{owner}/{repo}/stacks/{number}/add` to extend an existing
   stack. Without stacks, the chained bases alone still make a reviewable stack.

Republishing an unchanged stack mints the same commits, so the pushes change
nothing, each PR is found by its branch, and only a changed base is patched.

### Publication checks

`publish_source` refuses a commit whose tree has a `.caos` entry, such as an
uncleared `.caos/conflicts` ledger, by looking up the pinned commit's tree.
`/pr` checked this before pushing, and a squash carries a ledger into its
commit, so a squashed layer needs the check too.

### Removing `/pr`

With the tool in place, `/pr`, `/publish-branch` and the client code behind
them go: the host's `git push` and `gh pr create`, the base and history fetched
into the client, the merge-marker `git grep`, and the preview's offer to import
the base and ask the agent to integrate it, which `import_source` and `merge`
already cover (stacks.md, "Updating the base"). Publishing then needs neither
`gh` nor a local copy of the history on the client.

One behavior changes: no person confirms a GitHub write. The agent makes it,
as it already makes pushes with `publish_source`.

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
