use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use caos::{GitTransport, Transport};
use caos_cli::{
    archive_user_conversation, compare_and_set_conversation_title, conversation_load,
    conversation_load_at, conversation_reference, conversation_snapshot, describe_tool_set,
    first_available_conversation_name, fork_conversation, generate_conversation_title,
    interrupt_request, invite_user_to_conversation, list_user_conversations,
    publish_user_conversation, resume_request, run_chat_turn, set_conversation_title,
    submit_interjection, unarchive_user_conversation, ConversationLoad, ConversationRole,
    ConversationSnapshot, InviteOutcome, ToolSetDescription, TurnEvent, TurnOptions, TurnOutcome,
    TurnPhase, UserConversationStatus, UserConversationSummary, WorkspaceDiff, DEFAULT_MODEL,
};
use ratatui_core::buffer::{Buffer, CellWidth};
use ratatui_core::layout::Rect;
use ratatui_crossterm::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::args::Args;
use super::workspace::{
    commit_working_tree, fetch_remote_branch_tip, load_conversation_workspace,
    local_default_branch_tip, prepare_publish_workspace, publish_conversation_branch,
    publish_conversation_pr, remote_base_is_ancestor, remote_default_branch,
};

#[path = "ui.rs"]
pub(crate) mod ui;

fn short_hash(hash: &str) -> &str {
    hash.get(..7).unwrap_or(hash)
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn automatic_title(prompt: &str) -> String {
    const MAX_CHARS: usize = 60;

    let title = collapse_whitespace(prompt);
    if title.chars().count() <= MAX_CHARS {
        return title;
    }

    title
        .chars()
        .take(MAX_CHARS - 1)
        .chain(std::iter::once('…'))
        .collect()
}

fn message_preview(text: &str, max_cells: u16) -> String {
    let text = collapse_whitespace(text);
    if max_cells == 0 {
        return String::new();
    }
    if text.cell_width() <= max_cells {
        return text.to_string();
    }
    let content_cells = max_cells.saturating_sub(1);
    let mut preview = String::new();
    let mut width: u16 = 0;
    for ch in text.chars() {
        let ch_width = ch.to_string().cell_width();
        if width.saturating_add(ch_width) > content_cells {
            break;
        }
        preview.push(ch);
        width = width.saturating_add(ch_width);
    }
    preview.push('…');
    preview
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Chat,
    Activity,
    Diff,
    Tools,
    Help,
}

/// Which pane currently receives navigation keys: the left conversation list
/// or the main conversation pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    List,
    Conversation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryRole {
    Human,
    Peer(String),
    Agent(Option<String>),
    Info,
    Notice,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranscriptEntry {
    role: EntryRole,
    commit: Option<String>,
    text: String,
    pending_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingSubmission {
    id: u64,
    text: String,
    commit: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReferenceNotice {
    refname: String,
    head: String,
}

struct RemotePollEntry {
    summary: UserConversationSummary,
    observed_head: Option<String>,
    observed_title: Option<String>,
    load: Option<Result<Box<ConversationLoad>, String>>,
}

#[derive(Debug, Default)]
struct ScrollState {
    offset: Option<usize>,
    rendered_max: Cell<usize>,
}

impl ScrollState {
    fn follow_tail(&mut self) {
        self.offset = None;
    }

    fn scroll_up(&mut self, rows: usize) {
        let offset = self.offset.unwrap_or_else(|| self.rendered_max.get());
        self.offset = Some(offset.saturating_sub(rows));
    }

    fn scroll_down(&mut self, rows: usize) {
        let max = self.rendered_max.get();
        let offset = self.offset.unwrap_or(max).saturating_add(rows).min(max);
        self.offset = (offset < max).then_some(offset);
    }

    fn resolve(&self, max: usize) -> u16 {
        self.rendered_max.set(max);
        self.offset.unwrap_or(max).min(max).min(u16::MAX as usize) as u16
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Activity {
    request: String,
    round: u64,
    id: String,
    step_commit: String,
    name: String,
    summary: String,
    detail: String,
    state: ActivityState,
}

impl Activity {
    fn answers(&self, request: &str, round: u64, tool_use_id: &str) -> bool {
        self.request == request && self.round == round && self.id == tool_use_id
    }

    fn running_verb(&self) -> &'static str {
        match self.name.as_str() {
            "bash" => "Running",
            "read" | "cat" => "Reading",
            "write" => "Writing",
            "edit" => "Editing",
            "ls" => "Listing",
            "grep" | "rgrep" => "Searching",
            _ => "Running",
        }
    }

    fn running_summary(&self) -> &str {
        match self.name.as_str() {
            "read" | "cat" | "write" | "edit" | "ls" | "grep" | "rgrep" => self
                .summary
                .strip_prefix(&self.name)
                .and_then(|summary| summary.strip_prefix(' '))
                .unwrap_or(&self.summary),
            _ => &self.summary,
        }
    }
}

fn replayed_activities(events: &[TurnEvent]) -> Vec<Activity> {
    let mut activities: Vec<Activity> = Vec::new();
    for event in events {
        match event {
            TurnEvent::ToolCall {
                step_commit,
                request,
                round,
                tool_use_id,
                name,
                summary,
            } => activities.push(Activity {
                request: request.clone(),
                round: *round,
                id: tool_use_id.clone(),
                step_commit: step_commit.clone(),
                name: name.clone(),
                summary: summary.clone(),
                detail: String::new(),
                state: ActivityState::Running,
            }),
            TurnEvent::ToolResult {
                step_commit,
                request,
                round,
                tool_use_id,
                is_error,
                content,
            } => {
                if let Some(activity) = activities
                    .iter_mut()
                    .find(|activity| activity.answers(request, *round, tool_use_id))
                {
                    activity.state = if *is_error {
                        ActivityState::Failed
                    } else {
                        ActivityState::Succeeded
                    };
                    activity.detail = content.clone();
                } else {
                    activities.push(Activity {
                        request: request.clone(),
                        round: *round,
                        id: tool_use_id.clone(),
                        step_commit: step_commit.clone(),
                        name: "result".to_string(),
                        summary: format!("result {tool_use_id}"),
                        detail: content.clone(),
                        state: if *is_error {
                            ActivityState::Failed
                        } else {
                            ActivityState::Succeeded
                        },
                    });
                }
            }
            _ => {}
        }
    }
    activities
}

const LARGE_PASTE_CHAR_THRESHOLD: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingPaste {
    placeholder: String,
    content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TranscriptPoint {
    row: u16,
    column: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TranscriptSelection {
    anchor: TranscriptPoint,
    head: TranscriptPoint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScreenSelection {
    anchor: TranscriptPoint,
    head: TranscriptPoint,
}

impl ScreenSelection {
    fn ordered(self) -> (TranscriptPoint, TranscriptPoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

impl TranscriptSelection {
    fn ordered(self) -> (TranscriptPoint, TranscriptPoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Composer {
    text: String,
    cursor: usize,
    selection_anchor: Option<usize>,
    pending_pastes: Vec<PendingPaste>,
    command_selection: usize,
    command_menu_dismissed: bool,
}

impl Composer {
    fn insert_char(&mut self, ch: char) {
        self.delete_selection();
        self.snap_cursor_after_placeholder();
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.reset_command_menu();
    }

    fn insert_str(&mut self, text: &str) {
        self.delete_selection();
        self.snap_cursor_after_placeholder();
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.reset_command_menu();
    }

    fn insert_paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let char_count = text.chars().count();
        if char_count > LARGE_PASTE_CHAR_THRESHOLD {
            let placeholder = self.next_paste_placeholder(char_count);
            self.insert_str(&placeholder);
            self.pending_pastes.push(PendingPaste {
                placeholder,
                content: text,
            });
        } else {
            self.insert_str(&text);
        }
    }

    fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.delete_range(previous, self.cursor);
    }

    fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        let Some(ch) = self.text[self.cursor..].chars().next() else {
            return;
        };
        self.delete_range(self.cursor, self.cursor + ch.len_utf8());
    }

    fn move_left(&mut self) {
        if let Some((start, _)) = self.selection_range() {
            self.cursor = start;
            self.selection_anchor = None;
            return;
        }
        self.move_cursor(self.previous_cursor(), false);
    }

    fn select_left(&mut self) {
        self.move_cursor(self.previous_cursor(), true);
    }

    fn previous_cursor(&self) -> usize {
        if let Some((start, _)) = self
            .paste_ranges()
            .into_iter()
            .find(|(start, end)| self.cursor > *start && self.cursor <= *end)
        {
            return start;
        }
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(self.cursor)
    }

    fn move_right(&mut self) {
        if let Some((_, end)) = self.selection_range() {
            self.cursor = end;
            self.selection_anchor = None;
            return;
        }
        self.move_cursor(self.next_cursor(), false);
    }

    fn select_right(&mut self) {
        self.move_cursor(self.next_cursor(), true);
    }

    fn next_cursor(&self) -> usize {
        if let Some((_, end)) = self
            .paste_ranges()
            .into_iter()
            .find(|(start, end)| self.cursor >= *start && self.cursor < *end)
        {
            return end;
        }
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|ch| self.cursor + ch.len_utf8())
            .unwrap_or(self.cursor)
    }

    fn move_cursor(&mut self, target: usize, selecting: bool) {
        if selecting {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        self.cursor = target;
    }

    fn move_word_left(&mut self) {
        self.move_cursor(self.word_left(), false);
    }

    fn select_word_left(&mut self) {
        self.move_cursor(self.word_left(), true);
    }

    fn move_word_right(&mut self) {
        self.move_cursor(self.word_right(), false);
    }

    fn select_word_right(&mut self) {
        self.move_cursor(self.word_right(), true);
    }

    fn delete_word_left(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.word_left();
        self.delete_range(start, self.cursor);
    }

    fn delete_word_right(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.word_right();
        self.delete_range(self.cursor, end);
    }

    fn kill_line(&mut self) {
        if self.delete_selection() {
            return;
        }
        let (_, end) = self.line_bounds();
        if self.cursor == end {
            // Already at the end of the line: swallow the newline, joining the
            // next line onto this one (matching readline's Ctrl+K).
            if end < self.text.len() {
                self.delete_range(self.cursor, self.cursor + 1);
            }
            return;
        }
        self.delete_range(self.cursor, end);
    }

    fn word_left(&self) -> usize {
        let mut chars = self.text[..self.cursor].char_indices().rev().peekable();
        while chars.peek().is_some_and(|(_, ch)| ch.is_whitespace()) {
            chars.next();
        }
        while chars.peek().is_some_and(|(_, ch)| !ch.is_whitespace()) {
            chars.next();
        }
        chars
            .peek()
            .map(|(index, ch)| index + ch.len_utf8())
            .unwrap_or(0)
    }

    fn word_right(&self) -> usize {
        let mut chars = self.text[self.cursor..].char_indices().peekable();
        while chars.peek().is_some_and(|(_, ch)| !ch.is_whitespace()) {
            chars.next();
        }
        while chars.peek().is_some_and(|(_, ch)| ch.is_whitespace()) {
            chars.next();
        }
        chars
            .peek()
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len())
    }

    fn line_bounds(&self) -> (usize, usize) {
        let start = self.text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let end = self.text[self.cursor..]
            .find('\n')
            .map(|index| self.cursor + index)
            .unwrap_or(self.text.len());
        (start, end)
    }

    fn move_home(&mut self) {
        self.move_cursor(self.line_bounds().0, false);
    }

    fn select_home(&mut self) {
        self.move_cursor(self.line_bounds().0, true);
    }

    fn move_end(&mut self) {
        self.move_cursor(self.line_bounds().1, false);
    }

    fn select_end(&mut self) {
        self.move_cursor(self.line_bounds().1, true);
    }

    fn move_vertical(&mut self, up: bool) {
        self.selection_anchor = None;
        let (start, end) = self.line_bounds();
        let column = self.text[start..self.cursor].chars().count();
        let target = if up {
            if start == 0 {
                return;
            }
            let target_end = start - 1;
            let target_start = self.text[..target_end]
                .rfind('\n')
                .map(|index| index + 1)
                .unwrap_or(0);
            (target_start, target_end)
        } else {
            if end == self.text.len() {
                return;
            }
            let target_start = end + 1;
            let target_end = self.text[target_start..]
                .find('\n')
                .map(|index| target_start + index)
                .unwrap_or(self.text.len());
            (target_start, target_end)
        };
        self.cursor = byte_at_column(&self.text, target.0, target.1, column);
        self.snap_cursor_after_placeholder();
    }

    #[cfg(test)]
    fn cursor_row_col(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.bytes().filter(|byte| *byte == b'\n').count();
        let column = before
            .rsplit_once('\n')
            .map(|(_, line)| line)
            .unwrap_or(before)
            .chars()
            .count();
        (row, column)
    }

    fn take_message(&mut self) -> Option<String> {
        let message = self.expanded_text().trim().to_string();
        if message.is_empty() {
            return None;
        }
        self.clear();
        Some(message)
    }

    /// Put a failed submission back in the draft without replacing anything
    /// the user typed while the request was in flight.
    fn restore_message(&mut self, message: &str) {
        if self.expanded_text().trim() == message.trim() {
            return;
        }
        self.selection_anchor = None;
        self.cursor = self.text.len();
        if !self.text.is_empty() {
            self.insert_str("\n\n");
        }
        self.insert_str(message);
    }

    fn clear(&mut self) -> bool {
        if self.text.is_empty() && self.pending_pastes.is_empty() {
            return false;
        }
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = None;
        self.pending_pastes.clear();
        self.reset_command_menu();
        true
    }

    fn expanded_text(&self) -> String {
        let mut ranges: Vec<_> = self
            .pending_pastes
            .iter()
            .filter_map(|paste| {
                self.text
                    .find(&paste.placeholder)
                    .map(|start| (start, start + paste.placeholder.len(), &paste.content))
            })
            .collect();
        ranges.sort_by_key(|(start, _, _)| *start);

        let mut expanded = String::new();
        let mut previous = 0;
        for (start, end, content) in ranges {
            expanded.push_str(&self.text[previous..start]);
            expanded.push_str(content);
            previous = end;
        }
        expanded.push_str(&self.text[previous..]);
        expanded
    }

    fn next_paste_placeholder(&self, char_count: usize) -> String {
        let base = format!("[Pasted text: {char_count} chars]");
        if !self.text.contains(&base) {
            return base;
        }
        let mut ordinal = 2;
        loop {
            let candidate = format!("[Pasted text: {char_count} chars #{ordinal}]");
            if !self.text.contains(&candidate) {
                return candidate;
            }
            ordinal += 1;
        }
    }

    fn paste_ranges(&self) -> Vec<(usize, usize)> {
        self.pending_pastes
            .iter()
            .filter_map(|paste| {
                self.text
                    .find(&paste.placeholder)
                    .map(|start| (start, start + paste.placeholder.len()))
            })
            .collect()
    }

    fn snap_cursor_after_placeholder(&mut self) {
        if let Some((_, end)) = self
            .paste_ranges()
            .into_iter()
            .find(|(start, end)| self.cursor > *start && self.cursor < *end)
        {
            self.cursor = end;
        }
    }

    fn delete_range(&mut self, mut start: usize, mut end: usize) {
        loop {
            let original = (start, end);
            for (paste_start, paste_end) in self.paste_ranges() {
                if start < paste_end && end > paste_start {
                    start = start.min(paste_start);
                    end = end.max(paste_end);
                }
            }
            if (start, end) == original {
                break;
            }
        }
        self.text.drain(start..end);
        self.cursor = start;
        self.selection_anchor = None;
        self.pending_pastes
            .retain(|paste| self.text.contains(&paste.placeholder));
        self.reset_command_menu();
    }

    fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        (anchor != self.cursor).then_some({
            if anchor < self.cursor {
                (anchor, self.cursor)
            } else {
                (self.cursor, anchor)
            }
        })
    }

    fn selected_text(&self) -> Option<&str> {
        self.selection_range()
            .map(|(start, end)| &self.text[start..end])
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection_range() else {
            self.selection_anchor = None;
            return false;
        };
        self.delete_range(start, end);
        true
    }

    fn command_token(&self) -> Option<&str> {
        if self.command_menu_dismissed || !self.text.starts_with('/') {
            return None;
        }
        let token_end = self
            .text
            .find(char::is_whitespace)
            .unwrap_or(self.text.len());
        (self.cursor <= token_end).then(|| &self.text[..token_end])
    }

    fn command_matches(&self) -> Vec<&'static Command> {
        let Some(token) = self.command_token() else {
            return Vec::new();
        };
        COMMANDS
            .iter()
            .filter(|command| command.name.starts_with(token))
            .collect()
    }

    fn model_token(&self) -> Option<(usize, usize, &str)> {
        const PREFIX: &str = "/model";
        if self.command_menu_dismissed || !self.text.starts_with(PREFIX) {
            return None;
        }
        let arguments = &self.text[PREFIX.len()..];
        if !arguments.chars().next().is_some_and(char::is_whitespace) {
            return None;
        }
        let start = arguments
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
            .map(|(offset, _)| PREFIX.len() + offset)
            .unwrap_or(self.text.len());
        let end = self.text[start..]
            .find(char::is_whitespace)
            .map(|offset| start + offset)
            .unwrap_or(self.text.len());
        (self.cursor >= start && self.cursor <= end).then(|| (start, end, &self.text[start..end]))
    }

    fn model_matches(&self) -> Vec<&'static str> {
        let Some((_, _, token)) = self.model_token() else {
            return Vec::new();
        };
        MODEL_OPTIONS
            .iter()
            .copied()
            .filter(|model| {
                model.starts_with(token)
                    || model
                        .strip_prefix("claude-")
                        .is_some_and(|short| short.starts_with(token))
            })
            .collect()
    }

    fn completion_count(&self) -> usize {
        self.command_matches().len() + self.model_matches().len()
    }

    fn select_command(&mut self, amount: isize) -> bool {
        let count = self.completion_count();
        if count == 0 {
            return false;
        }
        self.command_selection =
            (self.command_selection as isize + amount).rem_euclid(count as isize) as usize;
        true
    }

    fn complete_command(&mut self) -> bool {
        if let Some(command) = self.command_matches().get(self.command_selection).copied() {
            let token_end = self
                .text
                .find(char::is_whitespace)
                .unwrap_or(self.text.len());
            self.text.replace_range(..token_end, command.name);
            self.cursor = command.name.len();
        } else if let (Some((start, end, _)), Some(model)) = (
            self.model_token(),
            self.model_matches().get(self.command_selection).copied(),
        ) {
            self.text.replace_range(start..end, model);
            self.cursor = start + model.len();
        } else {
            return false;
        }
        self.selection_anchor = None;
        if let Some(ch) = self.text[self.cursor..].chars().next() {
            self.cursor += ch.len_utf8();
        } else {
            self.text.push(' ');
            self.cursor += 1;
        }
        self.reset_command_menu();
        true
    }

    fn dismiss_command_menu(&mut self) -> bool {
        if self.completion_count() == 0 {
            return false;
        }
        self.command_menu_dismissed = true;
        true
    }

    fn reset_command_menu(&mut self) {
        self.command_selection = 0;
        self.command_menu_dismissed = false;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppAction {
    From,
    Help,
    Invite,
    Model,
    Commands,
    PublishBranch,
    Reference,
    Title,
    UpdateTree,
    NewConversation,
    Checkout,
    Publish,
    Activity,
    Changes,
    Tools,
    Reload,
    Archive,
    SelectionLock,
}

impl AppAction {
    fn submits_message(self) -> bool {
        matches!(self, Self::UpdateTree)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Command {
    name: &'static str,
    usage: &'static str,
    description: &'static str,
    action: AppAction,
    takes_argument: bool,
}

// Completion hints for current public models compatible with llm-step's
// adaptive-thinking request. Explicit strings remain accepted for gateways
// and newly released models.
const MODEL_OPTIONS: [&str; 8] = [
    "default",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-sonnet-4-6",
    "claude-opus-4-6",
];

const COMMANDS: [Command; 9] = [
    Command {
        name: "/from",
        usage: "/from <commit>",
        description: "start a conversation from a completed turn",
        action: AppAction::From,
        takes_argument: true,
    },
    Command {
        name: "/help",
        usage: "/help",
        description: "show keyboard shortcuts and slash commands",
        action: AppAction::Help,
        takes_argument: false,
    },
    Command {
        name: "/title",
        usage: "/title <new title>",
        description: "rename the selected conversation",
        action: AppAction::Title,
        takes_argument: true,
    },
    Command {
        name: "/update-tree",
        usage: "/update-tree <message>",
        description: "fold working-tree edits into the commit",
        action: AppAction::UpdateTree,
        takes_argument: true,
    },
    Command {
        name: "/commands",
        usage: "/commands",
        description: "open the searchable command palette",
        action: AppAction::Commands,
        takes_argument: false,
    },
    Command {
        name: "/publish-branch",
        usage: "/publish-branch",
        description: "push the complete conversation branch without a PR",
        action: AppAction::PublishBranch,
        takes_argument: false,
    },
    Command {
        name: "/ref",
        usage: "/ref",
        description: "show the copyable conversation ref and full head hash",
        action: AppAction::Reference,
        takes_argument: false,
    },
    Command {
        name: "/invite",
        usage: "/invite <username>",
        description: "add to one username's sidebar (case-sensitive; spaces allowed)",
        action: AppAction::Invite,
        takes_argument: true,
    },
    Command {
        name: "/model",
        usage: "/model <name|default>",
        description: "select the model for future turns in this client",
        action: AppAction::Model,
        takes_argument: true,
    },
];

fn parse_command(message: &str) -> Option<(&'static Command, &str)> {
    let token_end = message.find(char::is_whitespace).unwrap_or(message.len());
    let command = COMMANDS
        .iter()
        .find(|command| command.name == &message[..token_end])?;
    Some((command, message[token_end..].trim()))
}

fn byte_at_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map(|(offset, _)| start + offset)
        .unwrap_or(end)
}

struct ConversationState {
    id: String,
    title: String,
    parent: Option<String>,
    remote_title: Option<String>,
    sidebar_attention: Option<String>,
    automatic_title: bool,
    automatic_title_fallback_applied: bool,
    automatic_title_fallback: Option<String>,
    generating_title: bool,
    virtual_conversation: bool,
    turn_options: TurnOptions,
    transcript: Vec<TranscriptEntry>,
    pending_submissions: Vec<PendingSubmission>,
    next_pending_submission: u64,
    activities: Vec<Activity>,
    diff: Option<WorkspaceDiff>,
    tool_set: Option<Result<ToolSetDescription, String>>,
    composer: Composer,
    status: String,
    command_error: Option<String>,
    reference_notice: Option<ReferenceNotice>,
    reference_loading: bool,
    reference_generation: u64,
    publish_prompt: bool,
    running: bool,
    interrupting: bool,
    local_turn: bool,
    active_request: Option<String>,
    reconciling_request: Option<String>,
    reconcile_after: Option<Instant>,
    turn_phase: TurnPhase,
    publishing: bool,
    forking: bool,
    scroll: ScrollState,
    unread_below: bool,
    transcript_selection: Option<TranscriptSelection>,
    activity_selection: Option<usize>,
    activity_detail_scroll: usize,
    remote_head: Option<String>,
}

impl ConversationState {
    fn new(id: String, title: String, turn_options: TurnOptions, status: String) -> Self {
        let remote_title = Some(title.clone());
        Self {
            id,
            title,
            parent: None,
            remote_title,
            sidebar_attention: None,
            automatic_title: false,
            automatic_title_fallback_applied: false,
            automatic_title_fallback: None,
            generating_title: false,
            virtual_conversation: false,
            turn_options,
            transcript: Vec::new(),
            pending_submissions: Vec::new(),
            next_pending_submission: 0,
            activities: Vec::new(),
            diff: None,
            tool_set: None,
            composer: Composer::default(),
            status,
            command_error: None,
            reference_notice: None,
            reference_loading: false,
            reference_generation: 0,
            publish_prompt: false,
            running: false,
            interrupting: false,
            local_turn: false,
            active_request: None,
            reconciling_request: None,
            reconcile_after: None,
            turn_phase: TurnPhase::System,
            publishing: false,
            forking: false,
            scroll: ScrollState::default(),
            unread_below: false,
            transcript_selection: None,
            activity_selection: None,
            activity_detail_scroll: 0,
            remote_head: None,
        }
    }

    fn new_virtual(id: String, title: String, turn_options: TurnOptions, status: String) -> Self {
        let mut state = Self::new(id, title, turn_options, status);
        state.automatic_title = true;
        state.virtual_conversation = true;
        state.remote_title = None;
        state
    }

    fn apply_load(&mut self, load: ConversationLoad, current_user: &str) {
        self.virtual_conversation = false;
        if self
            .reference_notice
            .as_ref()
            .is_some_and(|notice| notice.head != load.snapshot.head)
        {
            self.reference_notice = None;
        }
        let preserve_local_lifecycle = self.running
            && self.local_turn
            && matches!(load.snapshot.status.as_str(), "queued" | "running")
            && self
                .active_request
                .as_ref()
                .is_none_or(|request| load.snapshot.request.as_ref() == Some(request));
        let previous_activity_len = self.activities.len();
        let followed_activity_tail = self
            .activity_selection
            .is_none_or(|selected| selected + 1 == previous_activity_len);
        let previous_activity_selection = self.activity_selection;
        let previous_activity_detail_scroll = self.activity_detail_scroll;
        let local_lifecycle = preserve_local_lifecycle.then(|| {
            (
                self.status.clone(),
                self.turn_phase,
                self.reconciling_request.clone(),
                self.reconcile_after,
            )
        });
        self.apply_snapshot(&load.snapshot);
        self.activities = replayed_activities(&load.replay.activity);
        if followed_activity_tail && self.activities.len() > previous_activity_len {
            self.activity_selection = self.activities.len().checked_sub(1);
            self.activity_detail_scroll = 0;
        } else if previous_activity_selection
            .is_some_and(|selected| selected < self.activities.len())
        {
            self.activity_selection = previous_activity_selection;
            self.activity_detail_scroll = previous_activity_detail_scroll;
        } else {
            self.activity_selection = self.activities.len().checked_sub(1);
            self.activity_detail_scroll = 0;
        }
        let mut transcript: Vec<_> = load
            .replay
            .turns
            .into_iter()
            .map(|turn| TranscriptEntry {
                role: match turn.role {
                    ConversationRole::Human if turn.author != current_user => {
                        EntryRole::Peer(turn.author)
                    }
                    ConversationRole::Human => EntryRole::Human,
                    ConversationRole::Agent => EntryRole::Agent(turn.model),
                },
                commit: Some(turn.commit),
                text: turn.message,
                pending_id: None,
            })
            .collect();
        self.pending_submissions.retain(|pending| {
            pending.commit.as_ref().is_none_or(|commit| {
                !transcript
                    .iter()
                    .any(|entry| entry.commit.as_deref() == Some(commit))
            })
        });
        transcript.extend(
            self.pending_submissions
                .iter()
                .map(|pending| TranscriptEntry {
                    role: EntryRole::Human,
                    commit: None,
                    text: pending.text.clone(),
                    pending_id: Some(pending.id),
                }),
        );
        self.transcript = transcript;
        self.diff = Some(load.workspace_diff);
        self.remote_head = Some(load.snapshot.head);
        self.transcript_selection = None;
        if let Some((status, turn_phase, reconciling_request, reconcile_after)) = local_lifecycle {
            self.status = status;
            self.turn_phase = turn_phase;
            self.local_turn = true;
            self.reconciling_request = reconciling_request;
            self.reconcile_after = reconcile_after;
        }
    }

    fn reload(
        &mut self,
        transport: &GitTransport,
        current_user: &str,
    ) -> Option<ConversationSnapshot> {
        match conversation_load(transport, &self.id) {
            Ok(Some(load)) => {
                let snapshot = load.snapshot.clone();
                self.apply_load(load, current_user);
                return Some(snapshot);
            }
            Ok(None) => {
                self.reference_notice = None;
                self.transcript = self
                    .pending_submissions
                    .iter()
                    .map(|pending| TranscriptEntry {
                        role: EntryRole::Human,
                        commit: None,
                        text: pending.text.clone(),
                        pending_id: Some(pending.id),
                    })
                    .collect();
                self.activities.clear();
                self.activity_selection = None;
                self.activity_detail_scroll = 0;
                self.diff = None;
                self.remote_head = None;
            }
            Err(error) => {
                self.push_error(format!("loading conversation failed: {error}"));
            }
        }
        self.transcript_selection = None;
        None
    }

    fn apply_snapshot(&mut self, snapshot: &ConversationSnapshot) {
        self.running = matches!(snapshot.status.as_str(), "queued" | "running");
        self.active_request = self.running.then(|| snapshot.request.clone()).flatten();
        self.status = match snapshot.status.as_str() {
            "queued" => "queued".to_string(),
            "running" => "agent running".to_string(),
            "idle" if snapshot.interrupted => {
                format!("interrupted {}", short_hash(&snapshot.head))
            }
            "idle" => format!("updated {}", short_hash(&snapshot.head)),
            other => other.to_string(),
        };
        if !self.running {
            self.interrupting = false;
            self.reconciling_request = None;
            self.reconcile_after = None;
            self.local_turn = false;
        }
    }

    fn current_hash(&self) -> Option<&str> {
        self.transcript
            .iter()
            .rev()
            .find_map(|entry| entry.commit.as_deref())
    }

    fn is_busy(&self) -> bool {
        self.running || self.publishing || self.forking
    }

    fn push_error(&mut self, error: impl Into<String>) {
        self.note_transcript_append();
        self.status.clear();
        self.transcript.push(TranscriptEntry {
            role: EntryRole::Notice,
            commit: None,
            text: error.into(),
            pending_id: None,
        });
        self.transcript_selection = None;
    }

    fn push_info(&mut self, message: impl Into<String>) {
        self.note_transcript_append();
        self.status.clear();
        self.transcript.push(TranscriptEntry {
            role: EntryRole::Info,
            commit: None,
            text: message.into(),
            pending_id: None,
        });
        self.transcript_selection = None;
    }

    fn show_command_error(&mut self, error: impl Into<String>) {
        self.command_error = Some(error.into());
        self.status.clear();
        self.transcript_selection = None;
    }

    fn show_command_error_preserving_status(&mut self, error: impl Into<String>) {
        self.command_error = Some(error.into());
        self.transcript_selection = None;
    }

    fn note_transcript_append(&mut self) {
        if self.scroll.offset.is_some() {
            self.unread_below = true;
        }
    }

    fn queue_pending_submission(&mut self, text: String) -> u64 {
        let id = self.next_pending_submission;
        self.next_pending_submission = self.next_pending_submission.wrapping_add(1);
        self.pending_submissions.push(PendingSubmission {
            id,
            text: text.clone(),
            commit: None,
        });
        self.transcript.push(TranscriptEntry {
            role: EntryRole::Human,
            commit: None,
            text,
            pending_id: Some(id),
        });
        id
    }

    fn mark_pending_submission(&mut self, id: u64, commit: String) {
        if self
            .transcript
            .iter()
            .any(|entry| entry.commit.as_deref() == Some(commit.as_str()))
        {
            self.discard_pending_submission(id);
            return;
        }
        if let Some(pending) = self
            .pending_submissions
            .iter_mut()
            .find(|pending| pending.id == id)
        {
            pending.commit = Some(commit);
        }
        let durable_is_visible = self
            .pending_submissions
            .iter()
            .find(|pending| pending.id == id)
            .and_then(|pending| pending.commit.as_deref())
            .is_some_and(|commit| {
                self.transcript
                    .iter()
                    .any(|entry| entry.commit.as_deref() == Some(commit))
            });
        if durable_is_visible {
            self.discard_pending_submission(id);
        }
    }

    fn discard_pending_submission(&mut self, id: u64) {
        self.pending_submissions.retain(|pending| pending.id != id);
        self.transcript.retain(|entry| entry.pending_id != Some(id));
    }

    fn restore_pending_submission(&mut self, id: u64) {
        let Some(pending) = self
            .pending_submissions
            .iter()
            .find(|pending| pending.id == id)
            .cloned()
        else {
            return;
        };
        self.discard_pending_submission(id);
        self.composer.restore_message(&pending.text);
    }

    fn follow_tail(&mut self) {
        self.scroll.follow_tail();
        self.unread_below = false;
    }

    fn apply_automatic_title(&mut self, prompt: &str) {
        if !self.automatic_title_fallback_applied {
            let fallback = automatic_title(prompt);
            if self.automatic_title {
                self.title = fallback.clone();
            }
            self.automatic_title_fallback = Some(fallback);
            self.automatic_title_fallback_applied = true;
        }
    }

    fn sidebar_text(&self, max_cells: u16) -> (String, String) {
        let detail = if self.running {
            self.running_activity()
                .map(|activity| {
                    format!("{} {}", activity.running_verb(), activity.running_summary())
                })
                .unwrap_or_else(|| self.status.clone())
        } else if self.generating_title {
            "Generating title…".to_string()
        } else if self.reference_loading || self.publishing {
            self.status.clone()
        } else if let Some(attention) = &self.sidebar_attention {
            attention.clone()
        } else {
            String::new()
        };
        (
            message_preview(&self.title, max_cells),
            message_preview(&detail, max_cells),
        )
    }

    fn running_activity(&self) -> Option<&Activity> {
        self.activities
            .iter()
            .rev()
            .find(|activity| activity.state == ActivityState::Running)
    }

    fn push_activity(&mut self, activity: Activity) {
        let followed_tail = self
            .activity_selection
            .is_none_or(|selected| selected + 1 == self.activities.len());
        self.activities.push(activity);
        if followed_tail {
            self.activity_selection = Some(self.activities.len() - 1);
            self.activity_detail_scroll = 0;
        }
    }

    fn ensure_activity_selection(&mut self) {
        if self.activity_selection.is_none() && !self.activities.is_empty() {
            self.activity_selection = Some(self.activities.len() - 1);
        }
    }
}

enum UiMessage {
    Forked {
        conversation: String,
        origin: String,
        source: String,
        result: Result<(String, Box<ConversationLoad>), String>,
    },
    Turn {
        conversation: String,
        event: TurnEvent,
    },
    Failed {
        conversation: String,
        pending_id: u64,
        error: String,
    },
    Completed {
        conversation: String,
        outcome: TurnOutcome,
    },
    Interrupted {
        conversation: String,
        result: Result<String, String>,
    },
    SubmissionCommitted {
        conversation: String,
        pending_id: u64,
        commit: String,
    },
    InterjectionRefreshed {
        conversation: String,
        observed_head: Option<String>,
        load: Result<Box<ConversationLoad>, String>,
    },
    InterjectionFailed {
        conversation: String,
        pending_id: u64,
        error: String,
    },
    TitleGenerated {
        conversation: String,
        result: Result<String, String>,
    },
    Published {
        conversation: String,
        result: Result<String, String>,
    },
    BranchPublished {
        conversation: String,
        result: Result<String, String>,
    },
    Reconciled {
        conversation: String,
        request: String,
        result: Result<(), String>,
    },
    ReferenceLoaded {
        conversation: String,
        generation: u64,
        observed_head: Option<String>,
        result: Result<(String, Option<String>), String>,
    },
    RemotePolled {
        result: Result<Vec<RemotePollEntry>, String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ConfirmAction {
    Publish {
        default_base: String,
        base_input: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shortcut {
    keys: &'static str,
    label: &'static str,
    shifted: bool,
    list_only: bool,
}

impl Shortcut {
    const fn new(keys: &'static str, label: &'static str, shifted: bool, list_only: bool) -> Self {
        Self {
            keys,
            label,
            shifted,
            list_only,
        }
    }

    fn label(self) -> &'static str {
        self.label
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaletteCommand {
    label: &'static str,
    shortcut: Shortcut,
    keywords: &'static str,
    action: AppAction,
}

const PALETTE_COMMANDS: [PaletteCommand; 10] = [
    PaletteCommand {
        label: "New conversation",
        shortcut: Shortcut::new("n", "Ctrl+N", false, false),
        keywords: "create start chat",
        action: AppAction::NewConversation,
    },
    PaletteCommand {
        label: "Check out conversation",
        shortcut: Shortcut::new("l", "Ctrl+L", false, false),
        keywords: "load workspace git",
        action: AppAction::Checkout,
    },
    PaletteCommand {
        label: "Publish pull request",
        shortcut: Shortcut::new("p", "Ctrl+P twice", false, false),
        keywords: "push pr github branch",
        action: AppAction::Publish,
    },
    PaletteCommand {
        label: "Show activity",
        shortcut: Shortcut::new("t", "Ctrl+T", false, false),
        keywords: "tools progress browser",
        action: AppAction::Activity,
    },
    PaletteCommand {
        label: "Show workspace changes",
        shortcut: Shortcut::new("q", "Ctrl+Q", false, false),
        keywords: "diff files",
        action: AppAction::Changes,
    },
    PaletteCommand {
        label: "Show available tools",
        shortcut: Shortcut::new("t", "Ctrl+Shift+T", true, false),
        keywords: "commands agent",
        action: AppAction::Tools,
    },
    PaletteCommand {
        label: "Reload conversation",
        shortcut: Shortcut::new("r", "Ctrl+R", false, false),
        keywords: "refresh history",
        action: AppAction::Reload,
    },
    PaletteCommand {
        label: "Show keyboard help",
        shortcut: Shortcut::new("h?/", "Ctrl+H", false, false),
        keywords: "shortcuts documentation",
        action: AppAction::Help,
    },
    PaletteCommand {
        label: "Archive conversation",
        shortcut: Shortcut::new("e", "Ctrl+E in list", false, true),
        keywords: "close remove",
        action: AppAction::Archive,
    },
    PaletteCommand {
        label: "Toggle native selection lock",
        shortcut: Shortcut::new("y", "Ctrl+Y", false, false),
        keywords: "copy mouse terminal freeze",
        action: AppAction::SelectionLock,
    },
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CommandPalette {
    query: String,
    selected: usize,
}

impl CommandPalette {
    fn matches(&self) -> Vec<&'static PaletteCommand> {
        let terms = self.query.split_whitespace().map(str::to_lowercase);
        let terms = terms.collect::<Vec<_>>();
        PALETTE_COMMANDS
            .iter()
            .filter(|command| {
                let searchable = format!("{} {}", command.label, command.keywords).to_lowercase();
                terms.iter().all(|term| searchable.contains(term))
            })
            .collect()
    }

    fn edit(&mut self, edit: impl FnOnce(&mut String)) {
        edit(&mut self.query);
        self.selected = 0;
    }

    fn select(&mut self, amount: isize) {
        let count = self.matches().len();
        if count > 0 {
            self.selected = (self.selected as isize + amount).rem_euclid(count as isize) as usize;
        }
    }

    fn selected_action(&self) -> Option<AppAction> {
        self.matches()
            .get(self.selected)
            .map(|command| command.action)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MouseAction {
    Ignored,
    Redraw,
    Copy(String),
}

pub(crate) struct App {
    repo_dir: PathBuf,
    user: String,
    conversations: Vec<ConversationState>,
    selected: usize,
    should_quit: bool,
    selection_locked: bool,
    confirm_action: Option<ConfirmAction>,
    palette: Option<CommandPalette>,
    selecting_transcript: bool,
    screen_selection: Option<ScreenSelection>,
    selecting_screen: bool,
    pending_conversation_click: Option<usize>,
    rendered_screen: Option<Buffer>,
    copied_chars: Option<usize>,
    animation_frame: usize,
    enhanced_keyboard: bool,
    remote_polling: bool,
    view: View,
    focus: Focus,
    tx: Sender<UiMessage>,
    rx: Receiver<UiMessage>,
}

impl App {
    pub(crate) fn new(mut args: Args) -> Result<Self, String> {
        // Fail before taking over the terminal if the repo or remote is invalid.
        let transport = GitTransport::from_cwd()?;
        let repo_dir = transport.work_dir().to_path_buf();
        args.turn
            .model
            .get_or_insert_with(|| DEFAULT_MODEL.to_string());
        if let Some(from) = args.from_commit.clone() {
            let commit = transport
                .resolve_revspec(&from)?
                .ok_or_else(|| format!("cannot resolve --from {from:?}"))?
                .to_string();
            args.from_commit = Some(commit.clone());
            args.turn.base = Some(commit);
        }
        let mut conversations =
            list_user_conversations(&transport, &args.user, UserConversationStatus::Active)?;
        let mut relist = false;
        if let Some(requested) = args.conversation.clone() {
            if args.new_conversation
                && (conversations
                    .iter()
                    .any(|conversation| conversation.id == requested)
                    || conversation_snapshot(&transport, &requested)?.is_some())
            {
                return Err(format!(
                    "--new: conversation {requested:?} already exists; omit --new to continue it"
                ));
            }
            if conversations
                .iter()
                .all(|conversation| conversation.id != requested)
            {
                let archived = list_user_conversations(
                    &transport,
                    &args.user,
                    UserConversationStatus::Archived,
                )?;
                if archived
                    .iter()
                    .any(|conversation| conversation.id == requested)
                {
                    unarchive_user_conversation(&transport, &args.user, &requested)?;
                    relist = true;
                } else if conversation_snapshot(&transport, &requested)?.is_some() {
                    invite_user_to_conversation(&transport, &args.user, &requested)?;
                    relist = true;
                }
            }
        }
        if relist {
            conversations =
                list_user_conversations(&transport, &args.user, UserConversationStatus::Active)?;
        }
        let choice = choose_conversation(
            args.conversation.as_deref(),
            args.new_conversation,
            &conversations,
        )?;
        let (tx, rx) = mpsc::channel();
        let initial_status = args
            .from_commit
            .as_deref()
            .map(|hash| format!("ready from {}", short_hash(hash)))
            .unwrap_or_else(|| "ready".to_string());
        let mut states: Vec<ConversationState> = conversations
            .iter()
            .map(|summary| {
                let mut state = ConversationState::new(
                    summary.id.clone(),
                    summary.title.clone(),
                    args.turn.clone(),
                    "ready".to_string(),
                );
                state.parent = summary.parent.clone();
                state.remote_head = Some(summary.head.clone());
                state
            })
            .collect();
        let load_selected = matches!(&choice, ConversationChoice::Existing(_));
        let selected_id = match choice {
            ConversationChoice::Existing(id) => id,
            ConversationChoice::New { id, title } => {
                let id = match id {
                    Some(id) => id,
                    None => fresh_conversation_id(&transport, &args.user)?,
                };
                states.insert(
                    0,
                    ConversationState::new_virtual(
                        id.clone(),
                        title,
                        new_conversation_options(args.turn.clone(), args.turn.base, &repo_dir)?.0,
                        initial_status,
                    ),
                );
                id
            }
        };
        let selected = states
            .iter()
            .position(|state| state.id == selected_id)
            .expect("the selected conversation was inserted");
        if load_selected {
            let _ = states[selected].reload(&transport, &args.user);
        }
        let mut app = Self {
            repo_dir,
            user: args.user,
            conversations: states,
            selected,
            should_quit: false,
            selection_locked: false,
            confirm_action: None,
            palette: None,
            selecting_transcript: false,
            screen_selection: None,
            selecting_screen: false,
            pending_conversation_click: None,
            rendered_screen: None,
            copied_chars: None,
            animation_frame: 0,
            enhanced_keyboard: false,
            remote_polling: false,
            view: View::Chat,
            focus: Focus::Conversation,
            tx,
            rx,
        };
        app.reconcile_active_requests();
        Ok(app)
    }

    fn selected(&self) -> &ConversationState {
        &self.conversations[self.selected]
    }

    fn selected_mut(&mut self) -> &mut ConversationState {
        &mut self.conversations[self.selected]
    }

    fn transport(&self) -> Result<GitTransport, String> {
        GitTransport::discover(&self.repo_dir)
    }

    /// Ensure every remotely active request has one process-local join. The
    /// guard is per request, so unchanged 500ms polls cannot create an
    /// unbounded set of HTTP waiters. Transport failures retry after a small
    /// backoff; successful joins stay guarded until the ref becomes terminal.
    fn reconcile_active_requests(&mut self) -> bool {
        let now = Instant::now();
        let repo_dir = self.repo_dir.clone();
        let tx = self.tx.clone();
        let mut started = false;
        for state in &mut self.conversations {
            if state.local_turn || !state.running {
                continue;
            }
            let Some(request) = state.active_request.clone() else {
                continue;
            };
            if state.reconciling_request.as_deref() == Some(&request)
                || state.reconcile_after.is_some_and(|after| after > now)
            {
                continue;
            }
            state.reconciling_request = Some(request.clone());
            state.reconcile_after = None;
            let conversation = state.id.clone();
            let repo_dir = repo_dir.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = GitTransport::discover(repo_dir)
                    .and_then(|transport| resume_request(&transport, &request));
                let _ = tx.send(UiMessage::Reconciled {
                    conversation,
                    request,
                    result,
                });
            });
            started = true;
        }
        started
    }

    pub(crate) fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub(crate) fn selection_locked(&self) -> bool {
        self.selection_locked
    }

    pub(crate) fn enhanced_keyboard(&self) -> bool {
        self.enhanced_keyboard
    }

    pub(crate) fn set_enhanced_keyboard(&mut self, supported: bool) {
        self.enhanced_keyboard = supported;
    }

    pub(crate) fn clear_copy_notice(&mut self) {
        self.copied_chars = None;
    }

    pub(crate) fn note_copy(&mut self, text: &str) {
        self.copied_chars = Some(text.chars().count());
    }

    pub(crate) fn capture_screen(&mut self, buffer: &Buffer) {
        self.rendered_screen = Some(buffer.clone());
    }

    pub(crate) fn has_visible_animation(&self) -> bool {
        self.selected().is_busy()
    }

    pub(crate) fn advance_animation(&mut self) {
        self.animation_frame = (self.animation_frame + 1) % ui::ACTIVITY_INDICATORS.len();
    }

    pub(crate) fn view(&self) -> View {
        self.view
    }

    pub(crate) fn focus(&self) -> Focus {
        self.focus
    }

    pub(crate) fn showing_transcript(&self) -> bool {
        self.view == View::Chat
    }

    pub(crate) fn insert_paste(&mut self, text: &str) {
        self.selected_mut().composer.insert_paste(text);
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> MouseAction {
        if self.palette.is_some() || self.confirm_action.is_some() {
            return MouseAction::Ignored;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(value) = ui::reference_copy_at(self, area, mouse.column, mouse.row) {
                self.selecting_transcript = false;
                self.selecting_screen = false;
                self.screen_selection = None;
                self.selected_mut().transcript_selection = None;
                return MouseAction::Copy(value);
            }
        }
        match mouse.kind {
            MouseEventKind::ScrollUp
                if self.view == View::Activity
                    && ui::content_contains(self.selected(), area, mouse.column, mouse.row) =>
            {
                self.scroll_activity_details_up(3);
                MouseAction::Redraw
            }
            MouseEventKind::ScrollDown
                if self.view == View::Activity
                    && ui::content_contains(self.selected(), area, mouse.column, mouse.row) =>
            {
                self.scroll_activity_details_down(3);
                MouseAction::Redraw
            }
            MouseEventKind::ScrollUp
                if self.showing_transcript()
                    && ui::transcript_contains(self.selected(), area, mouse.column, mouse.row) =>
            {
                self.selected_mut().transcript_selection = None;
                self.scroll_up(3);
                MouseAction::Redraw
            }
            MouseEventKind::ScrollDown
                if self.showing_transcript()
                    && ui::transcript_contains(self.selected(), area, mouse.column, mouse.row) =>
            {
                self.selected_mut().transcript_selection = None;
                self.scroll_down(3);
                MouseAction::Redraw
            }
            MouseEventKind::Down(MouseButton::Left) if self.showing_transcript() => {
                if let Some(point) =
                    ui::transcript_point(self.selected(), area, mouse.column, mouse.row)
                {
                    self.screen_selection = None;
                    self.selected_mut().transcript_selection = Some(TranscriptSelection {
                        anchor: point,
                        head: point,
                    });
                    self.selecting_transcript = true;
                    return MouseAction::Redraw;
                }
                self.start_screen_selection(mouse.column, mouse.row, area)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selecting_transcript => {
                if self.selected().transcript_selection.is_none() {
                    self.selecting_transcript = false;
                    return MouseAction::Ignored;
                }
                if let Some(point) =
                    ui::transcript_point(self.selected(), area, mouse.column, mouse.row)
                {
                    if let Some(selection) = self.selected_mut().transcript_selection.as_mut() {
                        selection.head = point;
                    }
                    MouseAction::Redraw
                } else {
                    MouseAction::Ignored
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.selecting_transcript => {
                if self.selected().transcript_selection.is_none() {
                    self.selecting_transcript = false;
                    return MouseAction::Ignored;
                }
                if let Some(point) =
                    ui::transcript_point(self.selected(), area, mouse.column, mouse.row)
                {
                    if let Some(selection) = self.selected_mut().transcript_selection.as_mut() {
                        selection.head = point;
                    }
                }
                self.selecting_transcript = false;
                ui::transcript_selection_text(self.selected(), area)
                    .map(MouseAction::Copy)
                    .unwrap_or(MouseAction::Redraw)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.start_screen_selection(mouse.column, mouse.row, area)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selecting_screen => {
                let point = screen_point(mouse.column, mouse.row, area);
                if let Some(selection) = self.screen_selection.as_mut() {
                    selection.head = point;
                    if selection.anchor != point {
                        self.pending_conversation_click = None;
                    }
                }
                MouseAction::Redraw
            }
            MouseEventKind::Up(MouseButton::Left) if self.selecting_screen => {
                let point = screen_point(mouse.column, mouse.row, area);
                if let Some(selection) = self.screen_selection.as_mut() {
                    selection.head = point;
                }
                self.selecting_screen = false;
                if let Some(index) = self.pending_conversation_click.take() {
                    if self
                        .screen_selection
                        .is_some_and(|selection| selection.anchor == selection.head)
                    {
                        self.screen_selection = None;
                        self.select(index);
                        return MouseAction::Redraw;
                    }
                }
                self.screen_selection_text()
                    .map(MouseAction::Copy)
                    .unwrap_or(MouseAction::Redraw)
            }
            _ => MouseAction::Ignored,
        }
    }

    fn start_screen_selection(&mut self, column: u16, row: u16, area: Rect) -> MouseAction {
        let point = screen_point(column, row, area);
        self.selected_mut().transcript_selection = None;
        self.screen_selection = Some(ScreenSelection {
            anchor: point,
            head: point,
        });
        self.selecting_screen = true;
        self.pending_conversation_click = ui::conversation_at(self, area, column, row);
        MouseAction::Redraw
    }

    fn screen_selection_text(&self) -> Option<String> {
        let selection = self.screen_selection?;
        let buffer = self.rendered_screen.as_ref()?;
        let (start, end) = selection.ordered();
        let mut rows = Vec::new();
        for row in start.row..=end.row.min(buffer.area.bottom().saturating_sub(1)) {
            let start_column = if row == start.row { start.column } else { 0 };
            let end_column = if row == end.row {
                end.column
            } else {
                buffer.area.right().saturating_sub(1)
            };
            let mut text = String::new();
            for column in start_column..=end_column.min(buffer.area.right().saturating_sub(1)) {
                if let Some(cell) = buffer.cell((column, row)) {
                    text.push_str(cell.symbol());
                }
            }
            rows.push(text.trim_end().to_string());
        }
        let text = rows.join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn start_turn(&mut self) {
        if self.selected().forking {
            self.selected_mut()
                .show_command_error("wait for this conversation fork to finish");
            return;
        }
        if self.selected().publishing {
            self.selected_mut()
                .show_command_error("finish publishing before sending another message");
            return;
        }
        let interjecting = self.selected().running;
        let Some(raw) = self.selected_mut().composer.take_message() else {
            return;
        };
        let state = self.selected_mut();
        state.reference_notice = None;
        state.reference_loading = false;
        if state.status == "loading conversation reference" {
            state.status.clear();
        }
        // Recognized local commands stop here as one class. Unrecognized slash
        // text and message-submitting commands continue through the ordinary
        // turn path.
        let mut human_tree = None;
        let message = if let Some((command, arguments)) = parse_command(&raw) {
            if command.takes_argument == arguments.is_empty() {
                self.selected_mut()
                    .show_command_error(format!("usage: {}", command.usage));
                return;
            }
            if !command.action.submits_message() {
                self.run_local_command(command, arguments);
                return;
            }
            match commit_working_tree(arguments, &self.repo_dir) {
                Ok(tree) => human_tree = Some(tree),
                Err(error) => {
                    self.selected_mut().show_command_error(error);
                    return;
                }
            }
            arguments.to_string()
        } else {
            raw
        };
        let should_generate_title =
            !interjecting && self.selected().automatic_title && !self.selected().generating_title;
        let observed_head = self.selected().remote_head.clone();
        let pending_id = if interjecting {
            let state = self.selected_mut();
            let pending_id = state.queue_pending_submission(message.clone());
            state.follow_tail();
            state.transcript_selection = None;
            pending_id
        } else {
            let state = self.selected_mut();
            state.apply_automatic_title(&message);
            if should_generate_title {
                state.generating_title = true;
            }
            let pending_id = state.queue_pending_submission(message.clone());
            state.activities.clear();
            state.activity_selection = None;
            state.activity_detail_scroll = 0;
            state.running = true;
            state.local_turn = true;
            state.sidebar_attention = None;
            state.turn_phase = TurnPhase::System;
            state.status = "starting turn".to_string();
            state.follow_tail();
            state.transcript_selection = None;
            pending_id
        };

        if should_generate_title {
            self.publish_automatic_title_fallback();
        }

        let tx = self.tx.clone();
        let options = self.selected().turn_options.clone();
        let conversation = self.selected().id.clone();
        let repo_dir = self.repo_dir.clone();
        if should_generate_title {
            let title_tx = tx.clone();
            let title_options = options.clone();
            let title_conversation = conversation.clone();
            let title_repo_dir = repo_dir.clone();
            let first_message = message.clone();
            std::thread::spawn(move || {
                let result = GitTransport::discover(title_repo_dir).and_then(|transport| {
                    generate_conversation_title(&transport, &title_options, &first_message)
                });
                let _ = title_tx.send(UiMessage::TitleGenerated {
                    conversation: title_conversation,
                    result,
                });
            });
        }
        if interjecting {
            std::thread::spawn(move || {
                let transport = match GitTransport::discover(repo_dir) {
                    Ok(transport) => transport,
                    Err(error) => {
                        let _ = tx.send(UiMessage::InterjectionFailed {
                            conversation,
                            pending_id,
                            error,
                        });
                        return;
                    }
                };
                match submit_interjection(
                    &transport,
                    &options,
                    &conversation,
                    &message,
                    human_tree.as_deref(),
                ) {
                    Ok(commit) => {
                        let _ = tx.send(UiMessage::SubmissionCommitted {
                            conversation: conversation.clone(),
                            pending_id,
                            commit,
                        });
                        let load = conversation_load(&transport, &conversation)
                            .and_then(|load| {
                                load.ok_or_else(|| {
                                    format!(
                                        "conversation {conversation:?} disappeared after submit"
                                    )
                                })
                            })
                            .map(Box::new);
                        let _ = tx.send(UiMessage::InterjectionRefreshed {
                            conversation,
                            observed_head,
                            load,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(UiMessage::InterjectionFailed {
                            conversation,
                            pending_id,
                            error,
                        });
                    }
                }
            });
            return;
        }
        std::thread::spawn(move || {
            let result = GitTransport::discover(repo_dir).and_then(|transport| {
                let outcome = run_chat_turn(
                    &transport,
                    &options,
                    &conversation,
                    &message,
                    human_tree.as_deref(),
                    |commit| {
                        let _ = tx.send(UiMessage::SubmissionCommitted {
                            conversation: conversation.clone(),
                            pending_id,
                            commit: commit.to_string(),
                        });
                    },
                    |event| {
                        let _ = tx.send(UiMessage::Turn {
                            conversation: conversation.clone(),
                            event,
                        });
                    },
                )?;
                let _ = tx.send(UiMessage::Completed {
                    conversation: conversation.clone(),
                    outcome,
                });
                Ok(())
            });
            if let Err(error) = result {
                let _ = tx.send(UiMessage::Failed {
                    conversation,
                    pending_id,
                    error,
                });
            }
        });
    }

    fn show_selected_ref(&mut self) {
        self.start_reference_lookup(self.selected);
    }

    fn run_local_command(&mut self, command: &Command, arguments: &str) {
        debug_assert!(!command.action.submits_message());
        match command.action {
            AppAction::Help | AppAction::Commands => self.execute_action(command.action),
            AppAction::Reference => self.show_selected_ref(),
            AppAction::Invite => self.invite_selected(arguments),
            AppAction::Model => {
                if arguments.split_whitespace().count() != 1 {
                    self.selected_mut()
                        .show_command_error(format!("usage: {}", command.usage));
                    return;
                }
                let model = if arguments == "default" {
                    DEFAULT_MODEL.to_string()
                } else {
                    arguments.to_string()
                };
                for state in &mut self.conversations {
                    state.turn_options.model = Some(model.clone());
                }
                self.selected_mut()
                    .push_info(format!("Model for future turns: {model}"));
            }
            AppAction::From => self.start_from_hash(arguments),
            AppAction::PublishBranch => self.publish_branch_selected(),
            AppAction::Title => self.rename_selected(arguments),
            AppAction::UpdateTree => unreachable!("message command reached local dispatch"),
            AppAction::NewConversation
            | AppAction::Checkout
            | AppAction::Publish
            | AppAction::Activity
            | AppAction::Changes
            | AppAction::Tools
            | AppAction::Reload
            | AppAction::Archive
            | AppAction::SelectionLock => unreachable!("palette-only action has no slash command"),
        }
    }

    fn publish_automatic_title_fallback(&mut self) {
        let state = self.selected();
        let Some(fallback) = state.automatic_title_fallback.clone() else {
            return;
        };
        let Some(expected) = state.remote_title.clone() else {
            return;
        };
        if expected == fallback {
            return;
        }
        let id = state.id.clone();
        match self.transport().and_then(|transport| {
            compare_and_set_conversation_title(&transport, &id, &expected, &fallback)
        }) {
            Ok(true) => self.selected_mut().remote_title = Some(fallback),
            Ok(false) => {}
            Err(error) => {
                self.selected_mut().sidebar_attention =
                    Some(format!("Failed to save conversation title: {error}"));
            }
        }
    }

    fn interrupt_selected(&mut self) {
        if !self.selected().running || self.selected().interrupting {
            return;
        }
        let wait_for_admission = self.selected().active_request.is_none();
        let conversation = self.selected().id.clone();
        let repo_dir = self.repo_dir.clone();
        let tx = self.tx.clone();
        self.selected_mut().interrupting = true;
        self.selected_mut().status = "stopping agent".to_string();
        std::thread::spawn(move || {
            let result = GitTransport::discover(repo_dir).and_then(|transport| {
                let attempts = if wait_for_admission { 40 } else { 1 };
                let mut last_error = None;
                for attempt in 0..attempts {
                    match interrupt_request(&transport, &conversation) {
                        Ok(commit) => return Ok(commit),
                        Err(error) => last_error = Some(error),
                    }
                    if attempt + 1 < attempts {
                        std::thread::sleep(Duration::from_millis(125));
                    }
                }
                Err(last_error.unwrap_or_else(|| "recording Escape failed".to_string()))
            });
            let _ = tx.send(UiMessage::Interrupted {
                conversation,
                result,
            });
        });
    }

    fn start_reference_lookup(&mut self, index: usize) {
        let state = &mut self.conversations[index];
        if state.reference_loading {
            return;
        }
        state.reference_loading = true;
        state.reference_generation = state.reference_generation.wrapping_add(1);
        state.reference_notice = None;
        state.command_error = None;
        state.status = "loading conversation reference".to_string();
        let conversation = state.id.clone();
        let generation = state.reference_generation;
        let observed_head = state.remote_head.clone();
        let repo_dir = self.repo_dir.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = GitTransport::discover(repo_dir)
                .and_then(|transport| conversation_reference(&transport, &conversation));
            let _ = tx.send(UiMessage::ReferenceLoaded {
                conversation,
                generation,
                observed_head,
                result,
            });
        });
    }

    fn invite_selected(&mut self, user: &str) {
        if self.selected().virtual_conversation {
            self.selected_mut().push_info(format!(
                "Send the first message before inviting username {user:?}."
            ));
            return;
        }
        let id = self.selected().id.clone();
        match self
            .transport()
            .and_then(|transport| invite_user_to_conversation(&transport, user, &id))
        {
            Ok(InviteOutcome::Created) => self.selected_mut().push_info(format!(
                "Invited username {user:?}. They must select that exact case-sensitive identity."
            )),
            Ok(InviteOutcome::AlreadyActive) => self.selected_mut().push_info(format!(
                "Username {user:?} already has this conversation active."
            )),
            Ok(InviteOutcome::Archived) => self.selected_mut().push_info(format!(
                "Username {user:?} has archived this conversation; their choice was preserved."
            )),
            Err(error) => self.selected_mut().show_command_error(error),
        }
    }

    pub(crate) fn drain_messages(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.rx.try_recv() {
            changed = true;
            match message {
                UiMessage::Forked {
                    conversation,
                    origin,
                    source,
                    result,
                } => self.finish_fork(&conversation, &origin, &source, result),
                UiMessage::Turn {
                    conversation,
                    event,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        self.on_turn_event(index, event);
                    }
                }
                UiMessage::Failed {
                    conversation,
                    pending_id,
                    error,
                } => {
                    let transport = self.transport();
                    let user = self.user.clone();
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        state.local_turn = false;
                        let refreshed = transport
                            .as_ref()
                            .ok()
                            .and_then(|transport| state.reload(transport, &user));
                        if refreshed.is_none() {
                            state.running = false;
                            state.active_request = None;
                            state.reconciling_request = None;
                            state.remote_head = None;
                        }
                        state.restore_pending_submission(pending_id);
                        if state.running {
                            state.sidebar_attention =
                                Some("Local follower failed — turn still active".to_string());
                            state.show_command_error_preserving_status(format!(
                                "following turn failed: {error}"
                            ));
                        } else {
                            state.sidebar_attention = Some("Failed — open for details".to_string());
                            state.push_error(error);
                            state.status = "turn failed".to_string();
                        }
                    }
                }
                UiMessage::Completed {
                    conversation,
                    outcome,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        self.finish_turn(index, outcome);
                    }
                }
                UiMessage::Interrupted {
                    conversation,
                    result,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        match result {
                            Ok(_) if state.running => state.status = "stopping agent".to_string(),
                            Ok(_) => state.interrupting = false,
                            Err(error) if state.running => {
                                state.interrupting = false;
                                state.show_command_error_preserving_status(format!(
                                    "interrupting turn failed: {error}"
                                ));
                            }
                            Err(_) => state.interrupting = false,
                        }
                    }
                }
                UiMessage::SubmissionCommitted {
                    conversation,
                    pending_id,
                    commit,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        state.virtual_conversation = false;
                        state.mark_pending_submission(pending_id, commit);
                    }
                }
                UiMessage::InterjectionRefreshed {
                    conversation,
                    observed_head,
                    load,
                } => {
                    let user = self.user.clone();
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        if state.remote_head != observed_head {
                            continue;
                        }
                        match load {
                            Ok(load) => state.apply_load(*load, &user),
                            Err(error) => {
                                state.remote_head = None;
                                state.show_command_error_preserving_status(format!(
                                    "message saved, but refreshing it failed: {error}"
                                ));
                            }
                        }
                    }
                }
                UiMessage::InterjectionFailed {
                    conversation,
                    pending_id,
                    error,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        state.restore_pending_submission(pending_id);
                        state.show_command_error_preserving_status(format!(
                            "sending message failed: {error}"
                        ));
                    }
                }
                UiMessage::TitleGenerated {
                    conversation,
                    result,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        self.finish_title_generation(index, result);
                    }
                }
                UiMessage::Published {
                    conversation,
                    result,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        state.publishing = false;
                        match result {
                            Ok(url) => state.push_info(format!("PR ready: {url}")),
                            Err(error) => {
                                state.sidebar_attention =
                                    Some("PR failed — open for details".to_string());
                                state.show_command_error(format!("PR failed: {error}"));
                            }
                        }
                    }
                }
                UiMessage::BranchPublished {
                    conversation,
                    result,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        state.publishing = false;
                        match result {
                            Ok(branch) => state
                                .push_info(format!("Conversation branch ready: origin/{branch}")),
                            Err(error) => {
                                state.sidebar_attention =
                                    Some("Branch publish failed — open for details".to_string());
                                state.show_command_error(format!(
                                    "publishing conversation branch failed: {error}"
                                ));
                            }
                        }
                    }
                }
                UiMessage::Reconciled {
                    conversation,
                    request,
                    result,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        if state.reconciling_request.as_deref() != Some(&request) {
                            continue;
                        }
                        if let Err(error) = result {
                            state.reconciling_request = None;
                            state.reconcile_after = Some(Instant::now() + Duration::from_secs(5));
                            state.status = format!("recovery retry pending: {error}");
                        }
                    }
                }
                UiMessage::ReferenceLoaded {
                    conversation,
                    generation,
                    observed_head,
                    result,
                } => {
                    let Some(index) = self.conversation_index(&conversation) else {
                        continue;
                    };
                    if !self.conversations[index].reference_loading
                        || self.conversations[index].reference_generation != generation
                    {
                        continue;
                    }
                    let retry = {
                        let state = &mut self.conversations[index];
                        state.reference_loading = false;
                        if state.status == "loading conversation reference" {
                            state.status.clear();
                        }
                        match result {
                            Ok((_refname, Some(head)))
                                if state.remote_head != observed_head
                                    && state.remote_head.as_deref() != Some(head.as_str()) =>
                            {
                                true
                            }
                            Ok((refname, Some(head))) => {
                                state.reference_notice = Some(ReferenceNotice { refname, head });
                                false
                            }
                            Ok((_refname, None)) if state.remote_head != observed_head => true,
                            Ok((_refname, None)) => {
                                state.reference_notice = None;
                                state.show_command_error(
                                    "this conversation has no remote ref until its first message",
                                );
                                false
                            }
                            Err(error) => {
                                state.reference_notice = None;
                                state.show_command_error(error);
                                false
                            }
                        }
                    };
                    if retry {
                        self.start_reference_lookup(index);
                    }
                }
                UiMessage::RemotePolled { result } => {
                    self.remote_polling = false;
                    if let Ok(entries) = result {
                        changed |= self.apply_remote_poll(entries);
                    }
                }
            }
        }
        changed |= self.reconcile_active_requests();
        changed
    }

    /// Schedule a canonical-ref refresh so network and Git work never block the
    /// input/render loop. At most one refresh may be in flight; its versioned
    /// result is applied from `drain_messages` only if the local state has not
    /// advanced in the meantime.
    pub(crate) fn poll_remote(&mut self) {
        if self.remote_polling {
            return;
        }
        self.remote_polling = true;
        let observed: HashMap<_, _> = self
            .conversations
            .iter()
            .map(|state| {
                (
                    state.id.clone(),
                    (
                        state.remote_head.clone(),
                        state.remote_title.clone(),
                        state.forking,
                    ),
                )
            })
            .collect();
        let repo_dir = self.repo_dir.clone();
        let user = self.user.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = GitTransport::discover(repo_dir).and_then(|transport| {
                let summaries =
                    list_user_conversations(&transport, &user, UserConversationStatus::Active)?;
                Ok(summaries
                    .into_iter()
                    .map(|summary| {
                        let (observed_head, observed_title, forking) = observed
                            .get(&summary.id)
                            .cloned()
                            .unwrap_or((None, None, false));
                        let load = (!forking
                            && observed_head.as_deref() != Some(summary.head.as_str()))
                        .then(|| {
                            conversation_load(&transport, &summary.id)
                                .and_then(|load| {
                                    load.ok_or_else(|| {
                                        format!(
                                            "conversation {:?} disappeared during refresh",
                                            summary.id
                                        )
                                    })
                                })
                                .map(Box::new)
                        });
                        RemotePollEntry {
                            summary,
                            observed_head,
                            observed_title,
                            load,
                        }
                    })
                    .collect())
            });
            let _ = tx.send(UiMessage::RemotePolled { result });
        });
    }

    fn apply_remote_poll(&mut self, entries: Vec<RemotePollEntry>) -> bool {
        let mut changed = false;
        for entry in entries {
            if let Some(index) = self.conversation_index(&entry.summary.id) {
                let state = &mut self.conversations[index];
                if state.parent != entry.summary.parent {
                    state.parent = entry.summary.parent.clone();
                    changed = true;
                }
                if state.forking
                    || state.remote_head != entry.observed_head
                    || state.remote_title != entry.observed_title
                {
                    continue;
                }
                if state.remote_title.as_deref() != Some(entry.summary.title.as_str()) {
                    let first_automatic_publication = state.generating_title
                        && state.automatic_title
                        && entry.observed_title.is_none()
                        && state.automatic_title_fallback.as_deref()
                            == Some(entry.summary.title.as_str());
                    state.remote_title = Some(entry.summary.title.clone());
                    if !first_automatic_publication {
                        state.title = entry.summary.title.clone();
                        state.automatic_title = false;
                    }
                    changed = true;
                }
                if let Some(load) = entry.load {
                    match load {
                        Ok(load) => state.apply_load(*load, &self.user),
                        Err(error) => {
                            state.remote_head = None;
                            state.push_error(format!("loading conversation failed: {error}"));
                        }
                    }
                    changed = true;
                }
            } else if let Some(Ok(load)) = entry.load {
                let mut state = ConversationState::new(
                    entry.summary.id.clone(),
                    entry.summary.title,
                    self.selected().turn_options.clone(),
                    "shared conversation".to_string(),
                );
                state.parent = entry.summary.parent;
                state.apply_load(*load, &self.user);
                let insert_at = state
                    .parent
                    .as_deref()
                    .and_then(|parent| self.conversation_index(parent))
                    .map(|parent_index| {
                        let mut index = parent_index + 1;
                        while self
                            .conversations
                            .get(index)
                            .is_some_and(|candidate| candidate.parent == state.parent)
                        {
                            index += 1;
                        }
                        index
                    })
                    .unwrap_or(0);
                self.conversations.insert(insert_at, state);
                if insert_at <= self.selected {
                    self.selected += 1;
                }
                changed = true;
            }
        }
        changed
    }

    fn conversation_index(&self, id: &str) -> Option<usize> {
        self.conversations.iter().position(|state| state.id == id)
    }

    fn on_turn_event(&mut self, index: usize, event: TurnEvent) {
        let state = &mut self.conversations[index];
        match event {
            TurnEvent::PhaseStarted(phase) => state.turn_phase = phase,
            TurnEvent::PhaseComplete {
                label,
                elapsed_secs,
            } => state.status = format!("{label}: {elapsed_secs:.1}s"),
            TurnEvent::Status(status) => state.status = status,
            TurnEvent::ToolCall {
                step_commit,
                request,
                round,
                tool_use_id,
                name,
                summary,
            } => {
                state.push_activity(Activity {
                    request,
                    round,
                    id: tool_use_id,
                    step_commit,
                    name,
                    summary,
                    detail: String::new(),
                    state: ActivityState::Running,
                });
            }
            TurnEvent::ToolResult {
                step_commit,
                request,
                round,
                tool_use_id,
                is_error,
                content,
            } => {
                if let Some(activity) = state
                    .activities
                    .iter_mut()
                    .find(|activity| activity.answers(&request, round, &tool_use_id))
                {
                    activity.state = if is_error {
                        ActivityState::Failed
                    } else {
                        ActivityState::Succeeded
                    };
                    activity.detail = content;
                } else {
                    state.push_activity(Activity {
                        request,
                        round,
                        id: tool_use_id.clone(),
                        step_commit,
                        name: "result".to_string(),
                        summary: format!("result {tool_use_id}"),
                        detail: content,
                        state: if is_error {
                            ActivityState::Failed
                        } else {
                            ActivityState::Succeeded
                        },
                    });
                }
            }
        }
    }

    fn finish_turn(&mut self, index: usize, outcome: TurnOutcome) {
        let transport = self.transport();
        let user = self.user.clone();
        let state = &mut self.conversations[index];
        state.running = false;
        state.interrupting = false;
        state.local_turn = false;
        state.active_request = None;
        state.reconciling_request = None;
        state.reconcile_after = None;
        state.sidebar_attention = None;
        state.status = if outcome.interrupted {
            format!("interrupted {}", outcome.short_commit)
        } else {
            format!("completed {}", outcome.short_commit)
        };
        match transport {
            Ok(transport) => {
                let published_title = state.title.clone();
                let initial_title = state
                    .automatic_title_fallback
                    .as_deref()
                    .unwrap_or(&published_title);
                match publish_user_conversation(&transport, &user, &state.id, initial_title) {
                    Ok(()) => {
                        if let Some(fallback) = state.automatic_title_fallback.clone() {
                            state.remote_title.get_or_insert_with(|| fallback.clone());
                            if !state.automatic_title
                                && state.title != fallback
                                && compare_and_set_conversation_title(
                                    &transport,
                                    &state.id,
                                    &fallback,
                                    &state.title,
                                )
                                .unwrap_or(false)
                            {
                                state.remote_title = Some(state.title.clone());
                                state.automatic_title_fallback = None;
                            }
                        }
                        let _ = state.reload(&transport, &user);
                        state.remote_head = Some(outcome.commit);
                    }
                    Err(error) => {
                        state.sidebar_attention = Some("Failed to save conversation".to_string());
                        state.push_error(format!(
                            "publishing completed conversation failed: {error}"
                        ));
                    }
                }
            }
            Err(error) => {
                state.sidebar_attention = Some("Failed to reload conversation".to_string());
                state.push_error(format!("reloading completed turn failed: {error}"));
            }
        }
    }

    fn finish_fork(
        &mut self,
        conversation: &str,
        _origin: &str,
        source: &str,
        result: Result<(String, Box<ConversationLoad>), String>,
    ) {
        let Some(index) = self.conversation_index(conversation) else {
            return;
        };
        match result {
            Ok((fork, load)) => {
                let remote_title = load.snapshot.title.clone();
                let state = &mut self.conversations[index];
                state.forking = false;
                state.apply_load(*load, &self.user);
                state.remote_head = Some(fork);
                state.remote_title = Some(remote_title);
                state.status = format!("forked from {}", short_hash(source));
            }
            Err(error) => {
                // Never discard text typed while the fork was in flight. Turn
                // the placeholder into an ordinary new conversation in place,
                // independent of how many other tabs happen to be open.
                let state = &self.conversations[index];
                let id = state.id.clone();
                let title = state.title.clone();
                let composer = state.composer.clone();
                match new_conversation_options(state.turn_options.clone(), None, &self.repo_dir) {
                    Ok((options, base)) => {
                        let mut replacement = ConversationState::new_virtual(
                            id,
                            title,
                            options,
                            format!("ready from {}; enter a prompt", short_hash(&base)),
                        );
                        replacement.composer = composer;
                        replacement.show_command_error(format!(
                            "creating conversation fork failed: {error}; opened a new conversation instead"
                        ));
                        self.conversations[index] = replacement;
                    }
                    Err(fallback_error) => {
                        // Keeping `forking` set makes this placeholder
                        // non-submittable until the user fixes repository state.
                        self.conversations[index].show_command_error(format!(
                            "creating conversation fork failed: {error}; creating a safe replacement also failed: {fallback_error}"
                        ));
                    }
                }
            }
        }
    }

    fn finish_title_generation(&mut self, index: usize, result: Result<String, String>) {
        let transport = self.transport();
        let state = &mut self.conversations[index];
        state.generating_title = false;
        if !state.automatic_title {
            return;
        }
        let title = match result {
            Ok(title) => title,
            Err(_) => return,
        };
        state.automatic_title = false;
        if state.current_hash().is_some() {
            let Some(expected) = state
                .remote_title
                .clone()
                .or_else(|| state.automatic_title_fallback.clone())
            else {
                return;
            };
            let Ok(transport) = transport else {
                return;
            };
            match compare_and_set_conversation_title(&transport, &state.id, &expected, &title) {
                Ok(true) => {
                    state.remote_title = Some(title.clone());
                    state.automatic_title_fallback = None;
                }
                Ok(false) | Err(_) => return,
            }
        }
        state.title = title;
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        self.selected_mut().command_error = None;
        let shortcut = match key.code {
            KeyCode::Char(input) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let shifted = key.modifiers.contains(KeyModifiers::SHIFT);
                PALETTE_COMMANDS
                    .iter()
                    .find(|command| {
                        command.shortcut.shifted == shifted
                            && (!command.shortcut.list_only || self.focus == Focus::List)
                            && command.shortcut.keys.contains(input.to_ascii_lowercase())
                    })
                    .map(|command| command.action)
            }
            _ => None,
        };
        if shortcut == Some(AppAction::SelectionLock) {
            self.execute_action(AppAction::SelectionLock);
            return;
        }
        if self.selection_locked {
            if key.code == KeyCode::Esc {
                self.selection_locked = false;
            }
            return;
        }
        if key.code == KeyCode::Esc && self.selected().running {
            self.interrupt_selected();
            return;
        }
        let is_palette = key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('p' | 'P'));
        if is_palette {
            self.execute_action(AppAction::Commands);
            return;
        }
        if self.palette.is_some() {
            self.handle_palette_key(key);
            return;
        }
        if shortcut == Some(AppAction::Publish) {
            self.execute_action(AppAction::Publish);
            return;
        }
        if matches!(self.confirm_action, Some(ConfirmAction::Publish { .. })) {
            match key.code {
                KeyCode::Esc => {
                    self.confirm_action = None;
                    self.selected_mut().status.clear();
                    self.selected_mut().publish_prompt = false;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.confirm_action = None;
                    self.selected_mut().status.clear();
                    self.selected_mut().publish_prompt = false;
                }
                KeyCode::Backspace => {
                    if let Some(ConfirmAction::Publish { base_input, .. }) =
                        self.confirm_action.as_mut()
                    {
                        base_input.pop();
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(ConfirmAction::Publish { base_input, .. }) =
                        self.confirm_action.as_mut()
                    {
                        base_input.clear();
                    }
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
                {
                    if let Some(ConfirmAction::Publish { base_input, .. }) =
                        self.confirm_action.as_mut()
                    {
                        base_input.push(ch);
                    }
                }
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Esc
            && (self.selected().reference_notice.is_some() || self.selected().reference_loading)
        {
            let state = self.selected_mut();
            state.reference_notice = None;
            state.reference_loading = false;
            if state.status == "loading conversation reference" {
                state.status.clear();
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if !self.selected_mut().composer.clear() {
                self.should_quit = true;
            }
            return;
        }
        if let Some(action) = shortcut {
            self.execute_action(action);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Up {
            self.select_relative(-1);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Down {
            self.select_relative(1);
            return;
        }
        if self.focus == Focus::List {
            match key.code {
                KeyCode::Enter => self.focus = Focus::Conversation,
                KeyCode::Up => self.select_relative(-1),
                KeyCode::Down => self.select_relative(1),
                _ => {}
            }
            return;
        }
        if self.view == View::Activity {
            match key.code {
                KeyCode::Esc => self.view = View::Chat,
                KeyCode::Up => self.select_activity(-1),
                KeyCode::Down => self.select_activity(1),
                KeyCode::PageUp => self.scroll_activity_details_up(8),
                KeyCode::PageDown => self.scroll_activity_details_down(8),
                _ => {}
            }
            return;
        }
        if self.view == View::Help && key.code == KeyCode::Esc {
            self.view = View::Chat;
            return;
        }
        match key.code {
            KeyCode::PageUp => self.scroll_up(8),
            KeyCode::PageDown => self.scroll_down(8),
            _ if self.view != View::Chat => {}
            KeyCode::Left
                if key
                    .modifiers
                    .contains(KeyModifiers::SHIFT | KeyModifiers::SUPER) =>
            {
                self.selected_mut().composer.select_home()
            }
            KeyCode::Right
                if key
                    .modifiers
                    .contains(KeyModifiers::SHIFT | KeyModifiers::SUPER) =>
            {
                self.selected_mut().composer.select_end()
            }
            KeyCode::Left
                if key
                    .modifiers
                    .contains(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.selected_mut().composer.select_word_left()
            }
            KeyCode::Right
                if key
                    .modifiers
                    .contains(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.selected_mut().composer.select_word_right()
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SUPER) => {
                self.selected_mut().composer.move_home()
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SUPER) => {
                self.selected_mut().composer.move_end()
            }
            KeyCode::Left
                if key
                    .modifiers
                    .contains(KeyModifiers::SHIFT | KeyModifiers::CONTROL) =>
            {
                self.selected_mut().composer.select_word_left()
            }
            KeyCode::Right
                if key
                    .modifiers
                    .contains(KeyModifiers::SHIFT | KeyModifiers::CONTROL) =>
            {
                self.selected_mut().composer.select_word_right()
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_mut().composer.move_word_left()
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_mut().composer.move_word_right()
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                self.selected_mut().composer.move_word_left()
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                self.selected_mut().composer.move_word_right()
            }
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                self.selected_mut().composer.delete_word_left()
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::ALT) => {
                self.selected_mut().composer.delete_word_right()
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.selected_mut().composer.move_word_left()
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.selected_mut().composer.move_word_right()
            }
            KeyCode::Enter | KeyCode::Char('s')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let command_is_complete = parse_command(&self.selected().composer.text).is_some();
                if command_is_complete || !self.selected_mut().composer.complete_command() {
                    self.start_turn();
                }
            }
            KeyCode::Enter => {
                if !self.selected_mut().composer.complete_command() {
                    self.selected_mut().composer.insert_char('\n');
                }
            }
            KeyCode::Tab => {
                self.selected_mut().composer.complete_command();
            }
            KeyCode::Esc => {
                self.selected_mut().composer.dismiss_command_menu();
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_mut().composer.insert_char('\n')
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_mut().composer.move_home()
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_mut().composer.move_end()
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_mut().composer.delete_word_left()
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_mut().composer.kill_line()
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
                self.selected_mut().composer.insert_char(ch)
            }
            KeyCode::Backspace => self.selected_mut().composer.backspace(),
            KeyCode::Delete => self.selected_mut().composer.delete(),
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.selected_mut().composer.select_left()
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.selected_mut().composer.select_right()
            }
            KeyCode::Left => self.selected_mut().composer.move_left(),
            KeyCode::Right => self.selected_mut().composer.move_right(),
            KeyCode::Up => {
                if !self.selected_mut().composer.select_command(-1) {
                    self.selected_mut().composer.move_vertical(true);
                }
            }
            KeyCode::Down => {
                if !self.selected_mut().composer.select_command(1) {
                    self.selected_mut().composer.move_vertical(false);
                }
            }
            KeyCode::Home if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.selected_mut().composer.select_home()
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.selected_mut().composer.select_end()
            }
            KeyCode::Home => self.selected_mut().composer.move_home(),
            KeyCode::End => self.selected_mut().composer.move_end(),
            _ => {}
        }
    }

    pub(crate) fn selected_composer_text(&self) -> Option<&str> {
        self.selected().composer.selected_text()
    }

    fn handle_palette_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Enter => {
                let action = self
                    .palette
                    .as_ref()
                    .and_then(CommandPalette::selected_action);
                self.palette = None;
                if let Some(action) = action {
                    self.execute_action(action);
                }
            }
            KeyCode::Up => self.palette.as_mut().expect("palette is open").select(-1),
            KeyCode::Down => self.palette.as_mut().expect("palette is open").select(1),
            KeyCode::Backspace => self
                .palette
                .as_mut()
                .expect("palette is open")
                .edit(|query| {
                    query.pop();
                }),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => self
                .palette
                .as_mut()
                .expect("palette is open")
                .edit(String::clear),
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
                self.palette
                    .as_mut()
                    .expect("palette is open")
                    .edit(|query| query.push(ch));
            }
            _ => {}
        }
    }

    fn execute_action(&mut self, action: AppAction) {
        match action {
            AppAction::NewConversation => {
                self.start_new_conversation(None);
                self.focus = Focus::Conversation;
            }
            AppAction::Checkout => self.load_selected(),
            AppAction::Publish => self.publish_selected(),
            AppAction::Activity => {
                self.view = if self.view == View::Activity {
                    View::Chat
                } else {
                    self.selected_mut().ensure_activity_selection();
                    View::Activity
                };
            }
            AppAction::Changes => {
                self.view = if self.view == View::Diff {
                    View::Chat
                } else {
                    View::Diff
                };
                self.selected_mut().follow_tail();
            }
            AppAction::Tools => {
                self.view = if self.view == View::Tools {
                    View::Chat
                } else {
                    View::Tools
                };
                self.selected_mut().follow_tail();
                if self.view == View::Tools {
                    self.load_selected_tool_set();
                }
            }
            AppAction::Reload => self.reload_selected(),
            AppAction::Help => {
                self.view = if self.view == View::Help {
                    View::Chat
                } else {
                    View::Help
                };
                self.selected_mut().follow_tail();
            }
            AppAction::Archive => self.close_selected(),
            AppAction::SelectionLock => self.selection_locked = !self.selection_locked,
            AppAction::Commands => {
                self.confirm_action = None;
                self.selected_mut().publish_prompt = false;
                self.palette = self.palette.take().is_none().then(CommandPalette::default);
            }
            AppAction::From
            | AppAction::Invite
            | AppAction::Model
            | AppAction::PublishBranch
            | AppAction::Reference
            | AppAction::Title
            | AppAction::UpdateTree => unreachable!("slash action needs arguments"),
        }
    }

    fn reload_selected(&mut self) {
        if !self.selected().is_busy() {
            match self.transport() {
                Ok(transport) => {
                    let user = self.user.clone();
                    let _ = self.selected_mut().reload(&transport, &user);
                    self.selected_mut().status = "reloaded".to_string();
                }
                Err(error) => self.selected_mut().show_command_error(error),
            }
        } else {
            self.selected_mut()
                .show_command_error("finish this conversation's operation before reloading");
        }
    }

    pub(crate) fn scroll_up(&mut self, rows: usize) {
        self.screen_selection = None;
        let state = self.selected_mut();
        state.transcript_selection = None;
        state.scroll.scroll_up(rows);
    }

    pub(crate) fn scroll_down(&mut self, rows: usize) {
        self.screen_selection = None;
        let state = self.selected_mut();
        state.transcript_selection = None;
        state.scroll.scroll_down(rows);
        if state.scroll.offset.is_none() {
            state.unread_below = false;
        }
    }

    fn select_activity(&mut self, amount: isize) {
        let state = self.selected_mut();
        if state.activities.is_empty() {
            state.activity_selection = None;
            return;
        }
        let selected = state
            .activity_selection
            .unwrap_or(state.activities.len() - 1);
        let next = selected
            .saturating_add_signed(amount)
            .min(state.activities.len() - 1);
        if next != selected {
            state.activity_selection = Some(next);
            state.activity_detail_scroll = 0;
        }
    }

    fn scroll_activity_details_up(&mut self, rows: usize) {
        let state = self.selected_mut();
        state.activity_detail_scroll = state.activity_detail_scroll.saturating_sub(rows);
    }

    fn scroll_activity_details_down(&mut self, rows: usize) {
        let state = self.selected_mut();
        state.activity_detail_scroll = state.activity_detail_scroll.saturating_add(rows);
    }

    fn start_from_hash(&mut self, hash: &str) {
        let resolved = self
            .transport()
            .and_then(|transport| transport.resolve_revspec(hash))
            .and_then(|commit| commit.ok_or_else(|| format!("cannot resolve commit {hash:?}")));
        let commit = match resolved {
            Ok(commit) => commit.to_string(),
            Err(error) => {
                self.selected_mut().show_command_error(error);
                return;
            }
        };
        self.start_new_conversation(Some(commit));
    }

    fn start_new_conversation(&mut self, base: Option<String>) {
        let transport = match self.transport() {
            Ok(transport) => transport,
            Err(error) => {
                self.selected_mut().show_command_error(error);
                return;
            }
        };
        // Name the conversation from the ones already loaded in memory rather
        // than re-listing the user's whole active+archived set from the server.
        // `list_user_conversations` fetches every conversation's head object and
        // title blob over the network, so doing it on each Ctrl+N made starting
        // a conversation slower the more conversations you had accumulated. The
        // default title only has to be unique among the conversations you can
        // see, and the minted id is content-unique regardless of the title.
        let title = first_available_conversation_name(
            self.conversations.iter().map(|item| item.title.as_str()),
        );
        let id = match fresh_conversation_id(&transport, &self.user) {
            Ok(id) => id,
            Err(error) => {
                self.selected_mut().show_command_error(error);
                return;
            }
        };
        let origin = self.selected().id.clone();
        let previous_options = self.selected().turn_options.clone();
        let (state, fork_source) = match base {
            None => {
                let (options, base) =
                    match new_conversation_options(previous_options, None, &self.repo_dir) {
                        Ok(result) => result,
                        Err(error) => {
                            self.selected_mut().show_command_error(error);
                            return;
                        }
                    };
                (
                    ConversationState::new_virtual(
                        id.clone(),
                        title.clone(),
                        options,
                        format!("ready from {}; enter a prompt", short_hash(&base)),
                    ),
                    None,
                )
            }
            Some(from) => {
                // Once materialized, the canonical first parent carries the
                // fork source. Keeping it as a fallback base would let a lost
                // remote ref silently recreate a marker-less conversation.
                let mut options = previous_options;
                options.base = None;
                let mut state = ConversationState::new_virtual(
                    id.clone(),
                    title.clone(),
                    options,
                    format!("forking from {}", short_hash(&from)),
                );
                state.forking = true;
                state.remote_title = Some(title.clone());
                (state, Some(from))
            }
        };
        self.conversations.insert(0, state);
        self.selected = 0;
        self.view = View::Chat;
        self.confirm_action = None;
        if let Some(source) = fork_source {
            let tx = self.tx.clone();
            let repo_dir = self.repo_dir.clone();
            let user = self.user.clone();
            std::thread::spawn(move || {
                let result = GitTransport::discover(repo_dir).and_then(|transport| {
                    let fork = fork_conversation(&transport, &user, &id, &title, &source)?;
                    let load = conversation_load_at(&transport, &id, &fork)?;
                    Ok((fork, Box::new(load)))
                });
                let _ = tx.send(UiMessage::Forked {
                    conversation: id,
                    origin,
                    source,
                    result,
                });
            });
        }
    }

    fn select_relative(&mut self, amount: isize) {
        let len = self.conversations.len() as isize;
        self.select((self.selected as isize + amount).rem_euclid(len) as usize);
    }

    fn select(&mut self, index: usize) {
        self.selected_mut().publish_prompt = false;
        self.selected = index;
        self.confirm_action = None;
        let needs_load = self.selected().diff.is_none() && self.selected().remote_head.is_some();
        if needs_load {
            self.selected_mut().remote_head = None;
            self.selected_mut().status = "loading conversation".to_string();
            self.poll_remote();
        }
        if self.view == View::Tools {
            self.load_selected_tool_set();
        }
    }

    fn close_selected(&mut self) {
        if self.selected().is_busy() {
            self.selected_mut()
                .show_command_error("finish this conversation's operation before archiving it");
            return;
        }
        let replacement = if self.conversations.len() == 1 {
            let title = first_available_conversation_name(
                self.conversations
                    .iter()
                    .map(|conversation| conversation.title.as_str()),
            );
            let id = match self
                .transport()
                .and_then(|transport| fresh_conversation_id(&transport, &self.user))
            {
                Ok(id) => id,
                Err(error) => {
                    self.selected_mut().show_command_error(error);
                    return;
                }
            };
            let (options, base) = match new_conversation_options(
                self.selected().turn_options.clone(),
                None,
                &self.repo_dir,
            ) {
                Ok(result) => result,
                Err(error) => {
                    self.selected_mut().show_command_error(error);
                    return;
                }
            };
            Some(ConversationState::new_virtual(
                id,
                title,
                options,
                format!("ready from {}; enter a prompt", short_hash(&base)),
            ))
        } else {
            None
        };
        if self.selected().current_hash().is_some() {
            let result = self.transport().and_then(|transport| {
                archive_user_conversation(&transport, &self.user, &self.selected().id)
            });
            if let Err(error) = result {
                self.selected_mut()
                    .show_command_error(format!("archiving conversation failed: {error}"));
                return;
            }
        }
        if let Some(replacement) = replacement {
            self.conversations[0] = replacement;
            self.selected = 0;
            self.view = View::Chat;
        } else {
            self.conversations.remove(self.selected);
            self.selected = self.selected.min(self.conversations.len() - 1);
        }
        self.confirm_action = None;
        if self.view == View::Tools {
            self.load_selected_tool_set();
        }
    }

    fn rename_selected(&mut self, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            self.selected_mut()
                .show_command_error("conversation title cannot be empty");
            return;
        }
        if title.contains(['\n', '\r', '\t']) {
            self.selected_mut()
                .show_command_error("conversation title must be one line");
            return;
        }
        let published = self.selected().current_hash().is_some();
        if published {
            let id = self.selected().id.clone();
            if let Err(error) = self
                .transport()
                .and_then(|transport| set_conversation_title(&transport, &id, title))
            {
                self.selected_mut().show_command_error(error);
                return;
            }
        }
        let state = self.selected_mut();
        state.title = title.to_string();
        if published {
            state.remote_title = Some(title.to_string());
        }
        state.automatic_title = false;
        if published {
            state.automatic_title_fallback = None;
        }
        state.status = format!("renamed conversation to {title:?}");
    }

    fn load_selected_tool_set(&mut self) {
        if self.selected().tool_set.is_some() {
            return;
        }
        let name = self.selected().id.clone();
        let options = self.selected().turn_options.clone();
        let result = self
            .transport()
            .and_then(|transport| describe_tool_set(&transport, &name, &options));
        self.selected_mut().tool_set = Some(result);
    }

    fn load_selected(&mut self) {
        if self.selected().is_busy() {
            self.selected_mut()
                .show_command_error("finish this conversation's operation before checking it out");
        } else if let Some(diff) = self.selected().diff.clone() {
            match load_conversation_workspace(&diff.head, &self.repo_dir) {
                Ok(()) => {
                    self.selected_mut().status =
                        format!("checked out {} in detached HEAD", short_hash(&diff.head));
                }
                Err(error) => self.selected_mut().show_command_error(error),
            }
        } else {
            self.selected_mut()
                .show_command_error("this conversation has no commit to check out");
        }
    }

    fn publish_branch_selected(&mut self) {
        if self.selected().is_busy() {
            self.selected_mut().show_command_error(
                "finish this conversation's operation before publishing its branch",
            );
            return;
        }
        let Some(diff) = self.selected().diff.clone() else {
            self.selected_mut()
                .show_command_error("this conversation has no completed turn to publish");
            return;
        };
        let conversation = self.selected().id.clone();
        self.selected_mut().publishing = true;
        self.selected_mut().status = "publishing the complete conversation branch".to_string();
        let repo_dir = self.repo_dir.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let prepared = prepare_publish_workspace(&diff.head, &diff.base_commit, &repo_dir)?;
                publish_conversation_branch(&conversation, &prepared, &repo_dir)
            })();
            let _ = tx.send(UiMessage::BranchPublished {
                conversation,
                result,
            });
        });
    }

    fn publish_selected(&mut self) {
        if self.selected().is_busy() {
            self.selected_mut()
                .show_command_error("finish this conversation's operation before publishing it");
        } else if self
            .selected()
            .diff
            .as_ref()
            .is_none_or(|diff| diff.patch.is_empty())
        {
            self.selected_mut()
                .show_command_error("there are no conversation changes to publish");
        } else if self.confirm_action.is_none() {
            let default_base = match remote_default_branch(&self.repo_dir) {
                Ok(branch) => branch,
                Err(error) => {
                    self.selected_mut().show_command_error(error);
                    return;
                }
            };
            self.confirm_action = Some(ConfirmAction::Publish {
                default_base,
                base_input: String::new(),
            });
            self.selected_mut().publish_prompt = true;
            self.selected_mut().status =
                "enter a PR base branch or press Ctrl+P again for the default".to_string();
        } else {
            let pr_base = match self.confirm_action.take() {
                Some(ConfirmAction::Publish {
                    default_base,
                    base_input,
                }) => {
                    let base_input = base_input.trim();
                    if base_input.is_empty() {
                        default_base
                    } else {
                        base_input.to_string()
                    }
                }
                None => unreachable!("publication was confirmed"),
            };
            self.selected_mut().publish_prompt = false;
            let name = self.selected().id.clone();
            let title = self.selected().title.clone();
            self.selected_mut().publishing = true;
            self.selected_mut().status = "fetching the selected PR base".to_string();
            let tx = self.tx.clone();
            let head = self
                .selected()
                .diff
                .as_ref()
                .expect("publication requires a conversation diff")
                .head
                .clone();
            let options = self.selected().turn_options.clone();
            let repo_dir = self.repo_dir.clone();
            std::thread::spawn(move || {
                let result = (|| {
                    let base_commit = fetch_remote_branch_tip(&pr_base, &repo_dir)?;
                    let target = base_commit;
                    let base_is_ancestor = remote_base_is_ancestor(&target, &head, &repo_dir)?;
                    let transport = GitTransport::discover(&repo_dir)?;
                    if !base_is_ancestor {
                        transport.ensure_pushed(&target)?;
                    }
                    let message = publish_turn_message(&target, base_is_ancestor);
                    let outcome = run_chat_turn(
                        &transport,
                        &options,
                        &name,
                        &message,
                        None,
                        |_| {},
                        |event| {
                            let _ = tx.send(UiMessage::Turn {
                                conversation: name.clone(),
                                event,
                            });
                        },
                    )?;
                    let conversation =
                        prepare_publish_workspace(&outcome.commit, &target, &repo_dir)?;
                    let _ = tx.send(UiMessage::Completed {
                        conversation: name.clone(),
                        outcome,
                    });
                    publish_conversation_pr(&name, &title, &conversation, &pr_base, &repo_dir)
                })();
                let _ = tx.send(UiMessage::Published {
                    conversation: name,
                    result,
                });
            });
        }
    }
}

