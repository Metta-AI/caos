use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use caos::chat::{
    archive_user_conversation, conversation_replay, conversation_snapshot,
    conversation_workspace_diff, describe_tool_set, first_available_conversation_name,
    generate_conversation_title, list_user_conversations, publish_unindexed_conversations,
    publish_user_conversation, resume_request, run_chat_turn, set_conversation_title,
    unarchive_user_conversation, ConversationRole, ConversationSnapshot, ToolSetDescription,
    TurnEvent, TurnOptions, TurnOutcome, TurnPhase, UserConversationStatus,
    UserConversationSummary, WorkspaceDiff,
};
use caos::{GitTransport, Transport};
use ratatui_core::buffer::{Buffer, CellWidth};
use ratatui_core::layout::Rect;
use ratatui_crossterm::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::args::Args;
use super::workspace::{
    commit_working_tree, load_conversation_workspace, local_default_branch_tip,
    publish_conversation_pr, remote_default_branch,
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
    Agent,
    Info,
    Notice,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranscriptEntry {
    role: EntryRole,
    commit: Option<String>,
    text: String,
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
    id: String,
    step_commit: String,
    name: String,
    summary: String,
    detail: String,
    state: ActivityState,
}

impl Activity {
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
                tool_use_id,
                name,
                summary,
            } => activities.push(Activity {
                id: tool_use_id.clone(),
                step_commit: step_commit.clone(),
                name: name.clone(),
                summary: summary.clone(),
                detail: String::new(),
                state: ActivityState::Running,
            }),
            TurnEvent::ToolResult {
                step_commit,
                tool_use_id,
                is_error,
                content,
            } => {
                if let Some(activity) = activities
                    .iter_mut()
                    .find(|activity| activity.id == *tool_use_id)
                {
                    activity.state = if *is_error {
                        ActivityState::Failed
                    } else {
                        ActivityState::Succeeded
                    };
                    activity.detail = content.clone();
                } else {
                    activities.push(Activity {
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

    fn select_command(&mut self, amount: isize) -> bool {
        let count = self.command_matches().len();
        if count == 0 {
            return false;
        }
        self.command_selection =
            (self.command_selection as isize + amount).rem_euclid(count as isize) as usize;
        true
    }

    fn complete_command(&mut self) -> bool {
        let Some(command) = self.command_matches().get(self.command_selection).copied() else {
            return false;
        };
        let token_end = self
            .text
            .find(char::is_whitespace)
            .unwrap_or(self.text.len());
        self.text.replace_range(..token_end, command.name);
        self.cursor = command.name.len();
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
        if self.command_matches().is_empty() {
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
enum CommandAction {
    From,
    Help,
    Palette,
    Title,
    UpdateTree,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Command {
    name: &'static str,
    usage: &'static str,
    description: &'static str,
    action: CommandAction,
    takes_argument: bool,
}

const COMMANDS: [Command; 5] = [
    Command {
        name: "/from",
        usage: "/from <commit>",
        description: "start a conversation from a completed turn",
        action: CommandAction::From,
        takes_argument: true,
    },
    Command {
        name: "/help",
        usage: "/help",
        description: "show keyboard shortcuts and slash commands",
        action: CommandAction::Help,
        takes_argument: false,
    },
    Command {
        name: "/title",
        usage: "/title <new title>",
        description: "rename the selected conversation",
        action: CommandAction::Title,
        takes_argument: true,
    },
    Command {
        name: "/update-tree",
        usage: "/update-tree <message>",
        description: "fold working-tree edits into the commit",
        action: CommandAction::UpdateTree,
        takes_argument: true,
    },
    Command {
        name: "/commands",
        usage: "/commands",
        description: "open the searchable command palette",
        action: CommandAction::Palette,
        takes_argument: false,
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
    sidebar_attention: Option<String>,
    automatic_title: bool,
    automatic_title_fallback_applied: bool,
    generating_title: bool,
    turn_options: TurnOptions,
    transcript: Vec<TranscriptEntry>,
    activities: Vec<Activity>,
    diff: Option<WorkspaceDiff>,
    tool_set: Option<Result<ToolSetDescription, String>>,
    composer: Composer,
    status: String,
    command_error: Option<String>,
    publish_prompt: bool,
    running: bool,
    local_turn: bool,
    active_request: Option<String>,
    reconciling_request: Option<String>,
    reconcile_after: Option<Instant>,
    turn_phase: TurnPhase,
    publishing: bool,
    scroll: ScrollState,
    unread_below: bool,
    transcript_selection: Option<TranscriptSelection>,
    activity_selection: Option<usize>,
    activity_detail_scroll: usize,
    remote_head: Option<String>,
}

impl ConversationState {
    fn new(id: String, title: String, turn_options: TurnOptions, status: String) -> Self {
        Self {
            id,
            title,
            sidebar_attention: None,
            automatic_title: false,
            automatic_title_fallback_applied: false,
            generating_title: false,
            turn_options,
            transcript: Vec::new(),
            activities: Vec::new(),
            diff: None,
            tool_set: None,
            composer: Composer::default(),
            status,
            command_error: None,
            publish_prompt: false,
            running: false,
            local_turn: false,
            active_request: None,
            reconciling_request: None,
            reconcile_after: None,
            turn_phase: TurnPhase::System,
            publishing: false,
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
        state
    }

    fn reload(&mut self, transport: &GitTransport, current_user: &str) {
        match conversation_replay(transport, &self.id) {
            Ok(replay) => {
                self.activities = replay
                    .turn_events
                    .last()
                    .map(|turn| replayed_activities(&turn.events))
                    .unwrap_or_default();
                self.activity_selection = self.activities.len().checked_sub(1);
                self.activity_detail_scroll = 0;
                self.transcript = replay
                    .turns
                    .into_iter()
                    .map(|turn| TranscriptEntry {
                        role: match turn.role {
                            ConversationRole::Human if turn.author != current_user => {
                                EntryRole::Peer(turn.author)
                            }
                            ConversationRole::Human => EntryRole::Human,
                            ConversationRole::Agent => EntryRole::Agent,
                        },
                        commit: Some(turn.commit),
                        text: turn.message,
                    })
                    .collect();
                match conversation_workspace_diff(transport, &self.id) {
                    Ok(diff) => self.diff = Some(diff),
                    Err(error) => {
                        self.diff = None;
                        self.push_error(format!("loading workspace changes failed: {error}"));
                    }
                }
            }
            Err(error) => {
                self.transcript.clear();
                self.activities.clear();
                self.activity_selection = None;
                self.activity_detail_scroll = 0;
                self.diff = None;
                self.push_error(format!("loading conversation failed: {error}"));
            }
        }
        self.transcript_selection = None;
    }

    fn apply_snapshot(&mut self, snapshot: &ConversationSnapshot) {
        self.running = matches!(snapshot.status.as_str(), "queued" | "running");
        self.active_request = self.running.then(|| snapshot.request.clone()).flatten();
        self.status = match snapshot.status.as_str() {
            "queued" => "queued".to_string(),
            "running" => "agent running".to_string(),
            "idle" => format!("updated {}", short_hash(&snapshot.head)),
            other => other.to_string(),
        };
        if !self.running {
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
        self.running || self.publishing
    }

    fn push_error(&mut self, error: impl Into<String>) {
        self.note_transcript_append();
        self.status.clear();
        self.transcript.push(TranscriptEntry {
            role: EntryRole::Notice,
            commit: None,
            text: error.into(),
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
        });
        self.transcript_selection = None;
    }

    fn show_command_error(&mut self, error: impl Into<String>) {
        self.command_error = Some(error.into());
        self.status.clear();
        self.transcript_selection = None;
    }

    fn note_transcript_append(&mut self) {
        if self.scroll.offset.is_some() {
            self.unread_below = true;
        }
    }

    fn follow_tail(&mut self) {
        self.scroll.follow_tail();
        self.unread_below = false;
    }

    fn apply_automatic_title(&mut self, prompt: &str) {
        if self.automatic_title && !self.automatic_title_fallback_applied {
            self.title = automatic_title(prompt);
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
        } else if self.publishing {
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
    Turn {
        conversation: String,
        event: TurnEvent,
    },
    Failed {
        conversation: String,
        error: String,
    },
    Completed {
        conversation: String,
        outcome: TurnOutcome,
    },
    TitleGenerated {
        conversation: String,
        result: Result<String, String>,
    },
    Published {
        conversation: String,
        result: Result<String, String>,
    },
    Reconciled {
        conversation: String,
        request: String,
        result: Result<(), String>,
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
enum PaletteAction {
    NewConversation,
    Checkout,
    Publish,
    Activity,
    Changes,
    Tools,
    Reload,
    Help,
    Archive,
    SelectionLock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaletteCommand {
    label: &'static str,
    shortcut: &'static str,
    keywords: &'static str,
    action: PaletteAction,
}

const PALETTE_COMMANDS: [PaletteCommand; 10] = [
    PaletteCommand {
        label: "New conversation",
        shortcut: "Ctrl+N",
        keywords: "create start chat",
        action: PaletteAction::NewConversation,
    },
    PaletteCommand {
        label: "Check out conversation",
        shortcut: "Ctrl+L",
        keywords: "load workspace git",
        action: PaletteAction::Checkout,
    },
    PaletteCommand {
        label: "Publish pull request",
        shortcut: "Ctrl+P twice",
        keywords: "push pr github branch",
        action: PaletteAction::Publish,
    },
    PaletteCommand {
        label: "Show activity",
        shortcut: "Ctrl+T",
        keywords: "tools progress browser",
        action: PaletteAction::Activity,
    },
    PaletteCommand {
        label: "Show workspace changes",
        shortcut: "Ctrl+Q",
        keywords: "diff files",
        action: PaletteAction::Changes,
    },
    PaletteCommand {
        label: "Show available tools",
        shortcut: "Ctrl+Shift+T",
        keywords: "commands agent",
        action: PaletteAction::Tools,
    },
    PaletteCommand {
        label: "Reload conversation",
        shortcut: "Ctrl+R",
        keywords: "refresh history",
        action: PaletteAction::Reload,
    },
    PaletteCommand {
        label: "Show keyboard help",
        shortcut: "Ctrl+H",
        keywords: "shortcuts documentation",
        action: PaletteAction::Help,
    },
    PaletteCommand {
        label: "Archive conversation",
        shortcut: "Ctrl+E in list",
        keywords: "close remove",
        action: PaletteAction::Archive,
    },
    PaletteCommand {
        label: "Toggle native selection lock",
        shortcut: "Ctrl+Y",
        keywords: "copy mouse terminal freeze",
        action: PaletteAction::SelectionLock,
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

    fn selected_action(&self) -> Option<PaletteAction> {
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
        if let Some(from) = args.from_commit.clone() {
            let commit = transport
                .resolve_revspec(&from)?
                .ok_or_else(|| format!("cannot resolve --from {from:?}"))?
                .to_string();
            args.from_commit = Some(commit.clone());
            args.turn.base = Some(commit);
        }
        publish_unindexed_conversations(&transport, &args.user)?;
        let mut conversations =
            list_user_conversations(&transport, &args.user, UserConversationStatus::Active)?;
        if let Some(requested) = args.conversation.as_deref() {
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
                    unarchive_user_conversation(&transport, &args.user, requested)?;
                    conversations = list_user_conversations(
                        &transport,
                        &args.user,
                        UserConversationStatus::Active,
                    )?;
                }
            }
        }
        let selected_name = choose_conversation(
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
                state.remote_head = Some(summary.head.clone());
                state
            })
            .collect();
        for state in &mut states {
            state.reload(&transport, &args.user);
            if let Some(snapshot) = conversation_snapshot(&transport, &state.id)? {
                state.apply_snapshot(&snapshot);
            }
        }
        let selected_id = if states.iter().any(|state| state.id == selected_name) {
            selected_name.clone()
        } else {
            let id = if args.conversation.is_some() {
                selected_name.clone()
            } else {
                fresh_conversation_id(&transport, &args.user)?
            };
            states.insert(
                0,
                ConversationState::new_virtual(
                    id.clone(),
                    selected_name.clone(),
                    new_conversation_options(args.turn.clone(), args.turn.base, &repo_dir)?.0,
                    initial_status,
                ),
            );
            id
        };
        let selected = states
            .iter()
            .position(|state| state.id == selected_id)
            .expect("the selected conversation was inserted");
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
                if let Some(point) =
                    ui::transcript_point(self.selected(), area, mouse.column, mouse.row)
                {
                    self.selected_mut()
                        .transcript_selection
                        .as_mut()
                        .expect("dragging starts with a transcript selection")
                        .head = point;
                    MouseAction::Redraw
                } else {
                    MouseAction::Ignored
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.selecting_transcript => {
                if let Some(point) =
                    ui::transcript_point(self.selected(), area, mouse.column, mouse.row)
                {
                    self.selected_mut()
                        .transcript_selection
                        .as_mut()
                        .expect("dragging starts with a transcript selection")
                        .head = point;
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
        if self.selected().is_busy() {
            self.selected_mut()
                .show_command_error("this conversation already has an operation running");
            return;
        }
        let Some(raw) = self.selected_mut().composer.take_message() else {
            return;
        };
        // Resolve the prompt into the turn's message and, for `/update-tree`,
        // the tree the human commit should carry. `/from`, `/help`, and
        // `/title` are not turns and return here; everything else falls
        // through to run one.
        let mut human_tree = None;
        let message = if let Some((command, arguments)) = parse_command(&raw) {
            if command.takes_argument && arguments.is_empty() {
                self.selected_mut()
                    .show_command_error(format!("usage: {}", command.usage));
                return;
            }
            match command.action {
                CommandAction::Help => {
                    if arguments.is_empty() {
                        self.view = View::Help;
                    } else {
                        self.selected_mut().status = format!("usage: {}", command.usage);
                    }
                    return;
                }
                CommandAction::Palette => {
                    if arguments.is_empty() {
                        self.palette = Some(CommandPalette::default());
                    } else {
                        self.selected_mut().status = format!("usage: {}", command.usage);
                    }
                    return;
                }
                CommandAction::From => {
                    self.start_from_hash(arguments);
                    return;
                }
                CommandAction::Title => {
                    self.rename_selected(arguments);
                    return;
                }
                CommandAction::UpdateTree => {
                    match commit_working_tree(arguments, &self.repo_dir) {
                        Ok(tree) => human_tree = Some(tree),
                        Err(error) => {
                            self.selected_mut().show_command_error(error);
                            return;
                        }
                    }
                    arguments.to_string()
                }
            }
        } else {
            raw
        };
        let should_generate_title =
            self.selected().automatic_title && !self.selected().generating_title;
        {
            let state = self.selected_mut();
            state.apply_automatic_title(&message);
            if should_generate_title {
                state.generating_title = true;
            }
            state.transcript.push(TranscriptEntry {
                role: EntryRole::Human,
                commit: None,
                text: message.clone(),
            });
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
        std::thread::spawn(move || {
            let result = GitTransport::discover(repo_dir).and_then(|transport| {
                let outcome = run_chat_turn(
                    &transport,
                    &options,
                    &conversation,
                    &message,
                    human_tree.as_deref(),
                    |event| {
                        if matches!(event, TurnEvent::Completed(_)) {
                            return;
                        }
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
                    error,
                });
            }
        });
    }

    pub(crate) fn drain_messages(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.rx.try_recv() {
            changed = true;
            match message {
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
                    error,
                } => {
                    let refresh = self.transport().and_then(|transport| {
                        let snapshot = conversation_snapshot(&transport, &conversation)?;
                        Ok((transport, snapshot))
                    });
                    let user = self.user.clone();
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        state.local_turn = false;
                        if let Ok((transport, snapshot)) = refresh {
                            state.reload(&transport, &user);
                            if let Some(snapshot) = snapshot {
                                state.apply_snapshot(&snapshot);
                            } else {
                                state.running = false;
                            }
                        } else {
                            state.running = false;
                        }
                        state.status = "turn failed".to_string();
                        state.sidebar_attention = Some("Failed — open for details".to_string());
                        state.push_error(error);
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
            }
        }
        changed |= self.reconcile_active_requests();
        changed
    }

    /// Follow canonical remote refs so other TUI clients and detached workers
    /// become visible without any process-local notification channel.
    pub(crate) fn poll_remote(&mut self) -> bool {
        let Ok(transport) = self.transport() else {
            return false;
        };
        if publish_unindexed_conversations(&transport, &self.user).is_err() {
            return false;
        }
        let Ok(summaries) =
            list_user_conversations(&transport, &self.user, UserConversationStatus::Active)
        else {
            return false;
        };
        let mut changed = false;
        for summary in summaries {
            if let Some(index) = self.conversation_index(&summary.id) {
                let state = &mut self.conversations[index];
                if state.title != summary.title {
                    state.title = summary.title.clone();
                    state.automatic_title = false;
                    changed = true;
                }
                if state.remote_head.as_deref() == Some(&summary.head) {
                    continue;
                }
                state.reload(&transport, &self.user);
                state.remote_head = Some(summary.head.clone());
                if let Ok(Some(snapshot)) = conversation_snapshot(&transport, &summary.id) {
                    state.apply_snapshot(&snapshot);
                }
                changed = true;
            } else {
                let mut state = ConversationState::new(
                    summary.id.clone(),
                    summary.title,
                    self.selected().turn_options.clone(),
                    "shared conversation".to_string(),
                );
                state.reload(&transport, &self.user);
                state.remote_head = Some(summary.head);
                if let Ok(Some(snapshot)) = conversation_snapshot(&transport, &summary.id) {
                    state.apply_snapshot(&snapshot);
                }
                self.conversations.insert(0, state);
                self.selected += 1;
                changed = true;
            }
        }
        changed | self.reconcile_active_requests()
    }

    fn conversation_index(&self, id: &str) -> Option<usize> {
        self.conversations.iter().position(|state| state.id == id)
    }

    fn on_turn_event(&mut self, index: usize, event: TurnEvent) {
        if let TurnEvent::Completed(outcome) = event {
            self.finish_turn(index, outcome);
            return;
        }

        let state = &mut self.conversations[index];
        match event {
            TurnEvent::PhaseStarted(phase) => state.turn_phase = phase,
            TurnEvent::PhaseComplete {
                label,
                elapsed_secs,
            } => state.status = format!("{label}: {elapsed_secs:.1}s"),
            TurnEvent::Status(status) => state.status = status,
            TurnEvent::AssistantText(text) => {
                state.note_transcript_append();
                state.transcript.push(TranscriptEntry {
                    role: EntryRole::Agent,
                    commit: None,
                    text,
                });
                state.transcript_selection = None;
            }
            TurnEvent::ToolCall {
                step_commit,
                tool_use_id,
                name,
                summary,
            } => {
                state.push_activity(Activity {
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
                tool_use_id,
                is_error,
                content,
            } => {
                if let Some(activity) = state
                    .activities
                    .iter_mut()
                    .find(|activity| activity.id == tool_use_id)
                {
                    activity.state = if is_error {
                        ActivityState::Failed
                    } else {
                        ActivityState::Succeeded
                    };
                    activity.detail = content;
                } else {
                    state.push_activity(Activity {
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
            TurnEvent::Completed(_) => unreachable!("completed events return above"),
        }
    }

    fn finish_turn(&mut self, index: usize, outcome: TurnOutcome) {
        let transport = self.transport();
        let user = self.user.clone();
        let state = &mut self.conversations[index];
        state.running = false;
        state.local_turn = false;
        state.active_request = None;
        state.reconciling_request = None;
        state.reconcile_after = None;
        state.sidebar_attention = None;
        state.status = format!("completed {}", outcome.short_commit);
        match transport {
            Ok(transport) => {
                match publish_user_conversation(&transport, &user, &state.id, &state.title) {
                    Ok(()) => {
                        state.reload(&transport, &user);
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

    fn finish_title_generation(&mut self, index: usize, result: Result<String, String>) {
        let transport = self.transport();
        let state = &mut self.conversations[index];
        state.generating_title = false;
        if !state.automatic_title {
            return;
        }
        state.automatic_title = false;
        let title = match result {
            Ok(title) => title,
            Err(_) => return,
        };
        if state.current_hash().is_some() {
            let Ok(transport) = transport else {
                return;
            };
            if set_conversation_title(&transport, &state.id, &title).is_err() {
                return;
            }
        }
        state.title = title;
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        self.selected_mut().command_error = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
            self.selection_locked = !self.selection_locked;
            return;
        }
        if self.selection_locked {
            if key.code == KeyCode::Esc {
                self.selection_locked = false;
            }
            return;
        }
        let is_palette = key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('p' | 'P'));
        if is_palette {
            self.confirm_action = None;
            self.selected_mut().publish_prompt = false;
            self.palette = self.palette.take().is_none().then(CommandPalette::default);
            return;
        }
        if self.palette.is_some() {
            self.handle_palette_key(key);
            return;
        }
        let is_load =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l');
        let is_publish =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p');
        if matches!(self.confirm_action, Some(ConfirmAction::Publish { .. })) {
            if is_publish {
                self.publish_selected();
            } else {
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
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if !self.selected_mut().composer.clear() {
                self.should_quit = true;
            }
            return;
        }
        // Ctrl+H is a distinct control byte in legacy terminal input. Keep
        // Ctrl+? as an alias for terminals whose enhanced keyboard protocol
        // reports the modifiers unambiguously.
        let is_help = key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('h' | '?' | '/'));
        if is_load {
            self.load_selected();
            return;
        }
        if is_publish {
            self.publish_selected();
            return;
        }
        if is_help {
            self.view = if self.view == View::Help {
                View::Chat
            } else {
                View::Help
            };
            self.selected_mut().follow_tail();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            self.view = match self.view {
                View::Chat | View::Activity | View::Tools | View::Help => View::Diff,
                View::Diff => View::Chat,
            };
            self.selected_mut().follow_tail();
            return;
        }
        let ctrl_t = key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'));
        if ctrl_t && key.modifiers.contains(KeyModifiers::SHIFT) {
            self.view = match self.view {
                View::Tools => View::Chat,
                View::Chat | View::Activity | View::Diff | View::Help => View::Tools,
            };
            self.selected_mut().follow_tail();
            if self.view == View::Tools {
                self.load_selected_tool_set();
            }
            return;
        }
        if ctrl_t {
            self.view = if self.view == View::Activity {
                View::Chat
            } else {
                self.selected_mut().ensure_activity_selection();
                View::Activity
            };
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            self.start_new_conversation(None);
            self.focus = Focus::Conversation;
            return;
        }
        if self.focus == Focus::List
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('e')
        {
            self.close_selected();
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.reload_selected();
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
                if !self.selected_mut().composer.dismiss_command_menu() {
                    self.focus = Focus::List;
                }
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
                    self.execute_palette_action(action);
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

    fn execute_palette_action(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::NewConversation => self.start_new_conversation(None),
            PaletteAction::Checkout => self.load_selected(),
            PaletteAction::Publish => self.publish_selected(),
            PaletteAction::Activity => {
                self.selected_mut().ensure_activity_selection();
                self.view = View::Activity;
            }
            PaletteAction::Changes => {
                self.view = View::Diff;
                self.selected_mut().follow_tail();
            }
            PaletteAction::Tools => {
                self.view = View::Tools;
                self.selected_mut().follow_tail();
                self.load_selected_tool_set();
            }
            PaletteAction::Reload => self.reload_selected(),
            PaletteAction::Help => {
                self.view = View::Help;
                self.selected_mut().follow_tail();
            }
            PaletteAction::Archive => self.close_selected(),
            PaletteAction::SelectionLock => self.selection_locked = true,
        }
    }

    fn reload_selected(&mut self) {
        if !self.selected().is_busy() {
            match self.transport() {
                Ok(transport) => {
                    let user = self.user.clone();
                    self.selected_mut().reload(&transport, &user);
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
        let (options, base) = match new_conversation_options(
            self.selected().turn_options.clone(),
            base,
            &self.repo_dir,
        ) {
            Ok(result) => result,
            Err(error) => {
                self.selected_mut().show_command_error(error);
                return;
            }
        };
        let status = format!("ready from {}; enter a prompt", short_hash(&base));
        self.conversations.insert(
            0,
            ConversationState::new_virtual(id, title, options, status),
        );
        self.selected = 0;
        self.view = View::Chat;
        self.confirm_action = None;
    }

    fn select_relative(&mut self, amount: isize) {
        let len = self.conversations.len() as isize;
        self.select((self.selected as isize + amount).rem_euclid(len) as usize);
    }

    fn select(&mut self, index: usize) {
        self.selected_mut().publish_prompt = false;
        self.selected = index;
        self.confirm_action = None;
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
        if self.selected().current_hash().is_some() {
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
        state.automatic_title = false;
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
            let (default_base, pr_base) = match self.confirm_action.take() {
                Some(ConfirmAction::Publish {
                    default_base,
                    base_input,
                }) => {
                    let base_input = base_input.trim();
                    if base_input.is_empty() {
                        (default_base.clone(), default_base)
                    } else {
                        (default_base, base_input.to_string())
                    }
                }
                None => unreachable!("publication was confirmed"),
            };
            self.selected_mut().publish_prompt = false;
            let name = self.selected().id.clone();
            let diff = self
                .selected()
                .diff
                .clone()
                .expect("a non-empty diff was checked");
            self.selected_mut().publishing = true;
            self.selected_mut().status = "publishing a clean conversation branch".to_string();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = publish_conversation_pr(&name, &diff, &pr_base, &default_base);
                let _ = tx.send(UiMessage::Published {
                    conversation: name,
                    result,
                });
            });
        }
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
        "caos conversation v2\ncreator {user}\ncreated {created}\nprocess {}\n",
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

fn choose_conversation(
    requested: Option<&str>,
    new: bool,
    conversations: &[UserConversationSummary],
) -> Result<String, String> {
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
        return Ok(requested.to_string());
    }
    if !new {
        if let Some(latest) = conversations.first() {
            return Ok(latest.id.clone());
        }
    }
    Ok(first_available_conversation_name(
        conversations
            .iter()
            .map(|conversation| conversation.title.as_str()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_core::backend::TestBackend;
    use ratatui_core::layout::Rect;
    use ratatui_core::style::{Color, Modifier};
    use ratatui_core::terminal::Terminal;
    use ratatui_widgets::paragraph::{Paragraph, Wrap};

    use super::ui::{
        content_contains, paragraph_scroll, render, scroll_offset, transcript_contains,
    };

    fn summary(id: &str) -> UserConversationSummary {
        UserConversationSummary {
            id: id.to_string(),
            title: id.to_string(),
            head: "a".repeat(40),
            updated_unix: 1,
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
                view: View::Chat,
                focus: Focus::Conversation,
                tx: tx.clone(),
                rx,
                palette: None,
            },
            tx,
        )
    }

    fn rendered_main_pane(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().skip(26).map(|cell| cell.symbol()).collect())
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
    fn composer_filters_selects_completes_and_dismisses_commands() {
        let mut composer = Composer::default();
        composer.insert_str("/");
        assert_eq!(
            composer
                .command_matches()
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            ["/from", "/help", "/title", "/update-tree", "/commands"]
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
    }

    #[test]
    fn command_parser_only_claims_catalog_commands() {
        let (command, arguments) = parse_command("/title A useful title").unwrap();
        assert_eq!(command.action, CommandAction::Title);
        assert_eq!(arguments, "A useful title");

        let (command, arguments) = parse_command("/from\nabc123").unwrap();
        assert_eq!(command.action, CommandAction::From);
        assert_eq!(arguments, "abc123");

        let (command, arguments) = parse_command("/update-tree include this text").unwrap();
        assert_eq!(command.action, CommandAction::UpdateTree);
        assert_eq!(arguments, "include this text");
        assert!(command.takes_argument);

        let (command, arguments) = parse_command("/help").unwrap();
        assert_eq!(command.action, CommandAction::Help);
        assert_eq!(arguments, "");

        let (command, arguments) = parse_command("/commands").unwrap();
        assert_eq!(command.action, CommandAction::Palette);
        assert_eq!(arguments, "");

        assert!(parse_command("/future server convention").is_none());
        assert!(parse_command("/titlecard").is_none());
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
        assert_eq!(matches[0].action, PaletteAction::Changes);

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
        assert_eq!(palette.matches()[0].action, PaletteAction::Publish);

        palette.query.clear();
        palette.select(-1);
        assert_eq!(palette.selected, PALETTE_COMMANDS.len() - 1);
        assert_eq!(
            palette.selected_action(),
            Some(PaletteAction::SelectionLock)
        );
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
    fn list_focus_navigates_conversations_and_enter_opens_the_conversation() {
        let (mut app, _) = app_with(vec![state("talk-1"), state("talk-2"), state("talk-3")]);
        assert_eq!(app.focus(), Focus::Conversation);

        // Esc with an empty command menu moves focus to the conversation list.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.focus(), Focus::List);

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
    fn conversation_selection_is_sticky_or_fresh() {
        let conversations = vec![summary("recent"), summary("talk-1")];
        assert_eq!(
            choose_conversation(None, false, &conversations).unwrap(),
            "recent"
        );
        assert_eq!(
            choose_conversation(None, true, &conversations).unwrap(),
            "talk-2"
        );
        assert!(choose_conversation(Some("recent"), true, &conversations).is_err());
        assert_eq!(
            choose_conversation(Some("named"), false, &conversations).unwrap(),
            "named"
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
            app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 20), area),
            MouseAction::Redraw
        );
        let copied = app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 20), area);

        assert!(
            matches!(copied, MouseAction::Copy(ref text) if text.contains("caos") && text.contains("copy-anywhere"))
        );
    }

    #[test]
    fn cli_options_match_the_line_client_surface() {
        // --user rides along so the test never depends on ambient $USER
        // (the cargo worker's environment has none).
        let args = Args::parse(&[
            "--user".into(),
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
            "--user".into(),
            "tester".into(),
            "--from".into(),
            "5ec3751".into(),
            "--base".into(),
            "HEAD~1".into(),
        ])
        .is_err());
        assert!(Args::parse(&[
            "--user".into(),
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

        app.on_turn_event(0, TurnEvent::AssistantText("new response".to_string()));

        assert_eq!(scroll_offset(40, 10, &app.selected().scroll), 7);
        assert!(app.selected().unread_below);
        app.scroll_down(usize::MAX);
        assert!(!app.selected().unread_below);
    }

    #[test]
    fn paused_transcript_shows_unread_and_rendered_lines_below() {
        let mut conversation = state("talk-1");
        conversation.transcript.push(TranscriptEntry {
            role: EntryRole::Agent,
            commit: None,
            text: (0..60)
                .map(|line| format!("existing line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        let (mut app, _) = app_with(vec![conversation]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        app.scroll_up(8);

        app.on_turn_event(0, TurnEvent::AssistantText("new response".to_string()));
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
        activity.summary = "read crates/caos/src/chat.rs".to_string();
        activity.state = ActivityState::Running;

        assert_eq!(activity.running_verb(), "Reading");
        assert_eq!(activity.running_summary(), "crates/caos/src/chat.rs");

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
        let activities = replayed_activities(&[
            TurnEvent::ToolCall {
                step_commit: "1".repeat(40),
                tool_use_id: "tool-1".to_string(),
                name: "read".to_string(),
                summary: "read README.md".to_string(),
            },
            TurnEvent::ToolResult {
                step_commit: "2".repeat(40),
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
    fn new_activity_follows_only_a_selection_at_the_tail() {
        let mut conversation = state("talk-1");
        conversation.activities = vec![activity(1), activity(2)];
        conversation.activity_selection = Some(0);
        let (mut app, _) = app_with(vec![conversation]);

        app.on_turn_event(
            0,
            TurnEvent::ToolCall {
                step_commit: "3".repeat(40),
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
            role: EntryRole::Agent,
            commit: None,
            text: (0..60)
                .map(|line| format!("line {line:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
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
    fn full_layout_renders_chat_activity_and_prompt() {
        let mut selected = state("review-api");
        selected.transcript = vec![
            TranscriptEntry {
                role: EntryRole::Human,
                commit: Some("a".repeat(40)),
                text: "Please run the tests".to_string(),
            },
            TranscriptEntry {
                role: EntryRole::Agent,
                commit: Some("b".repeat(40)),
                text: "Running them now.".to_string(),
            },
        ];
        selected.activities = vec![Activity {
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
            role: EntryRole::Agent,
            commit: Some("b".repeat(40)),
            text: "done".to_string(),
        });
        let (app, _) = app_with(vec![conversation]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(&app, frame)).unwrap();

        let header = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .take(100)
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(header.contains("caos"));
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
            role: EntryRole::Agent,
            commit: None,
            text: "plain **bold** and _italic_".to_string(),
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
        let mut generating_title = ConversationState::new(
            internal_id.to_string(),
            "Existing title".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        generating_title.generating_title = true;
        let (app, _) = app_with(vec![selected, generating_title, state("Empty title")]);
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
    fn rejected_prompt_uses_the_command_panel_instead_of_a_chat_entry() {
        let mut conversation = state("talk-1");
        conversation.running = true;
        conversation.composer.insert_str("another prompt");
        let (mut app, _) = app_with(vec![conversation]);

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert!(app.selected().transcript.is_empty());
        assert_eq!(
            app.selected().command_error.as_deref(),
            Some("this conversation already has an operation running")
        );
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
        assert!(rendered.contains("Command error"));
        assert!(rendered.contains("this conversation already has an operation running"));
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
        // the composer is ready for the first prompt, regardless of whether
        // minting the conversation reaches a remote.
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
        conversation.reload(&transport, "alice");
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
    fn remote_poll_discovers_shared_conversations_and_names_the_other_user() {
        let (repo, remote, _) = repo_with_default_branch("remote-poll", "main");
        git_ok(&repo, &["remote", "add", "caos", remote.to_str().unwrap()]);
        let base = git_output(&repo, &["rev-parse", "HEAD"]);
        let tree = git_output(&repo, &["rev-parse", "HEAD^{tree}"]);
        let user = git_output(
            &repo,
            &[
                "commit-tree",
                &tree,
                "-p",
                &base,
                "-m",
                r#"{"v":2,"author":"user","username":"Alice","content":"hello from Alice"}"#,
            ],
        );
        let request = "b".repeat(40);
        let admission_message =
            format!(r#"{{"v":2,"status":"queued","request":"{request}","request_head":"{user}"}}"#);
        let admitted = git_output(
            &repo,
            &["commit-tree", &tree, "-p", &user, "-m", &admission_message],
        );
        git_ok(
            &repo,
            &[
                "push",
                "-q",
                "caos",
                &format!("{admitted}:refs/caos/v2/conversations/shared/head"),
            ],
        );

        let (mut app, _) = app_with(vec![state("local")]);
        app.repo_dir = repo.clone();
        app.user = "Bob".to_string();
        assert!(app.poll_remote());

        let shared = app
            .conversations
            .iter()
            .find(|conversation| conversation.id == "shared")
            .unwrap();
        assert!(shared.running);
        assert!(matches!(
            &shared.transcript[0].role,
            EntryRole::Peer(author) if author == "Alice"
        ));
        assert_eq!(shared.transcript[0].text, "hello from Alice");
        assert!(!app.poll_remote());

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
            source: "refs/caos/v2/conversations/talk-1/from-user:caos-tools".to_string(),
            tools: vec![caos::chat::ToolDescription {
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
        assert!(rendered.contains("talk-1/from-user:caos-tools"));
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
