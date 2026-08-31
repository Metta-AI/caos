# `caos tui`

`caos tui` is a full-screen terminal client for the CAOS agent harness. It uses
the same conversation engine as `caos talk`, while keeping terminal UI
dependencies out of the worker-side `caos` binary.

The interface keeps independent virtual conversations in a left sidebar. Each
entry has a stable task title and a second row reserved for live operation or
attention status. Idle conversations do not show a stripped message preview.
`Ctrl+N` only allocates local state; the first submitted message atomically
publishes the conversation head, fallback title, and active membership. The
first message also runs one separate, stateless `llm-call` title job concurrently
with the agent turn. The result uses
the existing durable title metadata, so reopening the TUI does not regenerate
it or require any additional refs. Text ends with a visible ellipsis instead of
hard terminal clipping, and internal conversation IDs stay hidden. Each
conversation has its own durable history, multiline prompt, live activity,
completed-turn hashes, and workspace diff.
Turns continue running when another conversation is selected, so several agent
workspaces can advance concurrently without touching the working checkout.
Agents may use `spawn_agent` to create an indexed child conversation. It runs
through `run_async`; applying its result is an ordinary `merge`. Child rows use
their prompt title and appear beneath the parent conversation.

## Build and run

Run the client from a Git working tree whose `caos` remote points at a running
CAOS server:

```bash
git remote add caos http://localhost:9090
nix build
./result/bin/caos tui
```

During development, launch it with
`cargo run -p caos-cli --bin caos-cli -- tui`.
The TUI checks the configured server before entering the alternate screen. If
it cannot connect within five seconds, it exits with the server URL and asks
you to check the running service and the `caos` git remote.

The Anthropic API key is checked next, still at the shell prompt. When the
git-ignored `.caos-secrets` store has no `anthropic-api-key` secret, the TUI
asks for one — paste the key, or enter the path to a file that holds it — and
writes the canonical secret entry, trimmed, with fresh cache-isolation entropy
already included (what `caos secrets` would add). It ensures git ignores
`.caos-secrets/` (adding the rule to `.git/info/exclude` when nothing else
covers it), re-reads the entry through the same store parser every turn starts
from, and continues straight into the UI — no relaunch. A pasted key is erased
from the screen the moment it is submitted. A store that exists but fails to
read is reported as the error it is rather than prompting, so an existing
broken configuration is never overwritten. The check is parse-only: the
secret's `reader=` expressions are resolved by the first turn, behind the
TUI's progress display, never while the terminal is still blank at startup.

```text
caos tui                  continue the most recent conversation
caos tui --username alice use alice's active conversation list
caos tui --new            start a fresh conversation
caos tui --from 5ec3751   branch from a completed turn
caos tui --list-archived  list archived conversation IDs and titles
caos tui --unarchive ID   restore one conversation to the active list
```

`--username` defaults to `$USER`. If `$USER` is a shared container account such
as `root` or `ubuntu`, pass a personal `--username`; persisted identity is future
work. Active and archived membership is stored on the
CAOS server under `refs/caos/v2/users/<user-key>/conversations/{active,archived}/`,
not in local TUI state. `<user-key>` is `u-` plus lowercase hex of the
normalized username's UTF-8 bytes; usernames are limited to 126 UTF-8 bytes.
Each membership ref ends in one `<conversation-key>` component: `c-` plus
lowercase hex of the conversation ID's UTF-8 bytes. Conversation IDs are
limited to 124 UTF-8 bytes so Git can create the encoded ref lockfile. This
preserves IDs containing `/` without creating Git ref file/directory
collisions. Only these v2 membership refs populate the sidebar. Unversioned
chat refs remain stored but invisible: v2 clients do not read, import, rename,
migrate, or delete them.

## Controls

The left pane lists conversations; keyboard input goes to the conversation
pane. Change the selected conversation from anywhere with `Ctrl+Up` /
`Ctrl+Down`, jump straight to one with `Ctrl+1` … `Ctrl+9`, or click its row
in the sidebar. `Escape` stops a running turn or dismisses the current layer,
so it never leaves the conversation pane.

