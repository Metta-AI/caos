# `caos tui`

`caos tui` is a full-screen terminal client for the CAOS agent harness. It uses
the same conversation engine as `caos talk`, while keeping terminal UI
dependencies out of the worker-side `caos` binary.

The interface keeps independent virtual conversations in a left sidebar. Each
entry has a stable task title and a second row reserved for live operation or
attention status. Idle conversations do not show a stripped message preview.
`Ctrl+N` allocates local state; a source tree operation or the first submitted
message publishes its conversation head and active membership. The launcher's
initial conversation is created immediately so its attachments are visible. The
first message also runs one separate, stateless `llm-call` title job concurrently
with the agent turn. The result uses
the existing durable title metadata, so reopening the TUI does not regenerate
it or require any additional refs. Text ends with a visible ellipsis instead of
hard terminal clipping, and internal conversation IDs stay hidden. Each
conversation has its own durable history, multiline prompt, live activity,
completed-turn hashes, and source tree diff.
Turns continue running when another conversation is selected, so several agent
source trees can advance concurrently without touching the working checkout.
Agents may use `spawn_agent` to create an indexed child conversation. It runs
through `run_async`; harvesting reconciles its changed conversation files and
source trees into the parent, optionally restricted to named paths. Child rows use
their prompt title and appear beneath the parent conversation.

## Build and run

Deploy matching client and daemon builds. The daemon seeds the compiler with
shared libraries, so updating only the client can leave new tools linked against
old libraries. Restart the updated daemon while preserving its Git store;
bootstrap defaults to a computation-cache namespace for that build.

Before declaring a deployment ready, resolve its workers against that server:

```sh
CAOS_SERVER_URL=http://127.0.0.1:9092 result/bin/caos eval-path std/llm-step
CAOS_SERVER_URL=http://127.0.0.1:9092 result/bin/caos eval-path std/llm-call
CAOS_SERVER_URL=http://127.0.0.1:9092 result/bin/caos run-tool tests/bash-tool --test-salt="$(date +%s)"
```

These checks compile the deployed dependencies and execute the bash worker
without making a model API call. Merely opening the TUI does not resolve its
worker images; the full test suite uses its own stack.


The packaged TUI can run anywhere. Inside a checkout, it uses that checkout's `caos` remote;
`--import <path>` imports a commit snapshot of disk content; `--base HEAD` excludes disk changes. Outside a checkout, it starts
without code and defaults to `http://localhost:9090`; `--server` overrides it.
The harness and object database live under `$XDG_DATA_HOME/caos/clients`
(default `~/.local/share/caos/clients`), independently of attached repositories.

To build and launch from the caos checkout:

```bash
git remote add caos http://localhost:9090
nix build
./result/bin/caos tui --llm-step:@=std/llm-step --llm-call:@=std/llm-call
```

A session NAMES the two workers it runs — the durable turn (`llm-step`) and the
one-shot call that titles a conversation (`llm-call`) — as ordinary image args.
Both are required; the TUI looks in no path of its own. The paths above are
this repository's; a repo that mounted caos through
`std/flake-input-loader` writes `--llm-step:@=caos-std/llm-step`, and one that
only pinned it writes
`--llm-step:@@=git+https://github.com/Metta-AI/caos?rev=<sha>&dir=std/llm-step`.
`:hash=<oid>` and `:docker=<ref>` work too — it is the same arg vocabulary
`caos run` and `caos curry` take.

During development, launch it with
`cargo run -p caos-cli --bin caos-cli -- tui --llm-step:@=std/llm-step --llm-call:@=std/llm-call`.
The TUI checks the configured server before entering the alternate screen. If
it cannot connect within five seconds, it exits with the server URL and asks
you to check the running service and the `caos` git remote.

