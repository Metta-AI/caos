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

## Submitting and resubmitting

`submit_stack` takes `path`, a GitHub `repository` (`owner/repo`), and `base_branch`. The registered base must match the observed remote mainline, and each layer must contain the exact lower tip.

The agent pins all source commits and remote-head leases before dispatching the GitHub worker. The worker then:

1. Calls `caos push-git` for each branch. The server pushes its stored objects directly; branch names are the source gitlink paths.
2. Finds an open PR by branch and repository owner. If absent, creates one with an explicit head and base. If present, preserves its title/body and corrects its base when needed.
3. Creates or extends GitHub stack membership through `gh api`, preserving already-merged entries. Existing unmerged members must remain in the same order.

New PRs are ready for review unless `draft:true` is requested. Optional `descriptions` supplies one `{title,body}` per layer for new PRs; otherwise titles use commit subjects.

Resubmission finds the same PRs. After rebasing published history, pass `rewrite:true`. This enables non-fast-forward updates but retains the exact remote-head lease and the server's publication
checks, including `.gitignore`. Default publication remains fast-forward only.

Branch pushes and GitHub changes are not one transaction. Results report completed branch pushes and PR URLs before a failure. An unfinished invocation is uncertain and is not blindly repeated;
inspect remote state before a new submission. A new call observes branches and PRs again and can finish partial progress.

This implements linear restacking, merge propagation and PR submission. It does not implement interactive reorder/squash/drop, rebasing merge commits, or automatically removing/reordering existing
GitHub stack membership. Use the general GitHub tool for review, comments and landing.
