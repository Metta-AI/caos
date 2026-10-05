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
  content, so it needs no repository on either side. `std/github` sends it to
  the GitHub API.

### `std/github`

A std tool like any other: the agent runs it with
`run_tool(path="caos-std/github", arguments={...})`, and `tool_help` at that
path describes it. One run is one API call.

| Argument | Meaning |
| --- | --- |
| `method` | `GET`, `POST`, `PATCH`, `PUT` or `DELETE`. |
| `path` | Path under `https://api.github.com`, with any query string, e.g. `/repos/owner/repo/pulls?head=owner:feature&state=open`. GraphQL is `POST /graphql`. |
| `body` | Optional JSON request body. |
| `at` | Any value no earlier call used, such as the current time. |

The result is the HTTP status line, then the response body, cut if long. A
response of any status is a result; the run fails only when none came back.
The worker reaches `api.github.com` and nothing else, and follows no redirects,
so the token goes nowhere else.

The token is the `github-token` secret, granted to the tool by a second
`reader:` line beside llm-step's:

```text
reader:@@=git+https://github.com/Metta-AI/caos?ref=refs/heads/main&dir=std/github
```

Without it, only public reads work. Paths are not restricted, so the token's
scope is the boundary: with this tool the agent can do anything the token
allows, not only import and push. Grant a fine-grained token limited to the
repositories the agent works on, with contents and pull-request permissions.

It calls the API rather than `gh`, whose stack commands work on local branches
and would mean materializing a repository for every call. `gh stack link` with
PR numbers is the stacks endpoint below.

### `at`

A run's result is memoized by its ArgTree, while an API call's answer depends on
GitHub's state, so each call needs an argument of its own: `at` (`salt` is the
interpreter's). A call that repeats every argument, `at` included, gets the
stored result without reaching GitHub; a new `at` asks again.

So a write is not sent twice once its run has a result. A run that dies after
sending and before storing its result runs again when retried, so a write is
sent at least once, and possibly twice. GitHub refuses a second open PR with
the same head and base, and repeating a base change sets the same base. When a
write's run fails, the request may still have arrived: read the affected state
with a GET before sending it again.

### PRs for a stack

Once a stack is squashed and its layers pushed ([stacks.md](stacks.md),
"Publishing"), each layer gets a PR, bottom first, each call with a new `at`:

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

### Removing `/pr`

With `std/github` in place, `/pr`, `/publish-branch` and the client code behind
them go: the host's `git push` and `gh pr create`, the base and history fetched
into the client, its conflict checks, and the preview's offer to import the base
and ask the agent to integrate it, which `import_source` and `merge` already
cover (stacks.md, "Updating the base"). Publishing then needs neither `gh` nor
a local copy of the history on the client.

Nothing replaces the conflict checks. Resolving a conflict clears its
`.caos/conflicts` entry, and saving removes the emptied ledger, so a ledger
survives only while a conflict is unresolved. Publishing then is the same
mistake as publishing code that does not build, and nothing at publish time
looks for either.

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