The Anthropic API key is checked next, still at the shell prompt. When the
git-ignored `.caos-secrets` store has no `anthropic-api-key` secret, the TUI
asks for one — paste the key, or enter the path to a file that holds it — and
writes the canonical secret entry, trimmed, with fresh cache-isolation entropy
already included (what `caos secrets` would add). Its `reader=` lines are the
paths `--llm-step`/`--llm-call` named, so the grant matches the run; an arg
with no reader spelling (`:@@=`, `:docker=`) is reported instead of silently
left ungranted. It ensures git ignores
`.caos-secrets/` (adding the rule to `.git/info/exclude` when nothing else
covers it), re-loads the store through the normal loader, and continues
straight into the UI — no relaunch. A pasted key is erased from the screen the
moment it is submitted. A store that exists but fails to load is reported as
the error it is rather than prompting, so an existing broken configuration is
never overwritten.

Before submitting a chat turn, the client checks that the selected worker
will receive the key. Missing or mismatched readers produce a local error
naming the required `reader=` setting. Correct the entry and resend the message.

Below, `$W` stands for the two required image args
(`--llm-step:@=std/llm-step --llm-call:@=std/llm-call`). The last two run no
worker, so they take neither.

```text
caos tui $W                  continue the most recent conversation
caos tui $W --username alice use alice's active conversation list
caos tui $W --new            start a fresh conversation
caos tui $W --import imports/caos/base  load this checkout at imports/caos/base
caos tui $W --server URL     use a specific server
caos tui $W --from 5ec3751   branch from a completed turn
caos tui --list-archived     list archived conversation IDs and titles
caos tui --unarchive ID      restore one conversation to the active list
```

`--username` defaults to `$USER`. If `$USER` is a shared container account such
as `root` or `ubuntu`, pass a personal `--username`; persisted identity is future
work. Active and archived membership is stored on the
CAOS server under `refs/caos/v3/users/<user-key>/conversations/{active,archived}/`.
User and conversation keys are lowercase hex of their UTF-8 IDs, without an
extra prefix. Usernames are limited to 126 bytes and conversation IDs to 124.
Only v3 refs populate this sidebar; earlier namespaces remain untouched.

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
| `Enter` | Run a recognized slash command at the end of a single-line prompt; otherwise complete a command or insert a newline |
| `Shift+Enter` or `Ctrl+J` | Insert a newline |
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
| `Ctrl+Shift+T` | Show the tools available to the selected conversation |
| `Up` / `Down` in Activity | Select the previous or next activity entry |
| `PageUp` / `PageDown` in Activity | Scroll the selected activity's full details |
| `Escape` in Activity | Return to the conversation |
| `PageUp` / `PageDown` in conversation | Scroll by rendered rows |
| Mouse wheel over the transcript | Scroll the conversation by rendered rows |
| Mouse wheel over Activity | Scroll the selected activity's full details |
| Mouse drag over rendered text | Select and copy text anywhere in the interface |
| `Ctrl+Y` | Release mouse capture and freeze redraws for native selection |
| `/checkout <gitlink> [directory]` | Check out this commit, reusing its local directory when omitted |
| `Ctrl+O` | Browse conversation files and source-tree diffs |
| `/pr <gitlink> <base-branch> [remote-URL]` | Preview one PR; Enter confirms |
| `/publish-branch <gitlink> [remote-URL]` | Preview and push this snapshot without creating a PR |
| `Ctrl+R` | Reload completed conversation history |
| `Ctrl+C` | Clear a non-empty prompt; exit when the prompt is empty |

Failures from local UI commands are shown in a temporary red command-error
panel instead of being inserted into the conversation transcript. Routine
operation status is shown only while the operation is running and is not added
to the transcript or title. Completed imports, pushes, and PR creation or
updates remain in the transcript as CAOS messages.

Completed user and agent turns show branchable hashes in the transcript. Enter
`/from <turn-hash>` to start a fresh conversation from one without leaving the
TUI. Enter `/title <new title>` to change the
shared title without changing the conversation ID (the metadata update advances its conversation head). Enter `/model <name>` to select the client-wide model
for later turns; known model names type ahead. `/model default` restores the
client default. Enter `/update-tree <gitlink> <message>` to send an ordinary
user turn whose commit also folds in edits in that gitlink's remembered checkout — the
intended companion to `/checkout <gitlink> [directory]` (check out the head, edit files, then
`/update-tree <gitlink> <message>` with the text you want in that turn). Activity entries
show the durable hashes of internal harness steps for inspection; those step
trees contain harness metadata and are not branch points.

