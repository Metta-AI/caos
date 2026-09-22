# Agent stacks

Part of [GitHub interactions](agent-github.md). This page covers source history and branch publication; [PRs and GitHub stack membership](agent-prs.md#prs-for-a-stack) are separate.

A stack is an ordered list of source gitlinks. Git computes merges directly from objects; CAOS records the order and any paused operation. Publication pushes the stack's branches directly from the
server.

```text
feature/
  00-base        gitlink: imported mainline
  01-core        gitlink: first layer
  02-ui          gitlink: second layer
  stack.json     layer order and the predecessor each layer was based on
```

Register existing gitlinks with the `stack` tool:

```json
{"action":"create","path":"feature","base":"00-base","layers":["01-core","02-ui"]}
```

Registration checks ancestry. The source gitlinks remain the editable code pointers; the manifest remembers old predecessor commits so editing a lower layer does not change which commits belong to the
next layer.

## Updating the stack

Import the current remote base using `import_source`, then call:

```json
{"action":"rebase","path":"feature","onto":"imports/repo/main-2"}
```

For each layer, the tool replays its commits above the previously recorded predecessor onto the new lower tip. It uses `git merge-tree --write-tree --merge-base=<old-parent> <new-parent>
<old-commit>`, then creates a commit from the resulting tree. It preserves authors, messages and empty commits; rewritten signatures are removed. Inputs and commit metadata are fixed, so recovery
repeats the same computation.

Rebase currently requires single-parent history between each pair of boundaries. Use `"method":"merge"` for histories containing merge commits: the updated predecessor is merged into each layer in
order, preserving both histories. Neither operation checks out the source tree.

After editing a lower layer, use the same operation with `onto` naming the stack's existing base. Upper layers then include the exact updated lower tips.

The object database fetches commit history and loads trees and blobs as needed. New objects are uploaded dependency-first through the CAOS object API; existing objects stop traversal. There is no
archive-and-reingest step or push from a partial clone. Large merges can still read substantial content.

## Conflicts and continuation

Conflicts leave every original source gitlink unchanged. The operation adds:

```text
feature/restack/
  operation.json  pinned inputs, progress and Git's complete conflict report
  work            gitlink: proposed merge tree, editable with ordinary tools
```

Read the report and edit `feature/restack/work`. Stage object hashes can be read with the normal `read` tool's `root` argument. Resolve structural conflicts as well as text conflicts, and run the
relevant checks. Then call:

```json
{"action":"continue","path":"feature","resolved":true}
```

`resolved:true` explicitly acknowledges every conflict. It does not infer resolution from missing text markers. Continue takes the draft's resolved tree, creates the intended replacement commit and
proceeds until completion or another conflict. Draft editing commits never become parents in the finished stack. No `.caos/conflicts` is added to these source trees.

The operation survives turns and worker restarts. Completion checks the original manifest and source pointers, then replaces all stack gitlinks together. A concurrent edit causes a refusal, not an
overwrite. `{"action":"abort","path":"feature"}` removes the pending attempt and leaves the sources unchanged; the draft remains in conversation history. `status` reports the pointers and pending
operation.

## Pushing branches

`push_stack` takes `path`, an HTTPS Git `repository` URL, and optional `rewrite`:

```json
{"path":"feature","repository":"https://github.com/owner/repo.git"}
```

It pushes one branch per layer, bottom to top. Branch names are the source gitlink paths (`feature/01-core`, `feature/02-ui`); the base gitlink is not pushed. Each upper commit must contain the exact
lower tip. Finish pending conflicts and restack edited lower layers before pushing.

The tool records all source commits and observed remote heads before the first push. It uses the same `caos push-git` path as `publish_source`: the server transfers its stored objects, and an exact
expected-head lease prevents overwriting a concurrent branch update. No checkout or GitHub worker is needed.

After rebasing published history, pass `rewrite:true`. This allows non-fast-forward updates while retaining the lease and publication checks, including `.gitignore`. An unchanged push converges on the
existing branch heads. The remote mainline need not match the stack's base just to push work; importing and rebasing onto updated mainline is a separate decision.

Branch pushes are not one transaction. The tool records each result before proceeding, stops at the first failure or uncertain result, and returns the completed receipts and branches not attempted.
Recovery skips recorded successful pushes and retains the original commits and leases. Inspect an uncertain result before making a new call, which observes fresh remote heads.

Git receives branches and commit ancestry. The intended stack order stays in the conversation manifest. This operation creates no PRs or GitHub stack membership; those are a separate follow-up.

This implements linear restacking, merge propagation and branch publication. Interactive reorder/squash/drop and rebasing merge commits remain unsupported.
