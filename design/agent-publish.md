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