| Input | Action |
|---|---|
| `Escape` | Stop a running turn, else dismiss the current layer |
| `Ctrl+A` / `Ctrl+E` | Move to the start / end of the current line |
| `Ctrl+S` | Send the prompt (`Ctrl+Enter` also works in terminals with enhanced keyboard input) |
| `Enter` or `Ctrl+J` | Insert a newline |
| `Tab` | Complete the selected slash command |
| `Up` / `Down` | Select a visible slash-command match |
| `Alt+Left` / `Alt+Right`, `Ctrl+Left` / `Ctrl+Right`, or `Alt+B` / `Alt+F` | Move by whitespace-delimited words |
| `Alt+Backspace` / `Alt+Delete` | Delete the previous or next word |
| `Ctrl+W` | Delete the previous word |
| `Ctrl+K` | Kill from the cursor to the end of the line |
| `Ctrl+D` | Delete the character to the right of the cursor |
| `Ctrl+Up` / `Ctrl+Down` | Select the previous or next conversation |
| `Ctrl+1` … `Ctrl+9` | Select the Nth conversation (terminals with enhanced keyboard input) |
| `Ctrl+N` | Start a new virtual conversation and select it |
| `Ctrl+H` | Enter or leave keyboard help |
| `Ctrl+Shift+P` | Open or close the searchable command palette |
| `Ctrl+Q` | Switch between conversation and workspace changes |
| `Ctrl+T` | Enter or leave the Activity browser |
| `Ctrl+Shift+T` | Show the tools available to the selected conversation |
| `Up` / `Down` in Activity | Select the previous or next activity entry |
| `PageUp` / `PageDown` in Activity | Scroll the selected activity's full details |
| `Escape` in Activity | Return to the conversation |
| `PageUp` / `PageDown` in conversation | Scroll by rendered rows |
| Mouse wheel over the transcript | Scroll the conversation by rendered rows |
| Mouse wheel over Activity | Scroll the selected activity's full details |
| Mouse drag over rendered text | Select and copy text anywhere in the interface |
| `Ctrl+Y` | Release mouse capture and freeze redraws for native selection |
| `Ctrl+L` | Check out the selected conversation's head commit in the working tree |
| `Ctrl+P` twice | Choose a base, push a clean branch, and open a PR |
| `Ctrl+R` | Reload completed conversation history |
| `Ctrl+C` | Clear a non-empty prompt; exit when the prompt is empty |

Failures from local UI commands are shown in a temporary red command-error
panel instead of being inserted into the conversation transcript. A
successfully opened PR is appended as a cyan `CAOS` entry so its URL remains
available. Routine operation status is shown only while the operation is
running and is not added to the transcript or title.

Completed user and agent turns show branchable hashes in the transcript. Enter
`/from <turn-hash>` to start a fresh conversation from one without leaving the
TUI. Enter `/title <new title>` to change the shared title without changing the
conversation ID or HEAD. Enter `/model <name>` to select the client-wide model
for later turns; known model names type ahead. `/model default` restores the
client default. Enter `/update-tree <message>` to send an ordinary
user turn whose commit also folds in your current working-tree changes — the
intended companion to `Ctrl+L` (check out the head, edit files, then
`/update-tree <message>` with the text you want in that turn). Activity entries
show the durable hashes of internal harness steps for inspection; those step
trees contain harness metadata and are not branch points.

Enter `/publish-branch` to push the selected conversation's complete event
history to `origin/caos/<conversation-id>` without opening a PR. Enter
`/load origin/caos/<conversation-id>` to import one of those branches into the
current CAOS server and sidebar, or pass a GitHub PR URL such as
`/load https://github.com/Metta-AI/caos/pull/34`. PR loading uses `gh` to
recover the head branch's conversation ID and GitHub's pull ref to support PRs
from forks. An import preserves the conversation ID and exact first-parent
event spine. It may advance an existing copy of that conversation, but refuses
divergent history with the same ID.

Conversation text renders `**bold**` and `_italic_` emphasis. Unmatched markers
remain visible, and marker-like text inside inline backticks is left literal.

A fresh conversation starts with a temporary `talk-N` title. Its first prompt
provides an immediate fallback title and starts a stateless `llm-call` job using
that message alone. Title generation runs concurrently with the agent turn, so
it does not depend on the turn succeeding. Failure leaves the fallback in
place, and later messages make no title calls. Using `/title` before the first
prompt keeps that explicit title instead.

Fresh conversations start from the fetched tip of `origin`'s advertised
default branch. `--base` and `/from <turn-hash>` override that default.

Typing `/` at the start of the prompt shows matching slash commands and their
usage. Matches are case-sensitive. Use Up and Down to choose a match, then Tab
or Enter to complete it with a trailing space. Typing arguments closes the
menu. Escape dismisses it without changing the prompt. An unrecognized
slash-prefixed prompt is sent normally.

`Ctrl+Shift+P` or `/commands` opens a searchable command palette without
changing the current draft. Type any words from an action, use Up and Down to
choose a match, then press Enter to run it. The palette covers conversation,
workspace, publishing, activity, tool, help, reload, archive, and selection
actions. Escape closes it.

Bracketed paste mode keeps pasted newlines inside the prompt instead of
submitting partial lines. Pastes over 1,000 characters are kept out of the
editable buffer and shown as an atomic `[Pasted text: N chars]` placeholder.
The full text, including newlines, replaces the placeholder when the prompt is
sent. Backspace or Delete removes the whole placeholder. Press `Ctrl+C` to
clear a draft and any stored paste content; press it again on the empty prompt
to exit.

