# `caos tui`

`caos tui` is a full-screen terminal client for the CAOS agent harness. It uses
the same conversation engine as `caos talk`, while keeping terminal UI
dependencies out of the worker-side `caos` binary.

The interface keeps independent virtual conversations in a left sidebar. Each
two-row entry shows its title and latest user or agent message without exposing
the internal conversation ID. Each conversation has its own durable history,
multiline prompt, live activity, completed-turn hashes, and workspace diff.
Turns continue running when another conversation is selected, so several agent
workspaces can advance concurrently without touching the working checkout.

## Build and run

Run the client from a Git working tree whose `caos` remote points at a running
CAOS server:

```bash
git remote add caos http://localhost:9090
nix build
./result/bin/caos tui
```

During development, launch it with `cargo run -p caos --bin caos-cli -- tui`.

```text
caos tui                  continue the most recent conversation
caos tui --user alice     use alice's active conversation list
caos tui --new            start a fresh conversation
caos tui --from 5ec3751   branch from a completed turn
caos tui --list-archived  list archived conversation IDs and titles
caos tui --unarchive ID   restore one conversation to the active list
```

`--user` defaults to `$USER`. Active and archived membership is stored on the
CAOS server under `refs/caos/users/<user>/conversations/{active,archived}/`,
not in local TUI state. Existing local conversation refs are imported the first
time that user opens the TUI.

## Controls

Focus is either in the left pane (the conversation list) or the main pane (the
conversation). While the conversation list is focused, `Up` / `Down` move
through conversations and `Enter` moves focus into the conversation pane;
`Escape` in the conversation pane (with no slash-command matches showing) moves
focus back to the list. The focused pane's border is highlighted.

| Input | Action |
|---|---|
| `Up` / `Down` (list focused) | Select the previous or next conversation |
| `Enter` (list focused) | Focus the conversation pane |
| `Ctrl+E` (list focused) | Archive the selected conversation for this user |
| `Escape` (conversation focused) | Dismiss slash-command matches, else focus the list |
| `Ctrl+A` / `Ctrl+E` (conversation focused) | Move to the start / end of the current line |
| `Ctrl+S` | Send the prompt (`Ctrl+Enter` also works in terminals with enhanced keyboard input) |
| `Enter` or `Ctrl+J` | Insert a newline |
| `Tab` | Complete the selected slash command |
| `Up` / `Down` | Select a visible slash-command match |
| `Alt+Left` / `Alt+Right` or `Alt+B` / `Alt+F` | Move by whitespace-delimited words |
| `Alt+Backspace` / `Alt+Delete` | Delete the previous or next word |
| `Ctrl+W` (conversation focused) | Delete the previous word |
| `Ctrl+K` (conversation focused) | Kill from the cursor to the end of the line |
| `Ctrl+Up` / `Ctrl+Down` | Select the previous or next conversation |
| `Ctrl+N` | Start a new virtual conversation (from either focus) and focus it |
| `Ctrl+H` | Enter or leave keyboard help |
| `Ctrl+Q` | Switch between conversation and workspace changes |
| `Ctrl+T` | Enter or leave the Activity browser |
| `Ctrl+Shift+T` | Show the tools available to the selected conversation |
| `Up` / `Down` in Activity | Select the previous or next activity entry |
| `PageUp` / `PageDown` in Activity | Scroll the selected activity's full details |
| `Escape` in Activity | Return to the conversation |
| `PageUp` / `PageDown` in conversation | Scroll by rendered rows |
| Mouse wheel over the transcript | Scroll the conversation by rendered rows |
| Mouse wheel over Activity | Scroll the selected activity's full details |
| Mouse drag over visible transcript text | Select and copy rendered text |
| `Ctrl+Y` | Release mouse capture and freeze redraws for native selection |
| `Ctrl+L` | Check out the selected conversation's head commit in the working tree |
| `Ctrl+P` twice | Push the selected conversation as a clean branch and open a PR |
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
conversation ID or HEAD. Enter `/update-tree <message>` to send an ordinary
user turn whose commit also folds in your current working-tree changes — the
intended companion to `Ctrl+L` (check out the head, edit files, then
`/update-tree <message>` with the text you want in that turn). Activity entries
show the durable hashes of internal harness steps for inspection; those step
trees contain harness metadata and are not branch points.

Conversation text renders `**bold**` and `_italic_` emphasis. Unmatched markers
remain visible, and marker-like text inside inline backticks is left literal.

A fresh conversation starts with a temporary `talk-N` title. Its first prompt
automatically becomes a whitespace-collapsed title of at most 60 characters.
Using `/title` before the first prompt keeps that explicit title instead.
Existing conversations are never automatically retitled.

Fresh conversations start from the fetched tip of `origin`'s advertised
default branch. `--base` and `/from <turn-hash>` override that default.

Typing `/` at the start of the prompt shows matching slash commands and their
usage. Matches are case-sensitive. Use Up and Down to choose a match, then Tab
or Enter to complete it with a trailing space. Typing arguments closes the
menu. Escape dismisses it without changing the prompt. An unrecognized
slash-prefixed prompt is sent normally.

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
to the conversation. If the selection is already on the newest step, new
activity remains selected. Moving to an older step pauses that tail-follow
behavior.

Archiving atomically moves only the selected user's membership ref from
`active` to `archived`; it does not move the conversation HEAD or affect other
users. A running or publishing conversation must finish first. Closing an
unsent virtual conversation simply discards it. Use `--list-archived` and
`--unarchive <conversation-id>` outside the full-screen UI to recover old
conversations.

The transcript fills the conversation pane above the fixed composer. Use
`PageUp`, `PageDown`, or the mouse wheel over the transcript to scroll it.
Scrolling up pauses tail-follow and holds the viewport in place as new activity
arrives. The conversation border shows how many rendered lines remain below the
viewport and highlights the count when a new message arrived off-screen.
Scrolling back to the bottom resumes tail-follow and marks the message read.

Mouse-wheel routing requires terminal mouse capture, so CAOS implements visible
selection for the current transcript viewport. Drag across rendered transcript
text to highlight it and copy automatically on mouse release. macOS uses
`pbcopy`; other environments receive the same text through the standard OSC 52
terminal clipboard sequence.

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

Publishing also leaves the checkout untouched. Two `Ctrl+P` presses create or
replace `caos/<conversation>` with one clean snapshot commit directly above the
fetched tip of `origin`'s advertised default branch. Before creating it, CAOS
three-way merges that tip with the conversation head without touching the
checkout or index. Non-conflicting upstream changes survive; a conflict stops
publication and lists its paths in the chat. CAOS pushes the clean snapshot and
uses the authenticated `gh` CLI to find or open its pull request against the
same default branch. Republish replaces the commit instead of retaining earlier
snapshots, and the branch excludes the conversation's internal step DAG and
`.caos` metadata even when one conversation starts from another conversation's
head.

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
