# Importing remote source

Part of [GitHub interactions](agent-github.md). This page describes the agent workflow; [git-import.md](git-import.md) describes the server endpoint and fetch negotiation.

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

Configure the [GitHub token](agent-github.md#credentials) for private repositories. The agent uses `/secret/github-token` for GitHub ref lookup and passes `--github-token-file=/secret/github-token` to
`import-git`. The command forwards it in the sensitive `X-Caos-Git-Token` header; the server does not look up the calling job's secrets.

Ref lookup and fetch share a repository-scoped Git credential helper. Tokens stay out of URLs, Git config, saved arguments, provenance, and logs. Automatic GitHub credentials apply only to
`github.com` on the default HTTPS port. Public imports need no token. Importing needs neither `gh` nor another worker.
