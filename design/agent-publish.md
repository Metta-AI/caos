# Branch publication

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

The endpoint performs one push:

1. Validate the HTTPS destination, branch and full hashes; require H to be a
   stored commit. Trust the complete history verified at ingestion and startup.
2. If E is non-null, require E to be a stored ancestor of H. If H contains E,
   CAOS already has E. A missing E or non-fast-forward is a rejection.
3. Push H to the destination branch with
   --force-with-lease=refs/heads/<branch>:<E>, disabling tag following.
   Empty E requires creation. The ancestry check prevents history rewrites;
   despite the Git flag's name, this API permits only creates and fast-forwards.
   There is no force-push option. Duplicate requests to the same branch are serialized.
4. Return complete, conflict, or uncertain, with a reason. Known validation and
   per-ref receiver rejections are definite failures. Unconfirmed transport
   failures are uncertain. The CLI preserves these results for llm-step.

The endpoint does not fetch, import, merge, rebase, resolve source policy, or
perform a follow-up remote lookup. Objects transfer directly from CAOS to the
destination through Git. Reuse import authentication: token-file option,
sensitive header and repository-scoped credential helper.

llm-step owns the workflow. It selects a source gitlink, resolves conflicts,
tests and inspects the diff, then reads its commit H and the remote head E.
Before sending, it records H, E, destination and branch under the tool-call
identity. Every attempt may send that same pinned intent; recovery never
substitutes a newer commit or refreshes the lease. Publishing leaves the
source gitlink unchanged.

On a lease conflict, llm-step can separately import the remote head, merge or
rebase, test, and make a new publication call. Other rejections carry their
reason through the CLI to the tool result. Importing and integration are
never hidden inside a push.

A remote can accept a push before the connection drops. Git's HTTP retry can
then report a stale lease. After a conflict or uncertain result, llm-step reads
the branch: H confirms completion. Otherwise it preserves a definite rejection;
for uncertainty, another value than E is a conflict, while E or a failed lookup
remains uncertain because a push may still be running. The endpoint itself does no recovery. A success receipt records the
original push even if the branch later advances.

Publication transfers the exact commit, including its tracked files. It does
not apply .gitignore or remove .caos content. Local Git staging respects
.gitignore for untracked files, and host path ingestion uses git ls-files.
Remote imports preserve their existing commit. The agent's bash tool, however,
stores every real file left in its working tree through caos put; that path
currently does not apply .gitignore. Ignore handling belongs in source capture
as a follow-up, before a commit is formed, not in publication.

Merge conflicts currently create a tracked .caos/conflicts ledger inside a
source tree, including conflicts without inline markers. llm-step must resolve
and clear it before publishing. The generic endpoint does not scan for that
ledger, inline markers, or conversation ancestry.
