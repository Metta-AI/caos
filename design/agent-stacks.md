# Agent stacks

A stack is an ordered list of source gitlinks. Git computes merges directly from objects; CAOS records the order and any paused operation. The GitHub worker manages PRs without a source checkout.

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