fn publish_turn_message(target: &str, base_is_ancestor: bool) -> String {
    if base_is_ancestor {
        format!(
            "Prepare this conversation for publication. The selected PR base `{target}` is \
             already an ancestor of this conversation, so do not call `merge` for it again. \
             Build and test, then finish only when the workspace is ready to publish."
        )
    } else {
        format!(
            "Prepare this conversation for publication. First call the existing `merge` tool \
             with `theirs` exactly `{target}`. Resolve every entry in `.caos/conflicts`, remove \
             `.caos/conflicts`, then build and test. Finish only when the workspace is ready to \
             publish."
        )
    }
}

fn screen_point(column: u16, row: u16, area: Rect) -> TranscriptPoint {
    TranscriptPoint {
        row: row.clamp(area.y, area.bottom().saturating_sub(1)),
        column: column.clamp(area.x, area.right().saturating_sub(1)),
    }
}

fn fresh_conversation_id(t: &GitTransport, user: &str) -> Result<String, String> {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("reading the clock: {error}"))?
        .as_nanos();
    let descriptor = format!(
        "caos conversation\ncreator {user}\ncreated {created}\nprocess {}\n",
        std::process::id()
    );
    t.put_object("blob", descriptor.as_bytes())
        .map(|id| id.to_string())
}

