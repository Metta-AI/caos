# GitHub interactions

The agent imports code, edits source gitlinks, and publishes their commits as remote branches. It uses GitHub CLI for PRs, reviews, issues and comments. The TUI handles local imports and checkouts.

| Workflow | Design | Main tools |
| --- | --- | --- |
| Bring remote code into a conversation | [Importing remote source](agent-import.md), [server fetch and negotiation](git-import.md) | `import_source`, `caos import-git`, `POST /git/import` |
| Publish one source gitlink as a branch | [Publishing branches](agent-publish.md) | `publish_source`, `caos push-git`, `POST /git/push` |
| Update and publish related branches | [Stacks](agent-stacks.md) | `stack`, `push_stack` |
| Create PRs and interact with GitHub | [PRs and GitHub commands](agent-prs.md) | `github`, running `gh` and the pinned `gh stack` extension |

## How the pieces fit

A [conversation](chat.md) contains source gitlinks pointing to code commits. Importing attaches an existing commit at a new path. Editing that source advances its gitlink. Publishing sends its exact
commit and history from the server's Git store to a remote branch; it does not move the source gitlink.

A registered stack orders these source gitlinks and remembers the predecessor each layer was based on. The `stack` tool rebases or merges using Git objects, pausing conflicts in a separate draft that
the agent can edit and continue. `push_stack` publishes the layers' branches through the same server path as a single-branch push.

The `github` tool runs a worker containing GitHub CLI. It manages remote metadata after branches exist. It needs no source checkout for PR creation, review, or linking existing PR URLs. Source
restacking and branch pushes remain CAOS operations.

Stack publication is independent of PRs. It pushes branches and records their results; it does not create PRs or GitHub stack membership. The generic `github` tool can manage those separately.
Automatically submitting all PRs for a stack is a follow-up described in the [PR design](agent-prs.md#prs-for-a-stack).

## Credentials

Supply a GitHub token through the existing secret store when launching the TUI:

```text
# .caos-secrets/github-token
name=github-token
value:@=.github-token-value
reader=std/llm-step
reader=std/github
```

Put the token value in an ignored file and run `caos secrets` to initialize its entropy. `std/llm-step` uses the token for GitHub ref lookup and forwards it to the server for imports and pushes.
`std/github` passes it to `gh` as `GH_TOKEN`.

## Local work and recovery

Use `/checkout` to edit a source locally. Commit the edits with Git, then use `/import` to attach the result at a new conversation path. Ask the agent to integrate that imported commit into the
intended source. See [local editing](chat.md#viewing-files-and-working-locally) for the commands.

Imports pin the resolved commit for each call. Pushes pin the source commit and expected remote head before sending. GitHub commands record each invocation so a worker retry does not silently repeat a
write. The linked designs describe each recovery path and what the agent does when the remote outcome is uncertain.