While a turn is running, a compact Activity row beneath the transcript shows a
verb such as `Thinking…`, `Reading…`, or `Running…` and the current operation.
`Ctrl+T` opens a focused Activity browser in all space above the composer;
`Ctrl+A` remains an alias. Up and Down select durable harness steps, and the
pane beside the list shows the selected step's complete result. Scroll long
results with PageUp, PageDown, or the mouse wheel. Escape or `Ctrl+T` returns
to the conversation. Completed activity is reconstructed from the durable
step chain when the TUI restarts. If the selection is already on the newest
step, new activity remains selected. Moving to an older step pauses that
tail-follow behavior.

Archive the selected conversation from the command palette (`Ctrl+Shift+P`,
then `archive`). Archiving atomically moves only the selected user's
membership ref from `active` to `archived`; it does not move the conversation
HEAD or affect other users. A running or publishing conversation must finish
first. Closing an unsent virtual conversation simply discards it. Use
`--list-archived` and `--unarchive <conversation-id>` outside the full-screen
UI to recover old conversations.

The transcript fills the conversation pane above the fixed composer. Use
`PageUp`, `PageDown`, or the mouse wheel over the transcript to scroll it.
Scrolling up pauses tail-follow and holds the viewport in place as new activity
arrives. The conversation border shows how many rendered lines remain below the
viewport and highlights the count when a new message arrived off-screen.
Scrolling back to the bottom resumes tail-follow and marks the message read.

Mouse-wheel routing requires terminal mouse capture, so CAOS implements visible
selection over the entire rendered interface. Drag across the header, sidebar,
conversation, activity, diff, help, prompt, or footer to highlight text and copy
automatically on mouse release. A click without a drag still selects a
conversation in the sidebar. macOS uses `pbcopy`; other environments receive
the same text through the standard OSC 52 terminal clipboard sequence.

For native terminal selection, press `Ctrl+Y`. CAOS releases mouse capture and
freezes redraws, so dragging and the terminal's normal copy shortcut (`Cmd+C`
on macOS or usually `Ctrl+Shift+C` elsewhere) work without moving output.
Press `Ctrl+Y` or `Escape` to resume.

## Workspace safety

Agent workspaces remain virtual commit trees under independent conversation
refs. Opening, switching, and running conversations never overwrite the working
checkout. Loading changes requires one `Ctrl+L` press and a clean working
tree, then detaches HEAD onto the conversation's head commit so the checkout
matches it exactly.

Publishing also leaves the checkout untouched. The first `Ctrl+P` opens a base
branch prompt with `origin`'s advertised default selected; type another branch
to override it, then press `Ctrl+P` again. CAOS starts a visible agent turn that
merges the exact fetched base with the standard `merge` tool, then resolves and
tests the result. When that fetched base is already an ancestor of the current
conversation head, the preparation turn runs without asking the agent to merge
it again. For another base, only this conversation's delta is applied, so child
conversations form clean PR stacks. Unresolved conflicts stop before the branch
moves. The checkout and index remain untouched.
Any conversation with a completed turn can be published, including one whose
workspace diff is empty — the transcript history is the content, and its
changes may already be part of the base.

CAOS points `caos/<conversation>` directly at the validated conversation head,
pushes it, and uses the authenticated `gh` CLI to find or open its pull
request. The first publication carries the conversation history; later
publications advance the branch by fast-forward. The selected PR base is an
ancestor of the published head, and the final tree must contain no reserved
`.caos` state.

`/publish-branch` is the sharing-only form of publication: it does not run the
agent merge-and-test preparation turn and does not create a PR. It still checks
the current tip for unresolved conflict records, conflict markers, and reserved
`.caos` state before moving the branch. It can publish a completed conversation
whose workspace diff is empty, since the transcript history itself is useful
to another client.

`/update-tree <message>` is the one command that reads the working tree back
into a conversation. It sends an ordinary user turn — authored by your git
identity, carrying your `<message>` — but the turn's commit takes its tree from
your local checkout instead of inheriting the head's. It first commits your
working tree: `git add -A` then a commit with `<message>` when the tree is
dirty (nothing is committed if you already committed the changes yourself). So
the snapshot covers tracked edits, new files, and deletions, honoring
`.gitignore`, and — because your changes are now committed — the checkout is
left clean. That matters: after a later agent turn you can press `Ctrl+L` again
to check out the new head without the clean-tree guard tripping on leftover
local changes. The agent then runs over the changes you folded in. A working
tree carrying the harness's reserved top-level `.caos` entry is refused.

The intended loop is `Ctrl+L` (check out the head) → edit files → `/update-tree
<message>` → let the agent respond → `Ctrl+L` again. You can also commit the
changes yourself first and then run `/update-tree <message>`; the already-clean
tree is committed no further and its `HEAD` tree is what the turn receives.

API responses currently arrive one completed model round at a time. The
backend also does not yet provide reliable cancellation for a running turn;
the UI states both limitations rather than simulating them client-side.