fn new_conversation_options(
    mut options: TurnOptions,
    requested_base: Option<String>,
    repo_dir: &Path,
) -> Result<(TurnOptions, String), String> {
    let base = match requested_base {
        Some(base) => base,
        None => local_default_branch_tip(repo_dir)?.1,
    };
    options.base = Some(base.clone());
    Ok((options, base))
}

#[derive(Debug, PartialEq, Eq)]
enum ConversationChoice {
    Existing(String),
    New { id: Option<String>, title: String },
}

fn choose_conversation(
    requested: Option<&str>,
    new: bool,
    conversations: &[UserConversationSummary],
) -> Result<ConversationChoice, String> {
    if let Some(requested) = requested {
        if new
            && conversations
                .iter()
                .any(|conversation| conversation.id == requested)
        {
            return Err(format!(
                "--new: conversation {requested:?} already exists; omit --new to continue it"
            ));
        }
        if conversations
            .iter()
            .any(|conversation| conversation.id == requested)
        {
            return Ok(ConversationChoice::Existing(requested.to_string()));
        }
        return Ok(ConversationChoice::New {
            id: Some(requested.to_string()),
            title: requested.to_string(),
        });
    }
    if !new {
        if let Some(latest) = conversations.first() {
            return Ok(ConversationChoice::Existing(latest.id.clone()));
        }
    }
    Ok(ConversationChoice::New {
        id: None,
        title: first_available_conversation_name(
            conversations
                .iter()
                .map(|conversation| conversation.title.as_str()),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use caos_cli::{conversation_head, conversation_ref, ConversationReplay};
    use ratatui_core::backend::TestBackend;
    use ratatui_core::layout::Rect;
    use ratatui_core::style::{Color, Modifier};
    use ratatui_core::terminal::Terminal;
    use ratatui_widgets::paragraph::{Paragraph, Wrap};

    use super::ui::{
        content_contains, paragraph_scroll, render, scroll_offset, transcript_contains,
    };

    #[test]
    fn publish_prompt_only_requests_a_merge_when_the_base_is_not_an_ancestor() {
        let already_merged = publish_turn_message("abc123", true);
        assert!(already_merged.contains("do not call `merge`"));
        assert!(!already_merged.contains("First call the existing `merge` tool"));

        let needs_merge = publish_turn_message("abc123", false);
        assert!(needs_merge.contains("First call the existing `merge` tool"));
        assert!(needs_merge.contains("`theirs` exactly `abc123`"));
    }

    fn summary(id: &str) -> UserConversationSummary {
        UserConversationSummary {
            id: id.to_string(),
            title: id.to_string(),
            head: "a".repeat(40),
            updated_unix: 1,
            parent: None,
        }
    }

    fn state(name: &str) -> ConversationState {
        ConversationState::new(
            name.to_string(),
            name.to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        )
    }

    /// A throwaway git repo for transport-touching paths, so no test depends
    /// on cwd being a repo — the cargo worker's is not.
    fn throwaway_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "caos-cli-tui-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        dir
    }

    fn git_ok(cwd: &Path, args: &[&str]) {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap()
            .success());
    }

    fn git_output(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn push_test_conversation(repo: &Path, id: &str, head: &str) {
        git_ok(
            repo,
            &[
                "push",
                "-q",
                "caos",
                &format!("{head}:refs/caos/v2/conversations/{id}/head"),
            ],
        );
    }

    fn seed_idle_conversation(repo: &Path, id: &str, username: &str, message: &str) -> String {
        let base = git_output(repo, &["rev-parse", "HEAD"]);
        let tree = git_output(repo, &["rev-parse", "HEAD^{tree}"]);
        let event = serde_json::to_string(&serde_json::json!({
            "base": base,
            "author": "user",
            "username": username,
            "content": message,
            "status": "idle",
        }))
        .unwrap();
        let head = git_output(repo, &["commit-tree", &tree, "-p", &base, "-m", &event]);
        push_test_conversation(repo, id, &head);
        head
    }

    fn seed_queued_conversation(
        repo: &Path,
        id: &str,
        username: &str,
        message: &str,
    ) -> (String, String) {
        let base = git_output(repo, &["rev-parse", "HEAD"]);
        let tree = git_output(repo, &["rev-parse", "HEAD^{tree}"]);
        let user_event = serde_json::to_string(&serde_json::json!({
            "base": base,
            "author": "user",
            "username": username,
            "content": message,
        }))
        .unwrap();
        let user = git_output(
            repo,
            &["commit-tree", &tree, "-p", &base, "-m", &user_event],
        );
        let request = "b".repeat(40);
        let admission = serde_json::to_string(&serde_json::json!({
            "status": "queued",
            "request": request,
            "request_head": user,
        }))
        .unwrap();
        let head = git_output(repo, &["commit-tree", &tree, "-p", &user, "-m", &admission]);
        push_test_conversation(repo, id, &head);
        (head, request)
    }

    fn repo_with_default_branch(name: &str, branch: &str) -> (PathBuf, PathBuf, String) {
        let dir = throwaway_repo(name);
        git_ok(&dir, &["config", "user.name", "CAOS test"]);
        git_ok(&dir, &["config", "user.email", "caos-test@example.invalid"]);
        std::fs::write(dir.join("base.txt"), "default branch\n").unwrap();
        git_ok(&dir, &["add", "base.txt"]);
        git_ok(&dir, &["commit", "-q", "-m", "default branch"]);
        // Name the local branch after the default branch, as a real checkout
        // would be: local_default_branch_tip reads refs/heads/<branch>.
        git_ok(&dir, &["branch", "-M", branch]);
        let tip = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let remote = dir.with_extension("remote.git");
        let remote_path = remote.to_string_lossy().to_string();
        git_ok(&dir, &["init", "--bare", "-q", &remote_path]);
        let git_dir = format!("--git-dir={remote_path}");
        let default_ref = format!("refs/heads/{branch}");
        git_ok(&dir, &[&git_dir, "symbolic-ref", "HEAD", &default_ref]);
        git_ok(&dir, &["remote", "add", "origin", &remote_path]);
        let push_ref = format!("HEAD:{default_ref}");
        git_ok(&dir, &["push", "-q", "origin", &push_ref]);
        // Set the origin/HEAD symref (via a one-time fetch + set-head) so
        // local_default_branch_tip can discover the default branch NAME without
        // touching the network — as it would in a real clone. It then reads the
        // tip from the local refs/heads/<branch>.
        git_ok(&dir, &["fetch", "-q", "origin"]);
        git_ok(&dir, &["remote", "set-head", "origin", "-a"]);

        (dir, remote, tip)
    }

    fn activity(number: usize) -> Activity {
        Activity {
            request: "a".repeat(40),
            round: number as u64,
            id: format!("tool-{number}"),
            step_commit: format!("{number:040x}"),
            name: "bash".to_string(),
            summary: format!("tool summary {number}"),
            detail: format!("detail line {number}\n{}", "more detail\n".repeat(20)),
            state: ActivityState::Succeeded,
        }
    }

    fn app_with(conversations: Vec<ConversationState>) -> (App, Sender<UiMessage>) {
        let (tx, rx) = mpsc::channel();
        (
            App {
                repo_dir: PathBuf::from("."),
                user: "tester".to_string(),
                conversations,
                selected: 0,
                should_quit: false,
                selection_locked: false,
                confirm_action: None,
                selecting_transcript: false,
                screen_selection: None,
                selecting_screen: false,
                pending_conversation_click: None,
                rendered_screen: None,
                copied_chars: None,
                animation_frame: 0,
                enhanced_keyboard: false,
                remote_polling: false,
                view: View::Chat,
                focus: Focus::Conversation,
                tx: tx.clone(),
                rx,
                palette: None,
            },
            tx,
        )
    }

    fn wait_for_fork(app: &mut App, id: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            app.drain_messages();
            match app.conversation_index(id) {
                Some(index) if !app.conversations[index].forking => return true,
                None => return false,
                Some(_) => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for fork {id}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn wait_for_remote_poll(app: &mut App) {
        app.poll_remote();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.remote_polling {
            app.drain_messages();
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for remote poll"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn wait_for_pending_submission(app: &mut App, id: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app
            .selected()
            .pending_submissions
            .iter()
            .any(|pending| pending.id == id)
        {
            app.drain_messages();
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for pending submission {id}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn wait_for_reference_lookup(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.selected().reference_loading {
            app.drain_messages();
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for conversation reference"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn rendered_main_pane(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().skip(26).map(|cell| cell.symbol()).collect())
            .collect()
    }

    fn rendered_header(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .iter()
            .take(buffer.area.width as usize)
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn composer_edits_utf8_and_moves_between_lines() {
        let mut composer = Composer::default();
        composer.insert_str("ab\nλx");
        composer.move_home();
        assert_eq!(composer.cursor_row_col(), (1, 0));
        composer.move_vertical(true);
        assert_eq!(composer.cursor_row_col(), (0, 0));
        composer.move_end();
        composer.insert_char('!');
        composer.backspace();
        composer.move_right();
        composer.delete();
        assert_eq!(composer.text, "ab\nx");
    }

    #[test]
    fn pasted_newlines_are_inserted_without_submitting() {
        let (mut app, _) = app_with(vec![state("talk-1")]);

        app.insert_paste("first\r\nsecond\rthird");

        assert_eq!(app.selected().composer.text, "first\nsecond\nthird");
        assert!(app.selected().transcript.is_empty());
        assert!(!app.selected().running);
    }

    #[test]
    fn enter_and_ctrl_j_insert_newlines() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("first");

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.selected_mut().composer.insert_str("second");
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));

        assert_eq!(app.selected().composer.text, "first\nsecond\n");
        assert!(app.selected().transcript.is_empty());
        assert!(!app.selected().running);
    }

    #[test]
    fn large_pastes_expand_only_when_the_message_is_taken() {
        let mut composer = Composer::default();
        let pasted = format!("first line\n{}", "λ".repeat(LARGE_PASTE_CHAR_THRESHOLD));

        composer.insert_paste(&pasted);

        assert_eq!(composer.pending_pastes.len(), 1);
        assert_eq!(
            composer.text,
            format!(
                "[Pasted text: {} chars]",
                LARGE_PASTE_CHAR_THRESHOLD + "first line\n".chars().count()
            )
        );
        assert!(!composer.text.contains("first line"));
        assert_eq!(composer.take_message().as_deref(), Some(pasted.as_str()));
        assert!(composer.text.is_empty());
        assert!(composer.pending_pastes.is_empty());
    }

    #[test]
    fn multiple_large_pastes_keep_distinct_placeholders_and_expand_in_text_order() {
        let mut composer = Composer::default();
        let first = "a".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
        let second = "b".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);

        composer.insert_paste(&first);
        composer.insert_str(" between ");
        composer.insert_paste(&second);

        assert!(composer.text.contains(&format!(
            "[Pasted text: {} chars]",
            LARGE_PASTE_CHAR_THRESHOLD + 1
        )));
        assert!(composer.text.contains(&format!(
            "[Pasted text: {} chars #2]",
            LARGE_PASTE_CHAR_THRESHOLD + 1
        )));
        assert_eq!(
            composer.take_message().unwrap(),
            format!("{first} between {second}")
        );
    }

    #[test]
    fn paste_placeholders_move_and_delete_as_atomic_text() {
        let mut composer = Composer::default();
        let pasted = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
        composer.insert_str("before ");
        let placeholder_start = composer.cursor;
        composer.insert_paste(&pasted);
        let placeholder_end = composer.cursor;
        composer.insert_str(" after");

        composer.cursor = placeholder_end;
        composer.move_left();
        assert_eq!(composer.cursor, placeholder_start);
        composer.move_right();
        assert_eq!(composer.cursor, placeholder_end);

        composer.backspace();
        assert_eq!(composer.text, "before  after");
        assert!(composer.pending_pastes.is_empty());

        composer.cursor = "before ".len();
        composer.insert_paste(&pasted);
        composer.cursor = "before ".len();
        composer.delete();
        assert_eq!(composer.text, "before  after");
        assert!(composer.pending_pastes.is_empty());
    }

    #[test]
    fn ctrl_c_clears_drafts_and_pending_pastes_before_exiting() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut()
            .composer
            .insert_paste(&"x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1));

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(!app.should_quit());
        assert!(app.selected().composer.text.is_empty());
        assert!(app.selected().composer.pending_pastes.is_empty());

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
    }

    #[test]
    fn automatic_titles_collapse_whitespace_and_limit_unicode_scalars() {
        assert_eq!(
            automatic_title("  Review\t the\nλ parser  "),
            "Review the λ parser"
        );
        assert_eq!(automatic_title(&"界".repeat(60)), "界".repeat(60));
        assert_eq!(
            automatic_title(&"界".repeat(61)),
            format!("{}…", "界".repeat(59))
        );
    }

    #[test]
    fn sidebar_text_only_collapses_whitespace_and_ellipsizes_by_terminal_cells() {
        assert_eq!(
            message_preview("##  Useful\nsummary", 20),
            "## Useful summary"
        );
        assert_eq!(message_preview(&"界".repeat(10), 6), "界界…");
    }

    #[test]
    fn only_new_virtual_conversations_take_their_first_prompt_as_fallback_title() {
        let mut virtual_conversation = ConversationState::new_virtual(
            "internal-id".to_string(),
            "talk-1".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        virtual_conversation.apply_automatic_title("First prompt");
        virtual_conversation.apply_automatic_title("Second prompt");
        assert_eq!(virtual_conversation.title, "First prompt");

        let mut existing = state("Existing title");
        existing.apply_automatic_title("A later prompt");
        assert_eq!(existing.title, "Existing title");
    }

    #[test]
    fn first_message_title_result_replaces_the_fallback_only_once() {
        let mut conversation = ConversationState::new_virtual(
            "internal-id".to_string(),
            "talk-1".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        conversation.apply_automatic_title("First message fallback");
        conversation.generating_title = true;
        assert!(!conversation.is_busy());
        let (mut app, _) = app_with(vec![conversation]);

        app.finish_title_generation(0, Ok("Generated task title".to_string()));
        assert_eq!(app.selected().title, "Generated task title");
        assert!(!app.selected().automatic_title);
        assert!(!app.selected().generating_title);

        app.finish_title_generation(0, Ok("Late replacement".to_string()));
        assert_eq!(app.selected().title, "Generated task title");
    }

    #[test]
    fn first_remote_title_publication_does_not_cancel_generation() {
        let mut conversation = ConversationState::new_virtual(
            "internal-id".to_string(),
            "talk-1".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        conversation.apply_automatic_title("published fallback");
        conversation.generating_title = true;
        let (mut app, _) = app_with(vec![conversation]);

        assert!(app.apply_remote_poll(vec![RemotePollEntry {
            summary: UserConversationSummary {
                id: "internal-id".to_string(),
                title: "published fallback".to_string(),
                head: "a".repeat(40),
                updated_unix: 1,
                parent: None,
            },
            observed_head: None,
            observed_title: None,
            load: None,
        }]));

        assert_eq!(app.selected().title, "published fallback");
        assert_eq!(
            app.selected().remote_title.as_deref(),
            Some("published fallback")
        );
        assert!(app.selected().automatic_title);
        assert!(app.selected().generating_title);
    }

    #[test]
    fn first_observed_foreign_title_cancels_automatic_generation() {
        let mut conversation = ConversationState::new_virtual(
            "internal-id".to_string(),
            "talk-1".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        conversation.apply_automatic_title("published fallback");
        conversation.generating_title = true;
        let (mut app, _) = app_with(vec![conversation]);

        assert!(app.apply_remote_poll(vec![RemotePollEntry {
            summary: UserConversationSummary {
                id: "internal-id".to_string(),
                title: "manual rename".to_string(),
                head: "a".repeat(40),
                updated_unix: 1,
                parent: None,
            },
            observed_head: None,
            observed_title: None,
            load: None,
        }]));

        assert_eq!(app.selected().title, "manual rename");
        assert!(!app.selected().automatic_title);
    }

    #[test]
    fn later_remote_title_change_cancels_generation_as_a_manual_rename() {
        let mut conversation = ConversationState::new_virtual(
            "internal-id".to_string(),
            "local fallback".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        conversation.remote_title = Some("published fallback".to_string());
        conversation.generating_title = true;
        let (mut app, _) = app_with(vec![conversation]);

        assert!(app.apply_remote_poll(vec![RemotePollEntry {
            summary: UserConversationSummary {
                id: "internal-id".to_string(),
                title: "manual rename".to_string(),
                head: "a".repeat(40),
                updated_unix: 1,
                parent: None,
            },
            observed_head: None,
            observed_title: Some("published fallback".to_string()),
            load: None,
        }]));

        assert_eq!(app.selected().title, "manual rename");
        assert_eq!(
            app.selected().remote_title.as_deref(),
            Some("manual rename")
        );
        assert!(!app.selected().automatic_title);
    }

    #[test]
    fn completed_first_turn_publishes_a_title_generated_before_its_first_reload() {
        let (repo, remote, _) = repo_with_default_branch("early-generated-title", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let head = seed_idle_conversation(&repo, "new-talk", "tester", "fallback prompt");
        let transport = GitTransport::discover(&repo).unwrap();
        publish_user_conversation(&transport, "tester", "new-talk", "fallback prompt").unwrap();

        let mut conversation = ConversationState::new_virtual(
            "new-talk".to_string(),
            "talk-1".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        conversation.apply_automatic_title("fallback prompt");
        conversation.title = "Generated title".to_string();
        conversation.automatic_title = false;
        let (mut app, _) = app_with(vec![conversation]);
        app.repo_dir = repo.clone();

        app.finish_turn(
            0,
            TurnOutcome {
                conversation: "new-talk".to_string(),
                commit: head.clone(),
                short_commit: short_hash(&head).to_string(),
                interrupted: false,
            },
        );

        assert_eq!(app.selected().title, "Generated title");
        assert_eq!(
            app.selected().remote_title.as_deref(),
            Some("Generated title")
        );
        let listed =
            list_user_conversations(&transport, "tester", UserConversationStatus::Active).unwrap();
        assert_eq!(listed[0].title, "Generated title");

        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn generated_title_uses_fallback_when_remote_title_is_not_cached() {
        let (repo, remote, _) = repo_with_default_branch("unknown-remote-title", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        seed_idle_conversation(&repo, "new-talk", "tester", "fallback prompt");
        let transport = GitTransport::discover(&repo).unwrap();
        publish_user_conversation(&transport, "tester", "new-talk", "fallback prompt").unwrap();

        let mut conversation = ConversationState::new_virtual(
            "new-talk".to_string(),
            "talk-1".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        conversation.apply_automatic_title("fallback prompt");
        conversation.generating_title = true;
        conversation.apply_load(
            conversation_load(&transport, "new-talk").unwrap().unwrap(),
            "tester",
        );
        assert!(conversation.current_hash().is_some());
        assert!(conversation.remote_title.is_none());
        let (mut app, _) = app_with(vec![conversation]);
        app.repo_dir = repo.clone();

        app.finish_title_generation(0, Ok("Generated title".to_string()));

        assert_eq!(app.selected().title, "Generated title");
        let listed =
            list_user_conversations(&transport, "tester", UserConversationStatus::Active).unwrap();
        assert_eq!(listed[0].title, "Generated title");

        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn composer_filters_selects_completes_and_dismisses_commands() {
        let mut composer = Composer::default();
        composer.insert_str("/");
        assert_eq!(
            composer
                .command_matches()
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            [
                "/from",
                "/help",
                "/title",
                "/update-tree",
                "/commands",
                "/publish-branch",
                "/ref",
                "/invite",
                "/model"
            ]
        );

        assert!(composer.select_command(2));
        assert!(composer.complete_command());
        assert_eq!(composer.text, "/title ");
        assert!(composer.command_matches().is_empty());

        composer.move_left();
        assert_eq!(
            composer
                .command_matches()
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            ["/title"]
        );
        assert!(composer.dismiss_command_menu());
        assert!(composer.command_matches().is_empty());
        composer.insert_char('x');
        assert!(!composer.command_menu_dismissed);

        let mut composer = Composer::default();
        composer.insert_str("/model sonnet-5");
        assert_eq!(composer.model_matches(), ["claude-sonnet-5"]);
        assert!(composer.complete_command());
        assert_eq!(composer.text, "/model claude-sonnet-5 ");
    }

    #[test]
    fn command_parser_only_claims_catalog_commands() {
        assert_eq!(
            COMMANDS
                .iter()
                .filter(|command| command.action.submits_message())
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            ["/update-tree"]
        );

        let (command, arguments) = parse_command("/title A useful title").unwrap();
        assert_eq!(command.action, AppAction::Title);
        assert_eq!(arguments, "A useful title");

        let (command, arguments) = parse_command("/from\nabc123").unwrap();
        assert_eq!(command.action, AppAction::From);
        assert_eq!(arguments, "abc123");

        let (command, arguments) = parse_command("/publish-branch").unwrap();
        assert_eq!(command.action, AppAction::PublishBranch);
        assert!(arguments.is_empty());

        let (command, arguments) = parse_command("/ref").unwrap();
        assert_eq!(command.action, AppAction::Reference);
        assert!(arguments.is_empty());

        let (command, arguments) = parse_command("/invite Bob Smith").unwrap();
        assert_eq!(command.action, AppAction::Invite);
        assert_eq!(arguments, "Bob Smith");

        let (command, arguments) = parse_command("/model claude-sonnet-5").unwrap();
        assert_eq!(command.action, AppAction::Model);
        assert_eq!(arguments, "claude-sonnet-5");

        let (command, arguments) = parse_command("/update-tree include this text").unwrap();
        assert_eq!(command.action, AppAction::UpdateTree);
        assert_eq!(arguments, "include this text");

        let (command, arguments) = parse_command("/help").unwrap();
        assert_eq!(command.action, AppAction::Help);
        assert_eq!(arguments, "");

        let (command, arguments) = parse_command("/commands").unwrap();
        assert_eq!(command.action, AppAction::Commands);
        assert_eq!(arguments, "");

        assert!(parse_command("/future server convention").is_none());
        assert!(parse_command("/titlecard").is_none());
    }

    #[test]
    fn model_command_updates_the_clients_last_used_model() {
        let (mut app, _) = app_with(vec![state("talk-1"), state("talk-2")]);
        app.selected_mut()
            .composer
            .insert_str("/model claude-sonnet-5");

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert_eq!(
            app.selected().turn_options.model.as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(
            app.selected().transcript.last().unwrap().text,
            "Model for future turns: claude-sonnet-5"
        );
        assert_eq!(
            app.conversations[1].turn_options.model.as_deref(),
            Some("claude-sonnet-5")
        );

        app.selected_mut().composer.insert_str("/model default");
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert_eq!(
            app.selected().turn_options.model.as_deref(),
            Some(DEFAULT_MODEL)
        );
        assert_eq!(
            app.conversations[1].turn_options.model.as_deref(),
            Some(DEFAULT_MODEL)
        );
        assert_eq!(
            app.selected().transcript.last().unwrap().text,
            format!("Model for future turns: {DEFAULT_MODEL}")
        );
    }

    #[test]
    fn new_conversations_default_to_the_local_default_branch_tip() {
        let (dir, remote, tip) = repo_with_default_branch("default-base", "release/next");
        let previous = TurnOptions {
            base: Some("old conversation base".to_string()),
            ..TurnOptions::default()
        };

        let (options, base) = new_conversation_options(previous, None, &dir).unwrap();

        assert_eq!(base, tip);
        assert_eq!(options.base.as_deref(), Some(tip.as_str()));
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn explicit_conversation_bases_override_the_remote_default() {
        let options = TurnOptions::default();
        let requested = "5ec3751".to_string();

        let (options, base) = new_conversation_options(
            options,
            Some(requested.clone()),
            Path::new("does-not-need-a-repository"),
        )
        .unwrap();

        assert_eq!(base, requested);
        assert_eq!(options.base.as_deref(), Some(requested.as_str()));
    }

    #[test]
    fn command_menu_keys_complete_select_and_dismiss() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("/");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.selected().composer.text, "/title ");
        assert!(!app.selected().running);

        app.selected_mut().composer = Composer::default();
        app.selected_mut().composer.insert_str("/f");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.selected().composer.command_menu_dismissed);
        assert_eq!(app.selected().composer.text, "/f");

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.selected().composer.text, "/from ");
    }

    #[test]
    fn ctrl_h_ctrl_question_mark_and_help_command_toggle_help() {
        let (mut app, _) = app_with(vec![state("talk-1")]);

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Help);
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Chat);

        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Help);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.view, View::Chat);

        app.selected_mut().composer.insert_str("/help");
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Help);
        assert!(!app.selected().running);
    }

    #[test]
    fn command_menu_renders_usage_and_descriptions_in_the_prompt_box() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("/");
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(&app, frame)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .chunks(terminal.backend().buffer().area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("> /from <commit> — start a conversation from a completed turn"));
        assert!(rendered.contains("/title <new title> — rename the selected conversation"));
        assert!(
            rendered.contains("/update-tree <message> — fold working-tree edits into the commit")
        );

        app.selected_mut().composer = Composer::default();
        app.selected_mut().composer.insert_str("/model sonnet-5");
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("claude-sonnet-5"));
    }

    #[test]
    fn command_palette_filters_and_runs_actions_without_changing_the_draft() {
        let mut conversation = state("talk-1");
        conversation.composer.insert_str("keep this draft");
        let (mut app, _) = app_with(vec![conversation]);

        app.handle_key(KeyEvent::new(
            KeyCode::Char('P'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert!(app.palette.is_some());
        for ch in "workspace changes".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let matches = app.palette.as_ref().unwrap().matches();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].action, AppAction::Changes);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Command palette"));
        assert!(rendered.contains("Show workspace changes"));
        assert!(!rendered.contains("New conversation"));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.palette.is_none());
        assert_eq!(app.view, View::Diff);
        assert_eq!(app.selected().composer.text, "keep this draft");
    }

    #[test]
    fn command_palette_searches_keywords_and_wraps_selection() {
        let mut palette = CommandPalette {
            query: "github branch".to_string(),
            ..CommandPalette::default()
        };
        assert_eq!(palette.matches()[0].action, AppAction::Publish);

        palette.query.clear();
        palette.select(-1);
        assert_eq!(palette.selected, PALETTE_COMMANDS.len() - 1);
        assert_eq!(palette.selected_action(), Some(AppAction::SelectionLock));
    }

    #[test]
    fn composer_moves_by_whitespace_and_non_whitespace_runs() {
        let mut composer = Composer::default();
        composer.insert_str("one  λambda\n三");

        composer.move_word_left();
        assert_eq!(&composer.text[composer.cursor..], "三");
        composer.move_word_left();
        assert_eq!(&composer.text[composer.cursor..], "λambda\n三");
        composer.move_word_left();
        assert_eq!(&composer.text[composer.cursor..], "one  λambda\n三");
        composer.move_word_left();
        assert_eq!(composer.cursor, 0);

        composer.move_word_right();
        assert_eq!(&composer.text[composer.cursor..], "λambda\n三");
        composer.move_word_right();
        assert_eq!(&composer.text[composer.cursor..], "三");
        composer.move_word_right();
        assert_eq!(composer.cursor, composer.text.len());
    }

    #[test]
    fn composer_deletes_words_without_splitting_utf8() {
        let mut composer = Composer::default();
        composer.insert_str("one  λambda\n三");

        composer.delete_word_left();
        assert_eq!(composer.text, "one  λambda\n");
        composer.delete_word_left();
        assert_eq!(composer.text, "one  ");

        composer.cursor = 0;
        composer.delete_word_right();
        assert_eq!(composer.text, "");
        assert_eq!(composer.cursor, 0);
    }

    #[test]
    fn option_word_keys_edit_the_composer() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("one  λambda");

        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(
            &app.selected().composer.text[app.selected().composer.cursor..],
            "λambda"
        );
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(
            app.selected().composer.cursor,
            app.selected().composer.text.len()
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(
            &app.selected().composer.text[app.selected().composer.cursor..],
            "λambda"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(app.selected().composer.cursor, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(
            &app.selected().composer.text[app.selected().composer.cursor..],
            "λambda"
        );
        app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT));
        assert_eq!(app.selected().composer.text, "one  ");
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(app.selected().composer.text, "");
    }

    #[test]
    fn control_arrows_move_by_words() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("one  λambda");

        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(
            &app.selected().composer.text[app.selected().composer.cursor..],
            "λambda"
        );
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(app.selected().composer.cursor, 0);
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(
            &app.selected().composer.text[app.selected().composer.cursor..],
            "λambda"
        );
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(
            app.selected().composer.cursor,
            app.selected().composer.text.len()
        );

        // Shift+Control extends a word-wise selection.
        app.handle_key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        ));
        assert_eq!(app.selected_composer_text(), Some("λambda"));
    }

    #[test]
    fn command_arrows_move_to_line_boundaries() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("first\nsecond");

        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER));
        assert_eq!(app.selected().composer.cursor_row_col(), (1, 0));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SUPER));
        assert_eq!(app.selected().composer.cursor_row_col(), (1, 6));
    }

    #[test]
    fn shifted_command_and_option_arrows_select_composer_text() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("one two\nthree");

        app.handle_key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::SHIFT | KeyModifiers::SUPER,
        ));
        assert_eq!(
            app.selected().composer.selection_range(),
            Some((8, app.selected().composer.text.len()))
        );
        assert_eq!(app.selected_composer_text(), Some("three"));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.selected().composer.text, "one two\nx");

        app.handle_key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::SHIFT | KeyModifiers::ALT,
        ));
        assert_eq!(app.selected().composer.selection_range(), Some((8, 9)));
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.selected().composer.text, "one two\n");
    }

    #[test]
    fn plain_arrows_collapse_composer_selections() {
        let mut composer = Composer::default();
        composer.insert_str("one two");
        composer.select_word_left();
        assert_eq!(composer.selection_range(), Some((4, 7)));

        composer.move_left();
        assert_eq!(composer.cursor, 4);
        assert_eq!(composer.selection_range(), None);

        composer.select_word_right();
        composer.move_right();
        assert_eq!(composer.cursor, 7);
        assert_eq!(composer.selection_range(), None);
    }

    #[test]
    fn escape_does_not_focus_the_conversation_list() {
        let (mut app, _) = app_with(vec![state("talk-1")]);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.focus(), Focus::Conversation);
    }

    #[test]
    fn list_focus_navigates_conversations_and_enter_opens_the_conversation() {
        let (mut app, _) = app_with(vec![state("talk-1"), state("talk-2"), state("talk-3")]);
        app.focus = Focus::List;

        // Up/Down move through conversations while the list is focused.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected().id, "talk-2");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected().id, "talk-3");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.selected().id, "talk-2");

        // Typing does not reach the composer while the list is focused.
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.selected().composer.text.is_empty());

        // Enter moves focus into the conversation pane, where typing lands.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.focus(), Focus::Conversation);
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.selected().composer.text, "x");
    }

    #[test]
    fn escape_interrupts_before_list_or_view_navigation() {
        let mut running = state("talk-1");
        running.running = true;
        let (mut app, _) = app_with(vec![running]);
        app.repo_dir = std::env::temp_dir().join(format!(
            "missing-caos-interrupt-test-repo-{}",
            std::process::id()
        ));
        app.focus = Focus::List;
        app.view = View::Activity;

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(app.selected().interrupting);
        assert_eq!(app.focus, Focus::List);
        assert_eq!(app.view, View::Activity);
    }

    #[test]
    fn conversation_selection_is_sticky_or_fresh() {
        let conversations = vec![summary("recent"), summary("talk-1")];
        assert_eq!(
            choose_conversation(None, false, &conversations).unwrap(),
            ConversationChoice::Existing("recent".to_string())
        );
        assert_eq!(
            choose_conversation(None, true, &conversations).unwrap(),
            ConversationChoice::New {
                id: None,
                title: "talk-2".to_string(),
            }
        );
        assert!(choose_conversation(Some("recent"), true, &conversations).is_err());
        assert_eq!(
            choose_conversation(Some("named"), false, &conversations).unwrap(),
            ConversationChoice::New {
                id: Some("named".to_string()),
                title: "named".to_string(),
            }
        );
        assert_eq!(
            first_available_conversation_name(
                conversations
                    .iter()
                    .map(|conversation| conversation.id.as_str())
                    .chain(std::iter::once("talk-2")),
            ),
            "talk-3"
        );
    }

    #[test]
    fn new_conversation_titles_never_reopen_a_matching_id() {
        let conversations = vec![UserConversationSummary {
            id: "talk-1".to_string(),
            title: "A generated title".to_string(),
            head: "a".repeat(40),
            updated_unix: 1,
            parent: None,
        }];
        assert_eq!(
            choose_conversation(None, true, &conversations).unwrap(),
            ConversationChoice::New {
                id: None,
                title: "talk-1".to_string(),
            }
        );
    }

    #[test]
    fn clicking_conversation_rows_selects_visible_and_scrolled_items() {
        let conversations = (0..20)
            .map(|index| state(&format!("talk-{index}")))
            .collect();
        let (mut app, _) = app_with(conversations);
        let area = Rect::new(0, 0, 100, 30);
        let mouse = |kind, row| MouseEvent {
            kind,
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5), area),
            MouseAction::Redraw
        );
        assert_eq!(app.selected, 0);
        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5), area),
            MouseAction::Redraw
        );
        assert_eq!(app.selected, 1);

        app.selected = 19;
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2), area);
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2), area);
        assert_eq!(app.selected, 7);

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1), area);
        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1), area),
            MouseAction::Redraw
        );
        assert_eq!(app.selected, 7);
    }

    #[test]
    fn mouse_drag_copies_text_outside_the_conversation_box() {
        let (mut app, _) = app_with(vec![state("copy-anywhere")]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let header = rendered_header(&terminal);
        let selection_end = header
            .find("copy-anywhere")
            .map(|column| column + "copy-anywhere".len() - 1)
            .unwrap() as u16;
        app.capture_screen(terminal.backend().buffer());
        let area = Rect::new(0, 0, 100, 30);
        let mouse = |kind, column| MouseEvent {
            kind,
            column,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1), area),
            MouseAction::Redraw
        );
        assert_eq!(
            app.handle_mouse(
                mouse(MouseEventKind::Drag(MouseButton::Left), selection_end),
                area
            ),
            MouseAction::Redraw
        );
        let copied = app.handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), selection_end),
            area,
        );

        assert!(
            matches!(copied, MouseAction::Copy(ref text) if text.contains("caos") && text.contains("copy-anywhere"))
        );
    }

    #[test]
    fn cli_options_match_the_line_client_surface() {
        // --username rides along so the test never depends on ambient $USER
        // (the cargo worker's environment has none).
        let args = Args::parse(&[
            "--username".into(),
            "tester".into(),
            "--from".into(),
            "5ec3751".into(),
            "--model".into(),
            "test-model".into(),
        ])
        .unwrap();
        assert_eq!(args.user, "tester");
        assert!(args.new_conversation);
        assert_eq!(args.from_commit.as_deref(), Some("5ec3751"));
        assert_eq!(args.turn.model.as_deref(), Some("test-model"));
        assert_eq!(args.turn.base.as_deref(), Some("5ec3751"));
    }

    #[test]
    fn from_commit_rejects_conflicting_conversation_options() {
        assert!(Args::parse(&[
            "--username".into(),
            "tester".into(),
            "--from".into(),
            "5ec3751".into(),
            "--base".into(),
            "HEAD~1".into(),
        ])
        .is_err());
        assert!(Args::parse(&[
            "--username".into(),
            "tester".into(),
            "--from".into(),
            "5ec3751".into(),
            "-c".into(),
            "work".into(),
        ])
        .is_err());
    }

    #[test]
    fn scroll_holds_an_anchor_until_it_returns_to_the_tail() {
        let mut scroll = ScrollState::default();
        assert_eq!(scroll_offset(20, 10, &scroll), 12);

        scroll.scroll_up(5);
        assert_eq!(scroll_offset(20, 10, &scroll), 7);
        assert_eq!(scroll_offset(40, 10, &scroll), 7);

        scroll.scroll_down(25);
        assert_eq!(scroll_offset(40, 10, &scroll), 32);

        let short = ScrollState::default();
        assert_eq!(scroll_offset(3, 10, &short), 0);
    }

    #[test]
    fn incoming_assistant_text_keeps_a_paused_scroll_anchor() {
        let mut conversation = state("talk-1");
        assert_eq!(scroll_offset(20, 10, &conversation.scroll), 12);
        conversation.scroll.scroll_up(5);
        let (mut app, _) = app_with(vec![conversation]);

        app.selected_mut().note_transcript_append();
        app.selected_mut().transcript.push(TranscriptEntry {
            role: EntryRole::Agent(None),
            commit: None,
            text: "new response".to_string(),
            pending_id: None,
        });

        assert_eq!(scroll_offset(40, 10, &app.selected().scroll), 7);
        assert!(app.selected().unread_below);
        app.scroll_down(usize::MAX);
        assert!(!app.selected().unread_below);
    }

    #[test]
    fn paused_transcript_shows_unread_and_rendered_lines_below() {
        let mut conversation = state("talk-1");
        conversation.transcript.push(TranscriptEntry {
            role: EntryRole::Agent(None),
            commit: None,
            text: (0..60)
                .map(|line| format!("existing line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            pending_id: None,
        });
        let (mut app, _) = app_with(vec![conversation]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        app.scroll_up(8);

        app.selected_mut().note_transcript_append();
        app.selected_mut().transcript.push(TranscriptEntry {
            role: EntryRole::Agent(None),
            commit: None,
            text: "new response".to_string(),
            pending_id: None,
        });
        terminal.draw(|frame| render(&app, frame)).unwrap();

        let rendered = rendered_main_pane(&terminal).join("\n");
        assert!(rendered.contains("New message ·"));
        assert!(rendered.contains("lines below ↓"));

        app.scroll_down(usize::MAX);
        terminal.draw(|frame| render(&app, frame)).unwrap();
        assert!(!rendered_main_pane(&terminal)
            .join("\n")
            .contains("lines below ↓"));
    }

    #[test]
    fn activity_browser_selects_and_scrolls_full_details() {
        let mut conversation = state("talk-1");
        conversation.activities = vec![activity(1), activity(2), activity(3)];
        let (mut app, _) = app_with(vec![conversation]);

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Activity);
        assert_eq!(app.selected().activity_selection, Some(2));

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.selected().activity_selection, Some(1));
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.selected().activity_detail_scroll, 8);

        let area = Rect::new(0, 0, 100, 30);
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 60,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(app.handle_mouse(wheel, area), MouseAction::Redraw);
        assert_eq!(app.selected().activity_detail_scroll, 11);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected().activity_selection, Some(2));
        assert_eq!(app.selected().activity_detail_scroll, 0);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.view, View::Chat);
    }

    #[test]
    fn live_activity_uses_tool_specific_verbs_and_concise_summaries() {
        let mut activity = activity(1);
        activity.name = "read".to_string();
        activity.summary = "read crates/caos-cli/src/lib.rs".to_string();
        activity.state = ActivityState::Running;

        assert_eq!(activity.running_verb(), "Reading");
        assert_eq!(activity.running_summary(), "crates/caos-cli/src/lib.rs");

        activity.name = "bash".to_string();
        activity.summary = "$ cargo test".to_string();
        assert_eq!(activity.running_verb(), "Running");
        assert_eq!(activity.running_summary(), "$ cargo test");

        activity.name = "cat".to_string();
        activity.summary = "cat README.md".to_string();
        assert_eq!(activity.running_verb(), "Reading");
        assert_eq!(activity.running_summary(), "README.md");

        activity.name = "unknown".to_string();
        activity.summary = "unknown something".to_string();
        assert_eq!(activity.running_verb(), "Running");
    }

    #[test]
    fn replayed_activity_restores_tool_results() {
        let request = "a".repeat(40);
        let activities = replayed_activities(&[
            TurnEvent::ToolCall {
                step_commit: "1".repeat(40),
                request: request.clone(),
                round: 3,
                tool_use_id: "tool-1".to_string(),
                name: "read".to_string(),
                summary: "read README.md".to_string(),
            },
            TurnEvent::ToolResult {
                step_commit: "2".repeat(40),
                request,
                round: 3,
                tool_use_id: "tool-1".to_string(),
                is_error: false,
                content: "README contents".to_string(),
            },
        ]);

        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].name, "read");
        assert_eq!(activities[0].state, ActivityState::Succeeded);
        assert_eq!(activities[0].detail, "README contents");
    }

    #[test]
    fn replayed_activity_scopes_reused_ids_by_request_and_round() {
        let request_a = "a".repeat(40);
        let request_b = "b".repeat(40);
        let events = [
            TurnEvent::ToolCall {
                step_commit: "1".repeat(40),
                request: request_a.clone(),
                round: 0,
                tool_use_id: "reused".to_string(),
                name: "read".to_string(),
                summary: "first call".to_string(),
            },
            TurnEvent::ToolResult {
                step_commit: "2".repeat(40),
                request: request_a.clone(),
                round: 0,
                tool_use_id: "reused".to_string(),
                is_error: false,
                content: "first result".to_string(),
            },
            TurnEvent::ToolCall {
                step_commit: "3".repeat(40),
                request: request_a.clone(),
                round: 1,
                tool_use_id: "reused".to_string(),
                name: "read".to_string(),
                summary: "second call".to_string(),
            },
            TurnEvent::ToolResult {
                step_commit: "4".repeat(40),
                request: request_a,
                round: 1,
                tool_use_id: "reused".to_string(),
                is_error: true,
                content: "second result".to_string(),
            },
            TurnEvent::ToolCall {
                step_commit: "5".repeat(40),
                request: request_b.clone(),
                round: 1,
                tool_use_id: "reused".to_string(),
                name: "read".to_string(),
                summary: "third call".to_string(),
            },
            TurnEvent::ToolResult {
                step_commit: "6".repeat(40),
                request: request_b,
                round: 1,
                tool_use_id: "reused".to_string(),
                is_error: false,
                content: "third result".to_string(),
            },
        ];

        let activities = replayed_activities(&events);
        assert_eq!(activities.len(), 3);
        assert_eq!(activities[0].state, ActivityState::Succeeded);
        assert_eq!(activities[0].detail, "first result");
        assert_eq!(activities[1].state, ActivityState::Failed);
        assert_eq!(activities[1].detail, "second result");
        assert_eq!(activities[2].state, ActivityState::Succeeded);
        assert_eq!(activities[2].detail, "third result");
    }

    #[test]
    fn new_activity_follows_only_a_selection_at_the_tail() {
        let mut conversation = state("talk-1");
        conversation.activities = vec![activity(1), activity(2)];
        conversation.activity_selection = Some(0);
        let (mut app, _) = app_with(vec![conversation]);

        app.on_turn_event(
            0,
            TurnEvent::ToolCall {
                step_commit: "3".repeat(40),
                request: "a".repeat(40),
                round: 0,
                tool_use_id: "tool-3".to_string(),
                name: "bash".to_string(),
                summary: "third".to_string(),
            },
        );
        assert_eq!(app.selected().activity_selection, Some(0));

        app.selected_mut().activity_selection = Some(2);
        app.on_turn_event(
            0,
            TurnEvent::ToolCall {
                step_commit: "4".repeat(40),
                request: "a".repeat(40),
                round: 0,
                tool_use_id: "tool-4".to_string(),
                name: "bash".to_string(),
                summary: "fourth".to_string(),
            },
        );
        assert_eq!(app.selected().activity_selection, Some(3));
    }

    #[test]
    fn paragraph_scroll_counts_wrapped_visual_rows() {
        let paragraph = Paragraph::new(
            "this single logical line wraps across several visual rows in a narrow viewport",
        )
        .wrap(Wrap { trim: false });
        let area = Rect::new(0, 0, 18, 5);
        let mut scroll = ScrollState::default();
        let tail = paragraph_scroll(&paragraph, area, &scroll);
        assert!(tail > 0);
        scroll.scroll_up(2);
        assert!(paragraph_scroll(&paragraph, area, &scroll) < tail);
    }

    #[test]
    fn transcript_uses_all_space_above_the_composer() {
        let terminal = Rect::new(0, 0, 100, 30);
        let mut conversation = state("talk-1");
        assert!(content_contains(&conversation, terminal, 27, 1));
        assert!(content_contains(&conversation, terminal, 99, 25));
        assert!(!content_contains(&conversation, terminal, 25, 12));
        assert!(!content_contains(&conversation, terminal, 27, 26));

        conversation.composer.insert_str("one\ntwo\nthree");
        assert!(content_contains(&conversation, terminal, 99, 23));
        assert!(!content_contains(&conversation, terminal, 99, 24));
    }

    #[test]
    fn live_activity_reserves_space_below_the_transcript() {
        let terminal = Rect::new(0, 0, 100, 30);
        let mut conversation = state("talk-1");
        assert!(transcript_contains(&conversation, terminal, 27, 22));

        conversation.running = true;
        assert!(!transcript_contains(&conversation, terminal, 27, 23));
        assert!(transcript_contains(&conversation, terminal, 27, 22));
    }

    #[test]
    fn completion_status_does_not_move_a_paused_transcript() {
        let mut conversation = state("talk-1");
        conversation.running = true;
        conversation.status = "calling model".to_string();
        conversation.transcript.push(TranscriptEntry {
            role: EntryRole::Agent(None),
            commit: None,
            text: (0..60)
                .map(|line| format!("line {line:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
            pending_id: None,
        });
        let (mut app, _) = app_with(vec![conversation]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(&app, frame)).unwrap();
        app.scroll_up(8);
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let before = rendered_main_pane(&terminal);
        let first_visible_line = before
            .iter()
            .find(|row| row.contains("line "))
            .unwrap()
            .clone();

        app.selected_mut().running = false;
        app.selected_mut().status = "completed e1769972f6".to_string();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let after = rendered_main_pane(&terminal);

        assert_eq!(
            after.iter().find(|row| row.contains("line ")).unwrap(),
            &first_visible_line
        );
        assert!(!after.join("\n").contains("completed e1769972f6"));

        app.scroll_down(usize::MAX);
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let at_tail = rendered_main_pane(&terminal).join("\n");
        assert!(!at_tail.contains("Status"));
        assert!(!at_tail.contains("completed e1769972f6"));
    }

    #[test]
    fn live_activity_distinguishes_system_and_model_phases() {
        let mut conversation = state("talk-1");
        conversation.running = true;
        conversation.status = "preparing turn".to_string();
        let (mut app, _) = app_with(vec![conversation]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Chugging…"));

        app.on_turn_event(0, TurnEvent::PhaseStarted(TurnPhase::Model));
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Thinking…"));
        assert!(!rendered.contains("Chugging…"));
    }

    #[test]
    fn live_activity_indicator_pulses_while_busy() {
        let mut conversation = state("talk-1");
        conversation.running = true;
        let (mut app, _) = app_with(vec![conversation]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("· Chugging…"));

        app.advance_animation();
        app.advance_animation();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("✽ Chugging…"));
    }

    #[test]
    fn mouse_drag_selects_visible_transcript_text_for_copy() {
        let mut selected = state("talk-1");
        selected.transcript.push(TranscriptEntry {
            role: EntryRole::Human,
            commit: None,
            text: "hello".to_string(),
            pending_id: None,
        });
        let (mut app, _) = app_with(vec![selected]);
        let area = Rect::new(0, 0, 100, 30);
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 27, 2), area),
            MouseAction::Redraw
        );
        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 29, 2), area),
            MouseAction::Redraw
        );
        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 29, 2), area),
            MouseAction::Copy("You".to_string())
        );
        app.note_copy("You");

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        for column in 27..=29 {
            let cell = terminal.backend().buffer().cell((column, 2)).unwrap();
            assert_eq!(cell.bg, Color::Cyan);
            assert_eq!(cell.fg, Color::Black);
        }
        let footer: String = terminal
            .backend()
            .buffer()
            .content
            .chunks(100)
            .last()
            .unwrap()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(footer.ends_with(" 3 chars copied "));
    }

    #[test]
    fn stale_transcript_drag_state_is_ignored_instead_of_panicking() {
        let mut selected = state("talk-1");
        selected.transcript.push(TranscriptEntry {
            role: EntryRole::Human,
            commit: None,
            text: "hello".to_string(),
            pending_id: None,
        });
        let (mut app, _) = app_with(vec![selected]);
        let area = Rect::new(0, 0, 100, 30);
        let mouse = |kind| MouseEvent {
            kind,
            column: 27,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };

        app.selecting_transcript = true;
        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left)), area),
            MouseAction::Ignored
        );
        assert!(!app.selecting_transcript);

        app.selecting_transcript = true;
        assert_eq!(
            app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left)), area),
            MouseAction::Ignored
        );
        assert!(!app.selecting_transcript);
    }

    #[test]
    fn full_layout_renders_chat_activity_and_prompt() {
        let mut selected = state("review-api");
        selected.transcript = vec![
            TranscriptEntry {
                role: EntryRole::Human,
                commit: Some("a".repeat(40)),
                text: "Please run the tests".to_string(),
                pending_id: None,
            },
            TranscriptEntry {
                role: EntryRole::Agent(Some("claude-sonnet-5".to_string())),
                commit: Some("b".repeat(40)),
                text: "Running them now.".to_string(),
                pending_id: None,
            },
        ];
        selected.activities = vec![Activity {
            request: "a".repeat(40),
            round: 0,
            id: "tool-1".to_string(),
            step_commit: "c".repeat(40),
            name: "bash".to_string(),
            summary: "$ cargo test".to_string(),
            detail: "12 tests passed".to_string(),
            state: ActivityState::Running,
        }];
        selected.status = "calling model".to_string();
        selected.running = true;
        selected.composer.insert_str("follow-up");
        let (mut app, _) = app_with(vec![selected, state("other-chat")]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Conversations"));
        assert!(rendered.contains("review-api"));
        assert!(rendered.contains("other-chat"));
        assert!(rendered.contains("head bbbbbbb"));
        assert!(rendered.contains("Please run the tests"));
        assert!(rendered.contains("Agent (sonnet-5)"));
        assert!(rendered.contains("Running…"));
        assert!(rendered.contains("$ cargo test"));
        assert!(rendered.contains("Ctrl+T expands"));
        assert!(rendered.contains("follow-up"));
        assert!(rendered.contains("Enter/^J newline"));
        assert!(rendered.contains("^S send"));
        assert!(rendered.contains("^L checkout"));
        assert!(rendered.contains("^P×2 publish"));
        assert!(!rendered.contains("Alt+Enter"));

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let legacy_help: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(legacy_help.contains("Ctrl+S          send the prompt"));
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));

        app.set_enhanced_keyboard(true);
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let enhanced_footer: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(enhanced_footer.contains("^Enter send"));
        assert!(!enhanced_footer.contains("^S send"));
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let enhanced_help: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(enhanced_help.contains("Ctrl+Enter      send the prompt"));
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Activity);
        assert_eq!(app.selected().activity_selection, Some(0));
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let expanded: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(expanded.contains("ccccccc"));
        assert!(expanded.contains("$ cargo test"));
        assert!(expanded.contains("12 tests passed"));

        app.selected_mut().running = false;
        app.selected_mut().diff = Some(WorkspaceDiff {
            base_commit: "a".repeat(40),
            head: "b".repeat(40),
            patch: "diff --git a/a b/a".to_string(),
        });
        let (publish_repo, publish_remote, _) = repo_with_default_branch("publish-prompt", "trunk");
        app.repo_dir = publish_repo.clone();
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(
            app.confirm_action,
            Some(ConfirmAction::Publish {
                default_base: "trunk".to_string(),
                base_input: String::new(),
            })
        );
        assert!(app.selected().status.contains("enter a PR base branch"));
        assert!(!app.selected().publishing);

        for ch in "release/next".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(
            app.confirm_action,
            Some(ConfirmAction::Publish {
                default_base: "trunk".to_string(),
                base_input: "release/next".to_string(),
            })
        );
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let publish_prompt = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(publish_prompt.contains("Base branch: release/next"));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.confirm_action.is_none());
        assert!(!app.selected().publish_prompt);
        std::fs::remove_dir_all(publish_repo).unwrap();
        std::fs::remove_dir_all(publish_remote).unwrap();
    }

    #[test]
    fn idle_chat_header_keeps_only_the_title_and_head_metadata() {
        let mut conversation = state("A concise title");
        conversation.transcript.push(TranscriptEntry {
            role: EntryRole::Agent(None),
            commit: Some("b".repeat(40)),
            text: "done".to_string(),
            pending_id: None,
        });
        let (app, _) = app_with(vec![conversation]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(&app, frame)).unwrap();

        let header = rendered_header(&terminal);
        assert!(header.contains("caos"));
        assert!(header.contains("user tester"));
        assert!(header.contains("A concise title"));
        assert!(header.contains("head bbbbbbb"));
        assert!(!header.contains("idle"));
        assert!(!header.contains("[chat]"));
        assert!(!header.contains("0 running"));
    }

    #[test]
    fn ctrl_l_checks_out_the_conversation_on_the_first_press() {
        let dir = throwaway_repo("ctrl-l");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["config", "user.name", "Test User"]);
        git(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("file.txt"), "base\n").unwrap();
        git(&["add", "file.txt"]);
        git(&["commit", "-q", "-m", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        std::fs::write(dir.join("file.txt"), "conversation\n").unwrap();
        git(&["commit", "-qam", "conversation"]);
        let head = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--detach", "-q", &base]);

        let mut selected = state("talk-1");
        selected.diff = Some(WorkspaceDiff {
            base_commit: base,
            head: head.clone(),
            patch: "changed".to_string(),
        });
        let (mut app, _) = app_with(vec![selected]);
        app.repo_dir = dir.clone();

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));

        assert_eq!(git(&["rev-parse", "HEAD"]), head);
        assert!(app.selected().status.contains("checked out"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transcript_renders_markdown_emphasis_styles() {
        let mut selected = state("markdown");
        selected.transcript.push(TranscriptEntry {
            role: EntryRole::Agent(None),
            commit: None,
            text: "plain **bold** and _italic_".to_string(),
            pending_id: None,
        });
        let (app, _) = app_with(vec![selected]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(&app, frame)).unwrap();

        let width = terminal.backend().buffer().area.width as usize;
        let row = terminal
            .backend()
            .buffer()
            .content
            .chunks(width)
            .find(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("plain bold and italic")
            })
            .unwrap();
        let transcript_row = &row[26..];
        let rendered: String = transcript_row.iter().map(|cell| cell.symbol()).collect();
        let bold = transcript_row
            .windows("bold".len())
            .position(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>() == "bold")
            .unwrap();
        let italic = transcript_row
            .windows("italic".len())
            .position(|cells| {
                cells.iter().map(|cell| cell.symbol()).collect::<String>() == "italic"
            })
            .unwrap();
        assert!(transcript_row[bold].modifier.contains(Modifier::BOLD));
        assert!(!transcript_row[bold].modifier.contains(Modifier::ITALIC));
        assert!(transcript_row[italic].modifier.contains(Modifier::ITALIC));
        assert!(!rendered.contains("**"));
        assert!(!rendered.contains("_italic_"));
    }

    #[test]
    fn conversation_list_renders_titles_and_live_status_without_ids() {
        let internal_id = "0123456789abcdef0123456789abcdef01234567";
        let mut selected = ConversationState::new(
            internal_id.to_string(),
            "Readable title".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        assert_eq!(
            selected.sidebar_text(16),
            ("Readable title".to_string(), String::new())
        );
        selected.running = true;
        selected.status = "calling model…".to_string();
        assert_eq!(selected.sidebar_text(16).1, "calling model…".to_string());
        selected.running = false;
        let mut child = state("Child task");
        child.parent = Some(internal_id.to_string());
        let mut generating_title = ConversationState::new(
            internal_id.to_string(),
            "Existing title".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        generating_title.generating_title = true;
        let (mut app, _) = app_with(vec![
            selected,
            child,
            generating_title,
            state("Empty title"),
        ]);
        app.selected = 2;
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(&app, frame)).unwrap();

        let rendered_rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let sidebar_rows: Vec<String> = rendered_rows
            .iter()
            .map(|row| row.chars().take(26).collect())
            .collect();
        let title_row = sidebar_rows
            .iter()
            .position(|row| row.starts_with('│') && row.contains("Readable title"))
            .unwrap();
        assert!(sidebar_rows[title_row + 1]
            .trim_matches('│')
            .trim()
            .is_empty());
        let child_row = sidebar_rows
            .iter()
            .position(|row| row.contains("Child task"))
            .unwrap();
        assert_eq!(
            sidebar_rows[child_row].find("Child task").unwrap(),
            sidebar_rows[title_row].find("Readable title").unwrap() + 2
        );
        assert!(
            sidebar_rows
                .iter()
                .any(|row| row.contains("Generating title")),
            "{sidebar_rows:#?}"
        );
        let empty_title_row = sidebar_rows
            .iter()
            .position(|row| row.starts_with('│') && row.contains("Empty title"))
            .unwrap();
        assert!(sidebar_rows[empty_title_row + 1]
            .trim_matches('│')
            .trim()
            .is_empty());
        let sidebar = sidebar_rows.join("\n");
        assert!(!sidebar.contains(internal_id));
        assert!(!sidebar.contains("Latest human message"));
    }

    #[test]
    fn switching_conversations_keeps_background_turn_state() {
        let mut first = state("talk-1");
        first.running = true;
        first.status = "calling model".to_string();
        let (mut app, tx) = app_with(vec![first, state("talk-2")]);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(app.selected().id, "talk-2");
        assert!(app.conversations[0].running);

        tx.send(UiMessage::Turn {
            conversation: "talk-1".to_string(),
            event: TurnEvent::Status("running a tool".to_string()),
        })
        .unwrap();
        assert!(app.drain_messages());
        assert_eq!(app.conversations[0].status, "running a tool");
        assert_eq!(app.selected().id, "talk-2");
    }

    #[test]
    fn publish_errors_use_the_command_panel_without_changing_the_transcript() {
        let mut conversation = state("talk-1");
        conversation.status = "completed abc1234".to_string();
        conversation.publishing = true;
        conversation.scroll.offset = Some(12);
        let (mut app, tx) = app_with(vec![conversation]);

        tx.send(UiMessage::Published {
            conversation: "talk-1".to_string(),
            result: Err("gh could not open the PR".to_string()),
        })
        .unwrap();
        assert!(app.drain_messages());

        let state = app.selected();
        assert!(!state.publishing);
        assert!(state.status.is_empty());
        assert_eq!(state.scroll.offset, Some(12));
        assert_eq!(
            state.command_error.as_deref(),
            Some("PR failed: gh could not open the PR")
        );
        assert!(state.transcript.is_empty());

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(!rows[0].contains("PR failed"));
        let rendered = rows.join("\n");
        assert!(rendered.contains("PR failed: gh could not open the PR"));
        assert!(!rendered.contains("Status"));
    }

    #[test]
    fn active_turn_accepts_an_interjection() {
        let (repo, remote, _) = repo_with_default_branch("active-interjection", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let (head, request) = seed_queued_conversation(&repo, "talk-1", "Alice", "first prompt");
        let mut conversation = state("talk-1");
        conversation.running = true;
        conversation.local_turn = true;
        conversation.remote_head = Some(head);
        conversation.active_request = Some(request);
        conversation.status = "running a tool".to_string();
        conversation.turn_phase = TurnPhase::Model;
        conversation.activities.push(activity(1));
        conversation.activity_selection = Some(0);
        conversation.composer.insert_str("another prompt");
        let (mut app, _) = app_with(vec![conversation]);
        app.repo_dir = repo.clone();

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert_eq!(app.selected().transcript.len(), 1);
        assert_eq!(app.selected().transcript[0].role, EntryRole::Human);
        assert_eq!(app.selected().transcript[0].text, "another prompt");
        assert_eq!(app.selected().pending_submissions.len(), 1);
        assert_eq!(app.selected().status, "running a tool");
        assert_eq!(app.selected().turn_phase, TurnPhase::Model);
        assert_eq!(app.selected().activities, vec![activity(1)]);
        assert!(app.selected().running);
        assert!(app.selected().local_turn);
        assert!(app.selected().command_error.is_none());

        wait_for_pending_submission(&mut app, 0);
        assert_eq!(app.selected().status, "running a tool");
        assert_eq!(app.selected().turn_phase, TurnPhase::Model);
        assert!(app.selected().activities.is_empty());
        assert!(app.selected().running);
        assert!(app.selected().local_turn);
        assert!(app.selected().command_error.is_none());
        assert_eq!(
            app.selected()
                .transcript
                .iter()
                .filter(|entry| entry.text == "another prompt")
                .count(),
            1
        );
        assert!(app
            .selected()
            .transcript
            .iter()
            .find(|entry| entry.text == "another prompt")
            .unwrap()
            .commit
            .is_some());

        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn pending_interjection_survives_a_stale_reload_until_its_commit_is_visible() {
        let (repo, remote, _) = repo_with_default_branch("pending-interjection", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        seed_queued_conversation(&repo, "talk-1", "Alice", "first prompt");
        let transport = GitTransport::discover(&repo).unwrap();
        let load = conversation_load(&transport, "talk-1").unwrap().unwrap();
        let durable_commit = load.replay.turns[0].commit.clone();
        let mut conversation = state("talk-1");
        let pending_id = conversation.queue_pending_submission("another prompt".to_string());

        conversation.apply_load(load.clone(), "Alice");
        assert_eq!(conversation.pending_submissions.len(), 1);
        assert_eq!(
            conversation.transcript.last().unwrap().text,
            "another prompt"
        );
        assert_eq!(
            conversation.transcript.last().unwrap().pending_id,
            Some(pending_id)
        );

        conversation.mark_pending_submission(pending_id, durable_commit);
        conversation.apply_load(load, "Alice");
        assert!(conversation.pending_submissions.is_empty());
        assert_eq!(conversation.transcript.len(), 1);
        assert!(conversation.transcript[0].pending_id.is_none());

        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn first_durable_load_replays_activity_and_preserves_local_lifecycle() {
        let head = "a".repeat(40);
        let request = "b".repeat(40);
        let mut conversation = state("talk-1");
        conversation.running = true;
        conversation.local_turn = true;
        conversation.status = "running a tool".to_string();
        conversation.turn_phase = TurnPhase::Model;
        assert!(conversation.active_request.is_none());

        conversation.apply_load(
            ConversationLoad {
                snapshot: ConversationSnapshot {
                    id: "talk-1".to_string(),
                    head: head.clone(),
                    title: "talk-1".to_string(),
                    status: "queued".to_string(),
                    request: Some(request.clone()),
                    request_head: Some("c".repeat(40)),
                    interrupted: false,
                    messages: Vec::new(),
                },
                replay: ConversationReplay {
                    turns: Vec::new(),
                    activity: vec![TurnEvent::ToolCall {
                        step_commit: "e".repeat(40),
                        request: request.clone(),
                        round: 2,
                        tool_use_id: "sleep".to_string(),
                        name: "bash".to_string(),
                        summary: "$ sleep 120; echo done".to_string(),
                    }],
                },
                workspace_diff: WorkspaceDiff {
                    base_commit: "d".repeat(40),
                    head: head.clone(),
                    patch: String::new(),
                },
            },
            "tester",
        );

        assert_eq!(conversation.status, "running a tool");
        assert_eq!(conversation.turn_phase, TurnPhase::Model);
        assert_eq!(conversation.activities.len(), 1);
        assert_eq!(conversation.activities[0].id, "sleep");
        assert_eq!(conversation.activities[0].summary, "$ sleep 120; echo done");
        assert_eq!(conversation.activity_selection, Some(0));
        assert_eq!(
            conversation.active_request.as_deref(),
            Some(request.as_str())
        );
        assert_eq!(conversation.remote_head.as_deref(), Some(head.as_str()));
    }

    #[test]
    fn learning_a_commit_retires_an_already_visible_optimistic_row() {
        let mut conversation = state("talk-1");
        let pending_id = conversation.queue_pending_submission("hello".to_string());
        let commit = "a".repeat(40);
        conversation.transcript.insert(
            0,
            TranscriptEntry {
                role: EntryRole::Human,
                commit: Some(commit.clone()),
                text: "hello".to_string(),
                pending_id: None,
            },
        );

        conversation.mark_pending_submission(pending_id, commit);

        assert!(conversation.pending_submissions.is_empty());
        assert_eq!(conversation.transcript.len(), 1);
        assert!(conversation.transcript[0].pending_id.is_none());
    }

    #[test]
    fn first_committed_submission_materializes_virtual_state() {
        let mut conversation = ConversationState::new_virtual(
            "new-talk".to_string(),
            "talk-1".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        let pending_id = conversation.queue_pending_submission("hello".to_string());
        let (mut app, tx) = app_with(vec![conversation]);

        tx.send(UiMessage::SubmissionCommitted {
            conversation: "new-talk".to_string(),
            pending_id,
            commit: "a".repeat(40),
        })
        .unwrap();

        assert!(app.drain_messages());
        assert!(!app.selected().virtual_conversation);
    }

    #[test]
    fn failed_submission_restores_text_without_replacing_a_newer_draft() {
        let mut conversation = state("talk-1");
        let pending_id = conversation.queue_pending_submission("failed message".to_string());
        conversation.composer.insert_str("newer draft");

        conversation.restore_pending_submission(pending_id);

        assert!(conversation.pending_submissions.is_empty());
        assert!(conversation.transcript.is_empty());
        assert_eq!(conversation.composer.text, "newer draft\n\nfailed message");
    }

    #[test]
    fn stale_interjection_refresh_cannot_roll_back_a_newer_head() {
        let old_head = "a".repeat(40);
        let new_head = "b".repeat(40);
        let mut conversation = state("talk-1");
        conversation.remote_head = Some(new_head.clone());
        conversation.status = "completed bbbbbbbb".to_string();
        let (mut app, tx) = app_with(vec![conversation]);
        tx.send(UiMessage::InterjectionRefreshed {
            conversation: "talk-1".to_string(),
            observed_head: Some(old_head.clone()),
            load: Ok(Box::new(ConversationLoad {
                snapshot: ConversationSnapshot {
                    id: "talk-1".to_string(),
                    head: old_head.clone(),
                    title: "talk-1".to_string(),
                    status: "queued".to_string(),
                    request: Some("c".repeat(40)),
                    request_head: Some("d".repeat(40)),
                    interrupted: false,
                    messages: Vec::new(),
                },
                replay: ConversationReplay {
                    turns: Vec::new(),
                    activity: Vec::new(),
                },
                workspace_diff: WorkspaceDiff {
                    base_commit: "e".repeat(40),
                    head: old_head,
                    patch: String::new(),
                },
            })),
        })
        .unwrap();

        assert!(app.drain_messages());
        assert_eq!(
            app.selected().remote_head.as_deref(),
            Some(new_head.as_str())
        );
        assert_eq!(app.selected().status, "completed bbbbbbbb");
        assert!(!app.selected().running);
    }

    #[test]
    fn stale_remote_poll_result_does_not_overwrite_newer_local_state() {
        let mut conversation = state("local title");
        conversation.id = "shared".to_string();
        conversation.remote_head = Some("b".repeat(40));
        let (mut app, _) = app_with(vec![conversation]);

        assert!(!app.apply_remote_poll(vec![RemotePollEntry {
            summary: UserConversationSummary {
                id: "shared".to_string(),
                title: "stale title".to_string(),
                head: "a".repeat(40),
                updated_unix: 1,
                parent: None,
            },
            observed_head: Some("a".repeat(40)),
            observed_title: Some("old local title".to_string()),
            load: None,
        }]));

        assert_eq!(app.selected().title, "local title");
        assert_eq!(
            app.selected().remote_head.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn publishing_gate_keeps_the_draft_and_shows_the_command_error_panel() {
        let mut conversation = state("talk-1");
        conversation.publishing = true;
        conversation.status = "publishing".to_string();
        conversation.composer.insert_str("do not send yet");
        let (mut app, _) = app_with(vec![conversation]);

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert!(app.selected().publishing);
        assert_eq!(app.selected().composer.text, "do not send yet");
        assert!(app.selected().transcript.is_empty());
        assert_eq!(
            app.selected().command_error.as_deref(),
            Some("finish publishing before sending another message")
        );
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        assert!(rendered_main_pane(&terminal)
            .join("\n")
            .contains("finish publishing before sending another message"));
    }

    #[test]
    fn routine_idle_status_is_not_rendered() {
        let mut conversation = state("talk-1");
        conversation.status = "ready".to_string();
        let (app, _) = app_with(vec![conversation]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(&app, frame)).unwrap();

        let rendered = rendered_main_pane(&terminal).join("\n");
        assert!(!rendered.contains("Status"));
        assert!(!rendered.contains("ready"));
    }

    #[test]
    fn successful_publish_adds_a_caos_transcript_entry() {
        let mut conversation = state("talk-1");
        conversation.publishing = true;
        conversation.status = "publishing".to_string();
        let (mut app, tx) = app_with(vec![conversation]);

        tx.send(UiMessage::Published {
            conversation: "talk-1".to_string(),
            result: Ok("https://github.com/Metta-AI/caos/pull/54".to_string()),
        })
        .unwrap();
        assert!(app.drain_messages());

        let state = app.selected();
        assert!(!state.publishing);
        assert!(state.status.is_empty());
        assert_eq!(state.transcript.last().unwrap().role, EntryRole::Info);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered = rendered_main_pane(&terminal).join("\n");
        assert!(rendered.contains("CAOS"));
        assert!(rendered.contains("PR ready: https://github.com/Metta-AI/caos/pull/54"));
        assert!(!rendered.contains("Status"));
    }

    #[test]
    fn ctrl_e_removes_virtual_conversations_and_replaces_the_last_one() {
        let (mut app, _) = app_with(vec![state("talk-1"), state("talk-2"), state("talk-3")]);
        // Archiving lives in the conversation list, so focus it first.
        app.focus = Focus::List;
        // Replacing the last conversation mints a fresh id through the
        // transport, so point the app at a real (scratch) repo.
        let (dir, remote, tip) = repo_with_default_branch("ctrl-e", "main");
        app.repo_dir = dir.clone();
        app.selected = 1;

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(
            app.conversations
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<Vec<_>>(),
            ["talk-1", "talk-3"]
        );
        assert_eq!(app.selected().id, "talk-3");

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.selected().id, "talk-1");

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.conversations.len(), 1);
        assert_eq!(app.selected().title, "talk-2");
        assert_ne!(app.selected().id, app.selected().title);
        assert_eq!(
            app.selected().turn_options.base.as_deref(),
            Some(tip.as_str())
        );
        assert!(app.selected().remote_head.is_none());
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn ctrl_e_keeps_a_busy_conversation_open() {
        let mut running = state("talk-1");
        running.running = true;
        let (mut app, _) = app_with(vec![running, state("talk-2")]);
        app.focus = Focus::List;

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));

        assert_eq!(app.conversations.len(), 2);
        assert_eq!(app.selected().id, "talk-1");
        assert!(app.selected().status.is_empty());
        assert!(app
            .selected()
            .command_error
            .as_deref()
            .unwrap()
            .contains("before archiving"));
    }

    #[test]
    fn ctrl_e_moves_to_line_ends_in_the_conversation_and_never_archives() {
        let (mut app, _) = app_with(vec![state("talk-1"), state("talk-2")]);
        app.selected_mut().composer.insert_str("first\nsecond");

        // In the conversation pane, Ctrl+A/Ctrl+E move to the line's ends
        // and leave every conversation in place.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.selected().composer.cursor_row_col(), (1, 0));
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.selected().composer.cursor_row_col(), (1, 6));
        assert_eq!(app.conversations.len(), 2);
        assert_eq!(app.selected().composer.text, "first\nsecond");
    }

    #[test]
    fn ctrl_w_deletes_the_previous_word_in_the_conversation() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("one two three");

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.selected().composer.text, "one two ");
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.selected().composer.text, "one ");
    }

    #[test]
    fn ctrl_k_kills_to_the_end_of_the_line_in_the_conversation() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("first\nsecond");
        app.selected_mut().composer.move_home();

        // Kill from the cursor to the end of the current line, leaving the
        // preceding line and its newline intact.
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.selected().composer.text, "first\n");

        // With the cursor already at a line's end, a second Ctrl+K swallows the
        // newline, joining the next line onto this one.
        app.selected_mut().composer.clear();
        app.selected_mut().composer.insert_str("first\nsecond");
        app.selected_mut().composer.move_vertical(true);
        app.selected_mut().composer.move_end();
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.selected().composer.text, "firstsecond");
    }

    #[test]
    fn new_conversation_is_available_from_either_focus() {
        // Force transport discovery to fail so a dispatched attempt has an
        // observable command error without depending on the test runner's
        // current repository or remote.
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.repo_dir = std::env::temp_dir().join(format!(
            "caos-cli-tui-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        app.focus = Focus::List;
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Conversation);
        assert!(app.selected().status.is_empty());
        assert!(app.selected().command_error.is_some());
        assert!(app.selected().transcript.is_empty());

        app.selected_mut().command_error = None;
        app.focus = Focus::Conversation;
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(app.selected().status.is_empty());
        assert!(app.selected().command_error.is_some());
        assert!(app.selected().transcript.is_empty());
    }

    #[test]
    fn ctrl_n_focuses_the_new_conversation() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        // Pressing Ctrl+N from the list moves focus into the conversation so
        // the composer is ready for the first prompt.
        app.focus = Focus::List;

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));

        assert_eq!(app.focus, Focus::Conversation);
    }

    #[test]
    fn title_command_does_not_change_conversation_identity() {
        let conversation = ConversationState::new_virtual(
            "stable-id".to_string(),
            "talk-1".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        let (mut app, _) = app_with(vec![conversation]);
        app.selected_mut()
            .composer
            .insert_str("/title Mutable title");

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert_eq!(app.selected().id, "stable-id");
        assert_eq!(app.selected().title, "Mutable title");
        app.selected_mut()
            .apply_automatic_title("This prompt must not replace it");
        assert_eq!(app.selected().title, "Mutable title");
    }

    #[test]
    fn reload_surfaces_history_errors_instead_of_showing_an_empty_chat() {
        // A throwaway repo so the transport discovers a real working tree; the
        // conversation itself is absent, which is the error we're asserting on.
        let dir = throwaway_repo("reload");

        let mut conversation = state("missing-conversation-for-reload-test");
        assert_eq!(scroll_offset(20, 10, &conversation.scroll), 12);
        conversation.scroll.scroll_up(5);
        let transport = GitTransport::discover(&dir).unwrap();
        let _ = conversation.reload(&transport, "alice");
        assert!(conversation.diff.is_none());
        assert!(conversation.status.is_empty());
        let error = conversation.transcript.last().unwrap();
        assert_eq!(error.role, EntryRole::Notice);
        assert!(error.text.contains("loading conversation failed"));
        assert!(error.text.contains("caos"));
        assert_eq!(conversation.scroll.offset, Some(7));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remote_poll_discovers_invited_conversations_and_names_the_other_user() {
        let (repo, remote, _) = repo_with_default_branch("remote-poll", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let transport = GitTransport::discover(&repo).unwrap();
        let (_, request) = seed_queued_conversation(&repo, "shared", "Alice", "hello from Alice");
        invite_user_to_conversation(&transport, "Bob", "shared").unwrap();

        let (mut app, _) = app_with(vec![state("local")]);
        app.repo_dir = repo.clone();
        app.user = "Bob".to_string();
        wait_for_remote_poll(&mut app);

        let shared = app
            .conversations
            .iter()
            .find(|conversation| conversation.id == "shared")
            .unwrap();
        assert!(shared.running);
        assert_eq!(shared.active_request.as_deref(), Some(request.as_str()));
        assert_eq!(
            shared.reconciling_request.as_deref(),
            Some(request.as_str())
        );
        assert!(matches!(
            &shared.transcript[0].role,
            EntryRole::Peer(author) if author == "Alice"
        ));
        assert_eq!(shared.transcript[0].text, "hello from Alice");
        wait_for_remote_poll(&mut app);

        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn active_request_reconciliation_has_one_process_local_join() {
        let request = "b".repeat(40);
        let mut conversation = state("shared");
        conversation.running = true;
        conversation.active_request = Some(request.clone());
        let (mut app, _) = app_with(vec![conversation]);
        app.repo_dir = std::env::temp_dir().join(format!(
            "caos-cli-tui-missing-reconcile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        assert!(app.reconcile_active_requests());
        assert_eq!(
            app.selected().reconciling_request.as_deref(),
            Some(request.as_str())
        );
        // Polling an unchanged active snapshot cannot fan out more waiters for
        // the same exact request. Completion/failure is consumed by drain.
        assert!(!app.reconcile_active_requests());
    }

    #[test]
    fn ref_command_shows_a_copyable_canonical_ref_and_full_head() {
        let (repo, remote, _) = repo_with_default_branch("show-ref", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let id = "_shared_name_";
        let head = seed_idle_conversation(&repo, id, "Alice", "hello");

        let (mut app, _) = app_with(vec![state(id)]);
        app.repo_dir = repo.clone();
        app.show_selected_ref();
        assert!(app.selected().reference_loading);
        wait_for_reference_lookup(&mut app);

        let refname = conversation_ref(id).unwrap();
        assert!(app.selected().transcript.is_empty());
        assert_eq!(
            app.selected().reference_notice,
            Some(ReferenceNotice {
                refname: refname.clone(),
                head: head.clone(),
            })
        );

        // A coherent reload must not erase this presentation-only result.
        let transport = GitTransport::discover(&repo).unwrap();
        let load = conversation_load(&transport, id).unwrap().unwrap();
        app.selected_mut().apply_load(load.clone(), "Alice");
        assert_eq!(
            app.selected().reference_notice,
            Some(ReferenceNotice {
                refname: refname.clone(),
                head: head.clone(),
            })
        );

        // Narrow rendering clips raw text instead of markdown-parsing or
        // soft-wrapping it, while clicks return the complete underlying value.
        let area = Rect::new(0, 0, 70, 30);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("_shared_"));

        let copy_rows = (0..area.height)
            .filter_map(|row| ui::reference_copy_at(&app, area, 27, row).map(|value| (row, value)))
            .collect::<Vec<_>>();
        assert_eq!(
            copy_rows.iter().map(|(_, value)| value).collect::<Vec<_>>(),
            vec![&refname, &head]
        );
        app.palette = Some(CommandPalette::default());
        assert!(ui::reference_copy_at(&app, area, 27, copy_rows[0].0).is_none());
        assert_eq!(
            app.handle_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 27,
                    row: copy_rows[0].0,
                    modifiers: KeyModifiers::NONE,
                },
                area,
            ),
            MouseAction::Ignored
        );
        app.palette = None;
        for (row, expected) in copy_rows {
            assert_eq!(
                app.handle_mouse(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: 27,
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                    area,
                ),
                MouseAction::Copy(expected)
            );
        }

        let mut advanced = load;
        advanced.snapshot.head = "b".repeat(40);
        advanced.workspace_diff.head = advanced.snapshot.head.clone();
        app.selected_mut().apply_load(advanced, "Alice");
        assert!(app.selected().reference_notice.is_none());

        app.selected_mut().reference_notice = Some(ReferenceNotice { refname, head });
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.selected().reference_notice.is_none());
        assert_eq!(app.focus, Focus::Conversation);

        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn ctrl_n_stays_virtual_and_local_commands_do_not_submit() {
        let (repo, remote, _) = repo_with_default_branch("new-command-ref", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let transport = GitTransport::discover(&repo).unwrap();
        let (mut app, _) = app_with(vec![state("existing")]);
        app.repo_dir = repo.clone();
        app.user = "Alice".to_string();

        app.start_new_conversation(None);
        let id = app.selected().id.clone();
        assert!(app.selected().remote_head.is_none());
        assert!(conversation_head(&transport, &id).unwrap().is_none());
        assert!(
            list_user_conversations(&transport, "Alice", UserConversationStatus::Active,)
                .unwrap()
                .is_empty()
        );
        assert!(app.selected().transcript.is_empty());

        app.selected_mut()
            .composer
            .insert_str("/title Named before prompting");
        app.start_turn();
        assert!(app.selected().command_error.is_none());
        assert_eq!(app.selected().title, "Named before prompting");
        assert!(conversation_head(&transport, &id).unwrap().is_none());

        app.selected_mut().composer.insert_str("/invite Bob");
        app.start_turn();
        assert!(app.selected().command_error.is_none());
        assert!(app
            .selected()
            .transcript
            .last()
            .is_some_and(|entry| entry.text.contains("Send the first message")));
        assert!(
            list_user_conversations(&transport, "Bob", UserConversationStatus::Active)
                .unwrap()
                .is_empty()
        );
        assert!(conversation_head(&transport, &id).unwrap().is_none());

        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn failed_last_conversation_fork_keeps_a_safe_app_state() {
        let (repo, remote, tip) = repo_with_default_branch("last-fork-failure", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let mut fork = state("forked");
        fork.forking = true;
        fork.composer.insert_str("preserve this draft");
        let (mut app, _) = app_with(vec![fork]);
        app.repo_dir = repo.clone();

        app.finish_fork(
            "forked",
            "closed-origin",
            &"a".repeat(40),
            Err("remote rejected the fork".to_string()),
        );

        assert_eq!(app.conversations.len(), 1);
        assert_eq!(app.selected, 0);
        assert!(!app.selected().forking);
        assert_eq!(
            app.selected().turn_options.base.as_deref(),
            Some(tip.as_str())
        );
        assert_eq!(app.selected().composer.text, "preserve this draft");
        assert!(app.selected().remote_title.is_none());
        assert!(app.selected().remote_head.is_none());
        assert!(app
            .selected()
            .command_error
            .as_deref()
            .is_some_and(|error| error.contains("remote rejected the fork")));

        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn failed_fork_preserves_its_draft_with_other_conversations_open() {
        let (repo, remote, tip) = repo_with_default_branch("multi-fork-failure", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let mut fork = state("forked");
        fork.forking = true;
        fork.composer.insert_str("preserve this draft");
        let (mut app, _) = app_with(vec![state("origin"), fork]);
        app.repo_dir = repo.clone();
        app.selected = 1;

        app.finish_fork(
            "forked",
            "origin",
            &"a".repeat(40),
            Err("remote rejected the fork".to_string()),
        );

        assert_eq!(app.conversations.len(), 2);
        assert_eq!(app.selected().id, "forked");
        assert!(!app.selected().forking);
        assert_eq!(
            app.selected().turn_options.base.as_deref(),
            Some(tip.as_str())
        );
        assert_eq!(app.selected().composer.text, "preserve this draft");

        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn from_materializes_inherited_history_immediately() {
        let (repo, remote, _) = repo_with_default_branch("durable-fork", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let transport = GitTransport::discover(&repo).unwrap();
        let source = seed_idle_conversation(&repo, "original", "Alice", "inherited message");

        let (mut app, _) = app_with(vec![state("original")]);
        app.repo_dir = repo.clone();
        app.user = "Alice".to_string();
        app.start_new_conversation(Some(source.clone()));

        let fork_id = app.selected().id.clone();
        assert_ne!(fork_id, "original");
        assert!(app.selected().forking);
        assert!(app.selected().is_busy());
        assert!(app.selected().transcript.is_empty());
        assert!(wait_for_fork(&mut app, &fork_id));
        assert_eq!(app.selected().transcript[0].text, "inherited message");
        assert!(conversation_head(&transport, &fork_id).unwrap().is_some());
        assert_eq!(app.selected().diff.as_ref().unwrap().base_commit, source);
        assert!(app.selected().diff.as_ref().unwrap().patch.is_empty());

        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn fork_placeholder_does_not_cancel_automatic_title_generation() {
        let (repo, remote, _) = repo_with_default_branch("fork-title", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let transport = GitTransport::discover(&repo).unwrap();
        let source = seed_idle_conversation(&repo, "original", "Alice", "inherited message");
        let (mut app, _) = app_with(vec![state("original")]);
        app.repo_dir = repo.clone();
        app.user = "Alice".to_string();
        app.start_new_conversation(Some(source));
        let fork_id = app.selected().id.clone();
        assert!(wait_for_fork(&mut app, &fork_id));
        let placeholder = app.selected().title.clone();
        app.selected_mut()
            .apply_automatic_title("fallback from the first prompt");
        app.selected_mut().generating_title = true;

        wait_for_remote_poll(&mut app);
        assert_eq!(app.selected().title, "fallback from the first prompt");
        assert!(app.selected().automatic_title);
        assert_eq!(
            app.selected().remote_title.as_deref(),
            Some(placeholder.as_str())
        );

        app.finish_title_generation(0, Ok("Generated fork title".to_string()));
        assert_eq!(app.selected().title, "Generated fork title");
        assert_eq!(
            app.selected().remote_title.as_deref(),
            Some("Generated fork title")
        );
        let summary = list_user_conversations(&transport, "Alice", UserConversationStatus::Active)
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == fork_id)
            .unwrap();
        assert_eq!(summary.title, "Generated fork title");

        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn generated_fork_title_does_not_overwrite_an_external_rename() {
        let (repo, remote, _) = repo_with_default_branch("fork-title-race", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let transport = GitTransport::discover(&repo).unwrap();
        let source = seed_idle_conversation(&repo, "original", "Alice", "inherited message");
        let (mut app, _) = app_with(vec![state("original")]);
        app.repo_dir = repo.clone();
        app.user = "Alice".to_string();
        app.start_new_conversation(Some(source));
        let fork_id = app.selected().id.clone();
        assert!(wait_for_fork(&mut app, &fork_id));
        app.selected_mut().apply_automatic_title("fallback");
        app.selected_mut().generating_title = true;
        set_conversation_title(&transport, &fork_id, "External rename").unwrap();

        app.finish_title_generation(0, Ok("Late generated title".to_string()));
        assert_ne!(app.selected().title, "Late generated title");
        wait_for_remote_poll(&mut app);
        assert_eq!(app.selected().title, "External rename");

        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn plain_commit_fork_failure_never_becomes_a_markerless_conversation() {
        let (repo, remote, plain_commit) = repo_with_default_branch("plain-fork", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let transport = GitTransport::discover(&repo).unwrap();
        let (mut app, _) = app_with(vec![state("origin")]);
        app.repo_dir = repo.clone();
        app.user = "Alice".to_string();
        app.start_new_conversation(Some(plain_commit));
        let pending_id = app.selected().id.clone();
        app.selected_mut().composer.insert_str("must not submit");
        app.start_turn();
        assert!(app.selected().forking);
        assert!(app
            .selected()
            .command_error
            .as_deref()
            .unwrap()
            .contains("fork"));

        assert!(wait_for_fork(&mut app, &pending_id));
        assert_eq!(app.selected().id, pending_id);
        assert_eq!(app.selected().composer.text, "must not submit");
        assert!(app
            .selected()
            .command_error
            .as_deref()
            .unwrap()
            .contains("not a JSON event"));
        assert!(conversation_head(&transport, &pending_id)
            .unwrap()
            .is_none());

        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn selection_lock_blocks_edits_and_ctrl_q_toggles_changes() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Diff);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Chat);

        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(app.selection_locked);
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.selected().composer.text.is_empty());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.selection_locked);
    }

    #[test]
    fn ctrl_t_toggles_activity_and_ctrl_shift_t_shows_tools() {
        let mut conversation = state("talk-1");
        conversation.tool_set = Some(Ok(ToolSetDescription {
            source: "refs/caos/v2/conversations/talk-1/head:caos-tools".to_string(),
            tools: vec![caos_cli::ToolDescription {
                name: "build".to_string(),
                docs: "Build everything the tree defines.".to_string(),
                image: "docker://caos-std-bash".to_string(),
            }],
        }));
        let (mut app, _) = app_with(vec![conversation]);
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Activity);
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Chat);
        app.handle_key(KeyEvent::new(
            KeyCode::Char('T'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(app.view, View::Tools);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Always available"));
        assert!(rendered.contains("read, ls, write, edit"));
        assert!(rendered.contains("talk-1/head:caos-tools"));
        assert!(rendered.contains("build"));
        assert!(rendered.contains("Build everything the tree defines."));
        assert!(rendered.contains("[docker://caos-std-bash]"));

        app.handle_key(KeyEvent::new(
            KeyCode::Char('T'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(app.view, View::Chat);
    }
}
