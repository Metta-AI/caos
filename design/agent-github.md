# Importing and publishing

Imports are implemented through the server endpoint, `caos import-git`, and the agent's `import_source` tool. Publication proceeds through branches, PRs and stacks. Stack operations keep merge drafts
outside source history.

## Importing

`import_source(source, revision?, into)` runs inline in `std/llm-step`. It accepts an HTTPS repository and a branch, full ref, or full commit hash. Omitting `revision` selects the default branch. Keep
`/import` for local paths.

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

The destination and provenance path must both be unused. An import creates an unchanged snapshot; it does not merge into or advance another source. Provenance records the repository, requested
revision, commit, observation time, and default branch when known. For `origin/main`, choose the repository from the selected source's provenance and import `main` at a fresh path.

The [server endpoint](git-import.md) fetches H and its full history into private staging, verifies them, and publishes the complete pack into the server store. It uses verified complete imports as
negotiation tips; standalone trees and blobs may still be downloaded again. A completion marker for the same URL and H skips fetch and verification. The endpoint handles object availability; callers
handle ref resolution and conversation state.

Supply the token through the existing secret store:

```text
# .caos-secrets/github-token
name=github-token
value:@=.github-token-value
reader=std/llm-step
reader=std/github
```

Keep the value file ignored and run `caos secrets` to initialize its entropy. The agent uses `/secret/github-token` for GitHub ref lookup and passes `--github-token-file=/secret/github-token` to
`import-git`. The command forwards it in the sensitive `X-Caos-Git-Token` header; the server does not look up the calling job's secrets.

Ref lookup and fetch share a repository-scoped Git credential helper. Tokens stay out of URLs, Git config, saved arguments, provenance, and logs. Automatic GitHub credentials apply only to
`github.com` on the default HTTPS port. Public imports need no token. Importing needs neither `gh` nor another worker.

## Publishing

[Branch publication](agent-publish.md) uses POST /git/push, caos push-git and publish_source to push exact commits directly from the server with an expected-head lease. The github tool runs gh with an
explicit repository for PRs, issues, review and stack metadata.

[Stack operations](agent-stacks.md) register ordered source gitlinks, remember each layer's predecessor, and merge or rebase them using Git objects. Conflicts pause with a separate draft gitlink and
report. The agent edits the draft and explicitly continues; finished source history contains no .caos/conflicts or draft editing commits.

submit_stack publishes the registered layers, finds or creates their PRs, corrects bases and links them through GitHub’s stack API. The GitHub worker needs no source checkout. Resubmission reuses PRs
by branch and preserves their descriptions.

The TUI's /pr and /publish-branch commands are removed. /import remains for local paths. Older non-stack merge operations still use the legacy source-tree conflict ledger.