Press `Ctrl+O` to open the read-only conversation filesystem. Up/Down select
entries and immediately preview them. The list includes each directory's immediate
children, indented one level, and those children can be selected directly.
Gitlinks are magenta; ordinary directories are cyan.
Right/Enter opens a directory or gitlink;
Left/Backspace returns to its parent, preserving the selection. Escape closes
the browser. Click to select; scroll over the file list to move the selection,
or over the preview to scroll its text. PageUp/PageDown scroll the preview.

Ordinary files show their contents, including memories and `.caos` metadata.
A folder containing sibling gitlinks previews its newest two entries in
descending filename order. Each gitlink compares against the next gitlink below
it; entering one shows file diffs, including deleted files. The oldest gitlink
shows contents because it has no earlier boundary. The preview names the
compared boundaries and hashes. No publishing destination is required.

The browser pins the conversation head when opened. `r` refreshes it. There
are no shell commands or controls that apply edits.

On a source-tree entry, `o` selects the source for tool descriptions and local
edit submission. Checkout and publication take explicit paths. Use `/import <path> <source> [revision]` to import
a Git checkout's disk snapshot at an unused path. An unchanged checkout reuses
HEAD; changes become a child commit. A URL or explicit revision imports that
commit instead. Local imports require a checkout root with a HEAD; linked
worktrees work, but plain directories, individual files, and subdirectory
snapshots are not supported. Untracked files honor Git ignore rules; tracked
files remain included. The source index, branches, and files stay unchanged. Each import's
portable repository details live in `<path>.source.json` when available. The
agent preserves imports and organizes feature work with ordinary file operations.

`/pr feature/01-change main [remote-URL]` fetches a preview for that exact
snapshot against the named remote branch. Its full path is the PR branch name.
The URL is inferred from matching import provenance when unambiguous; otherwise
supply it explicitly. `origin` is not a portable repository URL. Enter confirms
pushing and opening or updating the PR; Escape cancels. No picker or destination
editor is involved. For a stack, publish the first PR, then run e.g.
`/pr feature/02-next feature/01-change`. The command's base wins regardless of
sibling ordering. `/publish-branch <gitlink> [remote-URL]` skips PR creation.
Publication never edits or tests code. Ctrl+P and Ctrl+L have no bindings.

When a PR source does not contain the fetched base tip, the preview offers to
import that exact base and send an integration request to the agent. Enter
confirms both; Escape cancels. The request stays in the original conversation
and preserves drafts. It asks for a merge or rebase and tests, then stops for a
fresh `/pr` review. This action does not publish. Successful pushes and PR
creation or updates appear as persistent CAOS messages, including the PR URL.

Conversation text renders `**bold**` and `_italic_` emphasis. Unmatched markers
remain visible, and marker-like text inside inline backticks is left literal.

A fresh conversation starts with a dimmed `New conversation` placeholder.
Reopening it before sending a prompt preserves automatic naming. Its first prompt
provides an immediate fallback title and starts a stateless `llm-call` job using
that message alone. Title generation runs concurrently with the agent turn, so
it does not depend on the turn succeeding. Failure leaves the fallback in
place, and later messages make no title calls. Using `/title` before the first prompt keeps that explicit title instead.

The launcher starts without code. `--import imports/caos/base` snapshots the
checkout's current disk contents as a gitlink at that path. Add `--base HEAD`
or another revision to exclude disk changes and import that commit.
`/from <turn-hash>` forks the selected conversation history. Conversations in earlier formats
require the previous build; this version does not migrate them implicitly.

