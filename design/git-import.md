# Git import endpoint

`POST /git/import` makes an exact remote commit and its complete ancestor
history readable through CAOS's object API. Git fetches into a private staging
repository on the server, avoiding a worker checkout and re-upload. Only
verified objects enter the shared object store.

## Request and execution

The JSON body is `{"source": "<https-url>", "commit": "<full-40-character-hash>"}`.
An optional `X-Caos-Git-Token` header carries credentials. The caller resolves
branch names and pins the chosen commit H before requesting an import.
Conversation attachment and provenance also belong to the caller.

The server:

1. Validates the URL, hash and token format.
2. Locks `$GIT_DIR/caos-imports/<sha256(source)>/lock`, serializing imports
   from that URL across server processes sharing the object store.
3. Returns `{"commit": H}` immediately if `H.complete` exists in that directory.
4. Fetches H into a private bare repository under that directory. Its object
   store reads existing server objects through an alternate, but all writes
   stay private. The fetch requests H's ancestors, trees and blobs. Tags,
   recursive submodule fetching, and partial-clone filters are disabled.
5. Rejects incomplete ancestor history, verifies H is a commit, and walks H
   plus every received object as roots. It reads their complete closure through
   the object-storage code, including surplus objects unrelated to H. Git's
   pack checks alone can exempt parents of remote-declared shallow commits.
6. Publishes the verified pack, then its index, and flushes both. Readers see
   the new objects together. The live object API must be able to read H.
7. Writes and flushes `H.complete`, then returns `{"commit": H}`.

The private repository also isolates shallow boundaries. A failed fetch or
verification leaves no new objects visible in the shared store and writes no
completion marker; the same request can be retried. Verification currently
walks the full requested history on every new completion, including overlaps
with earlier imports.

The token is passed to Git through a child-process environment and a credential
helper scoped to the requested HTTPS URL. It is not saved in Git objects,
repository config or completion markers. Redirects and interactive credential
prompts are disabled.

## What fetch negotiation reuses

Normal `git fetch` reports **commit OIDs**, rather than listing every tree and
blob OID in the local store. When both sides have commit A, the sender can
exclude the history, trees and blobs reachable from A. This is how a later
import can reuse an earlier import's content without transferring it again.
See Git's [fetch negotiation options](https://git-scm.com/docs/git-fetch) and
[packfile negotiation](https://git-scm.com/docs/pack-protocol#_packfile_negotiation).

CAOS may contain commits whose parents, trees or blobs are missing. Advertising
one as complete could cause the remote to omit objects we still need. The
endpoint therefore supplies `--negotiation-tip=<commit>` only for completed
imports from the same source URL. With no completed imports for that URL, it
sets `fetch.negotiationAlgorithm=noop` and skips negotiation.

| Already present in CAOS | Effect on this import |
| --- | --- |
| Completion marker for the same URL and H | Skip fetch and verification entirely. |
| A prior complete import from that URL | Negotiate from its commit history to reduce the transfer. |
| Standalone trees or blobs outside that history | They are not advertised individually and may be downloaded again. |
| An import through a different URL, including a fork or alias | Its completion markers are not used as negotiation tips for this URL. |

For example, importing A and then its child B from the same URL lets Git omit
content covered by A. Merely having many of B's file blobs in CAOS does not
provide the same negotiation shortcut. The implementation reuses known Git
history; it does not guarantee avoiding every OID already in the store.

## Completion markers and access

A marker certifies that the requested history was verified as readable.
It is not a Git ref, GC root, permission record or invocation record. Automatic
GC is disabled; introducing GC will require explicit retention of imports.

A cache hit validates request syntax but does not recheck GitHub access.
Imported hashes remain readable through the object API without remote
credentials, like other objects already in the store.

Implementation: [endpoint](../rust/crates/server/src/import.rs),
[Git command and credential setup](../rust/crates/git-locator/src/import.rs).
Caller behavior: [agent imports](agent-github.md#importing).
