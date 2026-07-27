# TUI conversation and input usability

Status: accepted for implementation

## Problem

The TUI exposes internal conversation identifiers in its primary navigation,
does not help users discover slash commands, implements only character-wise
cursor movement, and captures the mouse until the user explicitly enters copy
mode. These behaviors make routine conversation navigation and prompt editing
feel more mechanical than they need to.

## Goals

- Make the conversation list useful at a glance without displaying internal
  object IDs.
- Give a new conversation a meaningful title without requiring a `/title`
  command.
- Make supported slash commands discoverable and quick to complete.
- Support the word-wise movement and deletion keys commonly produced by
  Option on macOS terminals.
- Let users select terminal text with the mouse by default.

## Conversation summaries

Each conversation-list item occupies two visual rows:

1. The conversation title, prefixed by its running or publishing marker.
2. A dimmed, single-line preview of the most recent human or agent message.

The preview collapses whitespace, omits internal hashes, and is clipped by the
list widget to the available width. A conversation without a message shows
`New conversation`.

The internal conversation ID remains the durable ref identity and is still
accepted by CLI flags, but it is not shown in the normal TUI list.

### Automatic titles

A newly-created virtual conversation starts with the existing temporary
`talk-N` title. When its first non-command prompt is sent, the TUI replaces the
temporary title with a deterministic title derived from that prompt:

- collapse whitespace to single spaces;
- use at most 60 Unicode scalar values;
- append an ellipsis when text was omitted.

The generated title is published atomically with the first completed turn.
Explicit `/title` use before the first turn disables automatic replacement.
Existing conversations are never retitled merely because they are opened or
continued.

## Slash-command typeahead

The TUI owns a single command catalog containing each command's name, usage,
and description. Initially it contains:

- `/from <commit>` — start a new conversation from a completed turn;
- `/title <new title>` — rename the selected conversation.

When the composer contains a leading slash-command token and the cursor is
within that token, the composer shows matching commands. Matching is
case-sensitive prefix matching. `Tab` completes the selected command and adds
the separating space. Up and Down move through multiple matches while the
menu is visible; Enter accepts the selected command rather than sending an
incomplete command. Escape dismisses the menu without changing the text.

Once the cursor moves into command arguments, the menu closes and normal
multiline editing resumes. Unknown slash-prefixed text remains an ordinary
prompt so future server-side conventions are not blocked.

## Prompt movement

In addition to the existing character and line movement:

- `Option+Left` moves to the start of the previous word.
- `Option+Right` moves to the start of the next word.
- `Option+Backspace` deletes to the start of the previous word.
- `Option+Delete` deletes to the start of the next word.

For terminal compatibility, `Option+B` and `Option+F` are accepted as aliases
for word-left and word-right. Movement is UTF-8 safe. Word boundaries treat
contiguous whitespace and contiguous non-whitespace as runs.

## Copyable by default

The TUI does not enable terminal mouse capture at startup. Dragging therefore
uses the terminal's native selection and copy behavior without first pressing
a CAOS shortcut. Keyboard scrolling with PageUp and PageDown remains
available.

`Ctrl+Y` remains an optional selection lock: it freezes redraws and input while
the user selects text from a conversation that is still producing output.
`Ctrl+Y` or Escape resumes. The footer describes native selection as the
default and the lock as optional.

Mouse-wheel scrolling inside CAOS is removed because it requires mouse
capture. This is an intentional tradeoff in favor of default selection.

## Verification

- Unit-test automatic title generation with whitespace, Unicode, and long
  prompts.
- Render-test two-row conversation items, last-message previews, and absence
  of internal IDs.
- Unit-test command filtering, completion, selection, dismissal, and command
  execution.
- Unit-test word movement and deletion across whitespace and Unicode text.
- Exercise terminal lifecycle tests or focused helpers to verify mouse capture
  is not enabled on entry or resume.
- Run the repository-required `caos-cli run-tool test` on every PR branch.