Typing `/` at the start of the prompt shows matching slash commands and their
usage. Matches are case-sensitive. Use Up and Down to choose a match, then Tab
or Enter to complete a partial command with a trailing space. Enter runs a
recognized command when the cursor is at the end of a single-line prompt.
A partial model name completes first; Enter again applies it. Shift+Enter or
Ctrl+J always inserts a newline. Typing arguments closes the command menu. Escape dismisses it without changing the prompt. An unrecognized
slash-prefixed prompt is sent normally.

`Ctrl+Shift+P` or `/commands` opens a searchable command palette without
changing the current draft. Type any words from an action, use Up and Down to
choose a match, then press Enter to run it. The palette covers conversation,
file browsing, activity, tool, help, reload, archive, and selection actions.
Escape closes the palette or slash-command menu while an agent turn or
publication keeps running. With the menu closed, Escape interrupts that work.

Bracketed paste mode keeps pasted newlines inside the prompt instead of
submitting partial lines. Pastes over 1,000 characters are kept out of the
editable buffer and shown as an atomic `[Pasted text: N chars]` placeholder.
The full text, including newlines, replaces the placeholder when the prompt is
sent. Backspace or Delete removes the whole placeholder. Press `Ctrl+C` to
clear a draft and any stored paste content; press it again on the empty prompt
to exit.

While a turn is running, a compact Activity row beneath the transcript shows a
verb such as `Thinking…`, `Reading…`, or `Running…` and the current operation.
Choose Activity in the command palette to open its browser in all space
above the composer. Up and Down select durable harness steps, and the
pane beside the list shows the selected step's complete result. Scroll long
results with PageUp, PageDown, or the mouse wheel. Escape returns
to the conversation. Completed activity is reconstructed from the durable
step chain when the TUI restarts. If the selection is already on the newest
step, new activity remains selected. Moving to an older step pauses that
tail-follow behavior.

Archive the selected conversation with `/archive` or from the command palette (`Ctrl+Shift+P`,
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
conversation in the sidebar. Local macOS sessions use `pbcopy` and show
“Copied” after it succeeds. SSH sessions and other environments send the same
text through the standard OSC 52 terminal clipboard sequence and show
“Copy requested”. A clipboard helper or terminal write failure stays in the
chat as a command error, preserving the draft. Press `Ctrl+H` for copying help.

For native terminal selection, press `Ctrl+Y`. CAOS releases mouse capture and
freezes redraws, so dragging and the terminal's normal copy shortcut (`Cmd+C`
on macOS or usually `Ctrl+Shift+C` elsewhere) work without moving output.
Press `Ctrl+Y` or `Escape` to resume.

## Source tree safety

Source tree code is referenced by ordinary commit hashes from the separate
conversation history. Opening and running conversations never overwrite a
checkout. `/checkout <gitlink> [directory]` uses an explicit destination or
reuses that gitlink's remembered local directory. The destination must be a clean Git checkout
or an empty/new directory. The client imports the code objects and detaches HEAD
at the named commit. `/update-tree <gitlink> <message>` commits local edits in
that gitlink's remembered checkout and imports their
closure into the client before submission. These commands never replace the
internal harness.

Publication preserves source tree history, uses leased branch updates, and
checks conflict cleanup before preview and again before pushing. Resolve a
nonempty `.caos/conflicts` ledger by fixing each path and clearing its entries.
Saving an edited source tree removes an empty ledger and prunes its empty
`.caos` directory. Any remaining `.caos` entry blocks publication; publishing
never rewrites the selected commit. It leaves the local
checkout and index unchanged. Credentials remain in the local secret store;
the launcher reuses an existing checkout store or its own persistent store under
the data directory.


Over SSH, clipboard copying uses a terminal escape sequence. “Copy requested”
means the sequence was sent; terminals can ignore it without acknowledging.
For manual copying, press `Ctrl+Y`, select text with the terminal, and use its
Copy action; Escape resumes the TUI. In iTerm2, automatic clipboard writes
require Settings > General > Selection > Applications in terminal may access
clipboard. This setting is required even when CAOS reports “Copy requested”;
the request has no acknowledgement. If iTerm2's clipboard-access warning was
previously dismissed, copying can fail silently until the setting is enabled.
