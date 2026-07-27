use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use caos::chat::{
    archive_user_conversation, conversation_history, conversation_workspace_diff,
    describe_tool_set, first_available_conversation_name, list_user_conversations,
    publish_unindexed_conversations, publish_user_conversation, run_chat_turn,
    set_conversation_title, unarchive_user_conversation, ConversationRole, ToolSetDescription,
    TurnEvent, TurnOptions, UserConversationStatus, UserConversationSummary, WorkspaceDiff,
};
use caos::{GitTransport, Transport};
use ratatui_core::layout::Rect;
use ratatui_crossterm::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::args::Args;
use super::workspace::{load_conversation_workspace, publish_conversation_pr};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Chat,
    Activity,
    Diff,
    Tools,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryRole {
    Human,
    Agent,
    Notice,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranscriptEntry {
    role: EntryRole,
    commit: Option<String>,
    text: String,
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
    summary: String,
    detail: String,
    state: ActivityState,
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
    pending_pastes: Vec<PendingPaste>,
    command_selection: usize,
    command_menu_dismissed: bool,
}

impl Composer {
    fn insert_char(&mut self, ch: char) {
        self.snap_cursor_after_placeholder();
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.reset_command_menu();
    }

    fn insert_str(&mut self, text: &str) {
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
        let Some(ch) = self.text[self.cursor..].chars().next() else {
            return;
        };
        self.delete_range(self.cursor, self.cursor + ch.len_utf8());
    }

    fn move_left(&mut self) {
        if let Some((start, _)) = self
            .paste_ranges()
            .into_iter()
            .find(|(start, end)| self.cursor > *start && self.cursor <= *end)
        {
            self.cursor = start;
            return;
        }
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if let Some((_, end)) = self
            .paste_ranges()
            .into_iter()
            .find(|(start, end)| self.cursor >= *start && self.cursor < *end)
        {
            self.cursor = end;
            return;
        }
        if let Some(ch) = self.text[self.cursor..].chars().next() {
            self.cursor += ch.len_utf8();
        }
    }

    fn move_word_left(&mut self) {
        self.cursor = self.word_left();
    }

    fn move_word_right(&mut self) {
        self.cursor = self.word_right();
    }

    fn delete_word_left(&mut self) {
        let start = self.word_left();
        self.delete_range(start, self.cursor);
    }

    fn delete_word_right(&mut self) {
        let end = self.word_right();
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
        self.cursor = self.line_bounds().0;
    }

    fn move_end(&mut self) {
        self.cursor = self.line_bounds().1;
    }

    fn move_vertical(&mut self, up: bool) {
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
        self.pending_pastes
            .retain(|paste| self.text.contains(&paste.placeholder));
        self.reset_command_menu();
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
    Title,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Command {
    name: &'static str,
    usage: &'static str,
    description: &'static str,
    action: CommandAction,
}

const COMMANDS: [Command; 2] = [
    Command {
        name: "/from",
        usage: "/from <commit>",
        description: "start a conversation from a completed turn",
        action: CommandAction::From,
    },
    Command {
        name: "/title",
        usage: "/title <new title>",
        description: "rename the selected conversation",
        action: CommandAction::Title,
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
    automatic_title: bool,
    turn_options: TurnOptions,
    transcript: Vec<TranscriptEntry>,
    activities: Vec<Activity>,
    diff: Option<WorkspaceDiff>,
    tool_set: Option<Result<ToolSetDescription, String>>,
    composer: Composer,
    status: String,
    running: bool,
    publishing: bool,
    scroll_from_bottom: usize,
    transcript_selection: Option<TranscriptSelection>,
    activity_selection: Option<usize>,
    activity_detail_scroll: usize,
}

impl ConversationState {
    fn new(id: String, title: String, turn_options: TurnOptions, status: String) -> Self {
        Self {
            id,
            title,
            automatic_title: false,
            turn_options,
            transcript: Vec::new(),
            activities: Vec::new(),
            diff: None,
            tool_set: None,
            composer: Composer::default(),
            status,
            running: false,
            publishing: false,
            scroll_from_bottom: 0,
            transcript_selection: None,
            activity_selection: None,
            activity_detail_scroll: 0,
        }
    }

    fn new_virtual(id: String, title: String, turn_options: TurnOptions, status: String) -> Self {
        let mut state = Self::new(id, title, turn_options, status);
        state.automatic_title = true;
        state
    }

    fn reload(&mut self, transport: &GitTransport) {
        match conversation_history(transport, &self.id) {
            Ok(turns) => {
                self.transcript = turns
                    .into_iter()
                    .map(|turn| TranscriptEntry {
                        role: match turn.role {
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
                        self.status = format!("loading workspace changes failed: {error}");
                    }
                }
            }
            Err(error) => {
                self.transcript.clear();
                self.diff = None;
                self.status = format!("loading conversation failed: {error}");
            }
        }
        self.scroll_from_bottom = 0;
        self.transcript_selection = None;
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

    fn apply_automatic_title(&mut self, prompt: &str) {
        if self.automatic_title {
            self.title = automatic_title(prompt);
            self.automatic_title = false;
        }
    }

    fn latest_message_preview(&self) -> String {
        self.transcript
            .iter()
            .rev()
            .find(|entry| matches!(entry.role, EntryRole::Human | EntryRole::Agent))
            .map(|entry| collapse_whitespace(&entry.text))
            .unwrap_or_else(|| "New conversation".to_string())
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
    Published {
        conversation: String,
        result: Result<String, String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmAction {
    Load,
    Publish,
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
    selecting_transcript: bool,
    view: View,
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
                ConversationState::new(
                    summary.id.clone(),
                    summary.title.clone(),
                    args.turn.clone(),
                    "ready".to_string(),
                )
            })
            .collect();
        for state in &mut states {
            state.reload(&transport);
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
                    args.turn,
                    initial_status,
                ),
            );
            id
        };
        let selected = states
            .iter()
            .position(|state| state.id == selected_id)
            .expect("the selected conversation was inserted");
        Ok(Self {
            repo_dir,
            user: args.user,
            conversations: states,
            selected,
            should_quit: false,
            selection_locked: false,
            confirm_action: None,
            selecting_transcript: false,
            view: View::Chat,
            tx,
            rx,
        })
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

    pub(crate) fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub(crate) fn selection_locked(&self) -> bool {
        self.selection_locked
    }

    pub(crate) fn view(&self) -> View {
        self.view
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
                    && ui::content_contains(area, mouse.column, mouse.row) =>
            {
                self.scroll_activity_details_up(3);
                MouseAction::Redraw
            }
            MouseEventKind::ScrollDown
                if self.view == View::Activity
                    && ui::content_contains(area, mouse.column, mouse.row) =>
            {
                self.scroll_activity_details_down(3);
                MouseAction::Redraw
            }
            MouseEventKind::ScrollUp
                if self.showing_transcript()
                    && ui::content_contains(area, mouse.column, mouse.row) =>
            {
                self.selected_mut().transcript_selection = None;
                self.scroll_up(3);
                MouseAction::Redraw
            }
            MouseEventKind::ScrollDown
                if self.showing_transcript()
                    && ui::content_contains(area, mouse.column, mouse.row) =>
            {
                self.selected_mut().transcript_selection = None;
                self.scroll_down(3);
                MouseAction::Redraw
            }
            MouseEventKind::Down(MouseButton::Left) if self.showing_transcript() => {
                let Some(point) =
                    ui::transcript_point(self.selected(), area, mouse.column, mouse.row)
                else {
                    return MouseAction::Ignored;
                };
                self.selected_mut().transcript_selection = Some(TranscriptSelection {
                    anchor: point,
                    head: point,
                });
                self.selecting_transcript = true;
                MouseAction::Redraw
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
            _ => MouseAction::Ignored,
        }
    }

    fn start_turn(&mut self) {
        if self.selected().is_busy() {
            self.selected_mut().status =
                "this conversation already has an operation running".to_string();
            return;
        }
        let Some(message) = self.selected_mut().composer.take_message() else {
            return;
        };
        if let Some((command, arguments)) = parse_command(&message) {
            if arguments.is_empty() {
                self.selected_mut().status = format!("usage: {}", command.usage);
            } else {
                match command.action {
                    CommandAction::From => self.start_from_hash(arguments),
                    CommandAction::Title => self.rename_selected(arguments),
                }
            }
            return;
        }
        {
            let state = self.selected_mut();
            state.apply_automatic_title(&message);
            state.transcript.push(TranscriptEntry {
                role: EntryRole::Human,
                commit: None,
                text: message.clone(),
            });
            state.activities.clear();
            state.activity_selection = None;
            state.activity_detail_scroll = 0;
            state.running = true;
            state.status = "starting turn".to_string();
            state.scroll_from_bottom = 0;
            state.transcript_selection = None;
        }

        let tx = self.tx.clone();
        let options = self.selected().turn_options.clone();
        let conversation = self.selected().id.clone();
        let repo_dir = self.repo_dir.clone();
        std::thread::spawn(move || {
            let result = GitTransport::discover(repo_dir).and_then(|transport| {
                run_chat_turn(&transport, &options, &conversation, &message, |event| {
                    let _ = tx.send(UiMessage::Turn {
                        conversation: conversation.clone(),
                        event,
                    });
                })
                .map(|_| ())
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
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        state.running = false;
                        state.status = "turn failed".to_string();
                        state.transcript.push(TranscriptEntry {
                            role: EntryRole::Notice,
                            commit: None,
                            text: error,
                        });
                    }
                }
                UiMessage::Published {
                    conversation,
                    result,
                } => {
                    if let Some(index) = self.conversation_index(&conversation) {
                        let state = &mut self.conversations[index];
                        state.publishing = false;
                        state.status = match result {
                            Ok(url) => format!("PR ready: {url}"),
                            Err(error) => format!("PR failed: {error}"),
                        };
                    }
                }
            }
        }
        changed
    }

    fn conversation_index(&self, id: &str) -> Option<usize> {
        self.conversations.iter().position(|state| state.id == id)
    }

    fn on_turn_event(&mut self, index: usize, event: TurnEvent) {
        if let TurnEvent::Completed(outcome) = event {
            let transport = self.transport();
            let user = self.user.clone();
            let state = &mut self.conversations[index];
            state.running = false;
            state.status = format!("completed {}", outcome.short_commit);
            match transport {
                Ok(transport) => {
                    match publish_user_conversation(&transport, &user, &state.id, &state.title) {
                        Ok(()) => state.reload(&transport),
                        Err(error) => {
                            state.status =
                                format!("publishing completed conversation failed: {error}")
                        }
                    }
                }
                Err(error) => state.status = format!("reloading completed turn failed: {error}"),
            }
            return;
        }

        let state = &mut self.conversations[index];
        match event {
            TurnEvent::PhaseComplete {
                label,
                elapsed_secs,
            } => state.status = format!("{label}: {elapsed_secs:.1}s"),
            TurnEvent::Status(status) => state.status = status,
            TurnEvent::AssistantText(text) => {
                state.transcript.push(TranscriptEntry {
                    role: EntryRole::Agent,
                    commit: None,
                    text,
                });
                state.scroll_from_bottom = 0;
                state.transcript_selection = None;
            }
            TurnEvent::ToolCall {
                step_commit,
                tool_use_id,
                summary,
                ..
            } => {
                state.push_activity(Activity {
                    id: tool_use_id,
                    step_commit,
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

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if !self.selected_mut().composer.clear() {
                self.should_quit = true;
            }
            return;
        }
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
        let is_load =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l');
        let is_publish =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p');
        if !is_load && !is_publish {
            self.confirm_action = None;
        }
        if is_load {
            self.load_selected();
            return;
        }
        if is_publish {
            self.publish_selected();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            self.view = match self.view {
                View::Chat | View::Activity | View::Tools => View::Diff,
                View::Diff => View::Chat,
            };
            self.selected_mut().scroll_from_bottom = 0;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.view = match self.view {
                View::Tools => View::Chat,
                View::Chat | View::Activity | View::Diff => View::Tools,
            };
            self.selected_mut().scroll_from_bottom = 0;
            if self.view == View::Tools {
                self.load_selected_tool_set();
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
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
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('w') {
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
            if !self.selected().is_busy() {
                match self.transport() {
                    Ok(transport) => {
                        self.selected_mut().reload(&transport);
                        self.selected_mut().status = "reloaded".to_string();
                    }
                    Err(error) => self.selected_mut().status = error,
                }
            } else {
                self.selected_mut().status =
                    "finish this conversation's operation before reloading".to_string();
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
        match key.code {
            KeyCode::PageUp => self.scroll_up(8),
            KeyCode::PageDown => self.scroll_down(8),
            _ if self.view != View::Chat => {}
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
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.selected_mut().composer.insert_char('\n')
            }
            KeyCode::Enter => {
                if !self.selected_mut().composer.complete_command() {
                    self.start_turn();
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
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_mut().composer.insert_char(ch)
            }
            KeyCode::Backspace => self.selected_mut().composer.backspace(),
            KeyCode::Delete => self.selected_mut().composer.delete(),
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
            KeyCode::Home => self.selected_mut().composer.move_home(),
            KeyCode::End => self.selected_mut().composer.move_end(),
            _ => {}
        }
    }

    pub(crate) fn scroll_up(&mut self, rows: usize) {
        let state = self.selected_mut();
        state.transcript_selection = None;
        state.scroll_from_bottom = state.scroll_from_bottom.saturating_add(rows);
    }

    pub(crate) fn scroll_down(&mut self, rows: usize) {
        let state = self.selected_mut();
        state.transcript_selection = None;
        state.scroll_from_bottom = state.scroll_from_bottom.saturating_sub(rows);
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
                self.selected_mut().status = error;
                return;
            }
        };
        self.start_new_conversation(Some(commit));
    }

    fn start_new_conversation(&mut self, base: Option<String>) {
        let transport = match self.transport() {
            Ok(transport) => transport,
            Err(error) => {
                self.selected_mut().status = error;
                return;
            }
        };
        let active =
            match list_user_conversations(&transport, &self.user, UserConversationStatus::Active) {
                Ok(conversations) => conversations,
                Err(error) => {
                    self.selected_mut().status = error;
                    return;
                }
            };
        let archived =
            match list_user_conversations(&transport, &self.user, UserConversationStatus::Archived)
            {
                Ok(conversations) => conversations,
                Err(error) => {
                    self.selected_mut().status = error;
                    return;
                }
            };
        let title = first_available_conversation_name(
            active
                .iter()
                .chain(&archived)
                .map(|item| item.title.as_str())
                .chain(self.conversations.iter().map(|item| item.title.as_str())),
        );
        let id = match fresh_conversation_id(&transport, &self.user) {
            Ok(id) => id,
            Err(error) => {
                self.selected_mut().status = error;
                return;
            }
        };
        let mut options = self.selected().turn_options.clone();
        options.base = base.clone();
        let status = base
            .as_deref()
            .map(|hash| format!("ready from {}; enter a prompt", short_hash(hash)))
            .unwrap_or_else(|| "new virtual conversation; enter a prompt".to_string());
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
        self.selected = (self.selected as isize + amount).rem_euclid(len) as usize;
        self.confirm_action = None;
        if self.view == View::Tools {
            self.load_selected_tool_set();
        }
    }

    fn close_selected(&mut self) {
        if self.selected().is_busy() {
            self.selected_mut().status =
                "finish this conversation's operation before archiving it".to_string();
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
                    self.selected_mut().status = error;
                    return;
                }
            };
            Some(ConversationState::new_virtual(
                id,
                title,
                self.selected().turn_options.clone(),
                "new virtual conversation; enter a prompt".to_string(),
            ))
        } else {
            None
        };
        if self.selected().current_hash().is_some() {
            let result = self.transport().and_then(|transport| {
                archive_user_conversation(&transport, &self.user, &self.selected().id)
            });
            if let Err(error) = result {
                self.selected_mut().status = format!("archiving conversation failed: {error}");
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
            self.selected_mut().status = "conversation title cannot be empty".to_string();
            return;
        }
        if title.contains(['\n', '\r', '\t']) {
            self.selected_mut().status = "conversation title must be one line".to_string();
            return;
        }
        if self.selected().current_hash().is_some() {
            let id = self.selected().id.clone();
            if let Err(error) = self
                .transport()
                .and_then(|transport| set_conversation_title(&transport, &id, title))
            {
                self.selected_mut().status = error;
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
            self.selected_mut().status =
                "finish this conversation's operation before loading it".to_string();
        } else if self
            .selected()
            .diff
            .as_ref()
            .is_none_or(|diff| diff.patch.is_empty())
        {
            self.selected_mut().status = "there are no conversation changes to load".to_string();
        } else if self.confirm_action != Some(ConfirmAction::Load) {
            self.confirm_action = Some(ConfirmAction::Load);
            self.selected_mut().status =
                "press Ctrl+L again to load this diff into a clean working tree".to_string();
        } else {
            self.confirm_action = None;
            let diff = self
                .selected()
                .diff
                .as_ref()
                .expect("a non-empty diff was checked")
                .clone();
            self.selected_mut().status = match load_conversation_workspace(&diff, Path::new(".")) {
                Ok(()) => "conversation loaded into the working tree".to_string(),
                Err(error) => error,
            };
        }
    }

    fn publish_selected(&mut self) {
        if self.selected().is_busy() {
            self.selected_mut().status =
                "finish this conversation's operation before publishing it".to_string();
        } else if self
            .selected()
            .diff
            .as_ref()
            .is_none_or(|diff| diff.patch.is_empty())
        {
            self.selected_mut().status = "there are no conversation changes to publish".to_string();
        } else if self.confirm_action != Some(ConfirmAction::Publish) {
            self.confirm_action = Some(ConfirmAction::Publish);
            self.selected_mut().status =
                "press Ctrl+P again to push a clean branch and open a PR".to_string();
        } else {
            self.confirm_action = None;
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
                let result = publish_conversation_pr(&name, &diff);
                let _ = tx.send(UiMessage::Published {
                    conversation: name,
                    result,
                });
            });
        }
    }
}

fn fresh_conversation_id(t: &GitTransport, user: &str) -> Result<String, String> {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("reading the clock: {error}"))?
        .as_nanos();
    let descriptor = format!(
        "caos conversation v1\ncreator {user}\ncreated {created}\nprocess {}\n",
        std::process::id()
    );
    t.put_object("blob", descriptor.as_bytes())
        .map(|id| id.to_string())
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

    use super::ui::{content_contains, paragraph_scroll, render, scroll_offset};

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

    fn activity(number: usize) -> Activity {
        Activity {
            id: format!("tool-{number}"),
            step_commit: format!("{number:040x}"),
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
                view: View::Chat,
                tx: tx.clone(),
                rx,
            },
            tx,
        )
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
    fn only_new_virtual_conversations_take_their_first_prompt_as_title() {
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
    fn composer_filters_selects_completes_and_dismisses_commands() {
        let mut composer = Composer::default();
        composer.insert_str("/");
        assert_eq!(
            composer
                .command_matches()
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            ["/from", "/title"]
        );

        assert!(composer.select_command(1));
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

        assert!(parse_command("/future server convention").is_none());
        assert!(parse_command("/titlecard").is_none());
    }

    #[test]
    fn command_menu_keys_complete_select_and_dismiss() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.selected_mut().composer.insert_str("/");

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
            "--from".into(),
            "5ec3751".into(),
            "--base".into(),
            "HEAD~1".into(),
        ])
        .is_err());
        assert!(Args::parse(&[
            "--from".into(),
            "5ec3751".into(),
            "-c".into(),
            "work".into(),
        ])
        .is_err());
    }

    #[test]
    fn scroll_follows_tail_and_moves_up() {
        assert_eq!(scroll_offset(20, 10, 0), 12);
        assert_eq!(scroll_offset(20, 10, 5), 7);
        assert_eq!(scroll_offset(3, 10, 0), 0);
    }

    #[test]
    fn activity_browser_selects_and_scrolls_full_details() {
        let mut conversation = state("talk-1");
        conversation.activities = vec![activity(1), activity(2), activity(3)];
        let (mut app, _) = app_with(vec![conversation]);

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
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
        let tail = paragraph_scroll(&paragraph, area, 0);
        assert!(tail > 0);
        assert!(paragraph_scroll(&paragraph, area, 2) < tail);
    }

    #[test]
    fn transcript_uses_all_space_above_the_composer() {
        let terminal = Rect::new(0, 0, 100, 30);
        assert!(content_contains(terminal, 27, 1));
        assert!(content_contains(terminal, 99, 22));
        assert!(!content_contains(terminal, 25, 12));
        assert!(!content_contains(terminal, 27, 23));
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

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        for column in 27..=29 {
            let cell = terminal.backend().buffer().cell((column, 2)).unwrap();
            assert_eq!(cell.bg, Color::Cyan);
            assert_eq!(cell.fg, Color::Black);
        }
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
        assert!(rendered.contains("follow-up"));
        assert!(rendered.contains("cancellation is not available"));

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
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
            base: "a".repeat(40),
            head: "b".repeat(40),
            stat: "1 file changed".to_string(),
            patch: "diff --git a/a b/a".to_string(),
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(app.confirm_action, Some(ConfirmAction::Load));
        assert!(app.selected().status.contains("press Ctrl+L again"));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.confirm_action, Some(ConfirmAction::Publish));
        assert!(app.selected().status.contains("press Ctrl+P again"));
        assert!(!app.selected().publishing);
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
    fn conversation_list_renders_titles_and_latest_message_previews_without_ids() {
        let internal_id = "0123456789abcdef0123456789abcdef01234567";
        let mut selected = ConversationState::new(
            internal_id.to_string(),
            "Readable title".to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        );
        selected.transcript = vec![
            TranscriptEntry {
                role: EntryRole::Human,
                commit: None,
                text: "Latest\n  human\tmessage".to_string(),
            },
            TranscriptEntry {
                role: EntryRole::Notice,
                commit: None,
                text: "internal failure".to_string(),
            },
        ];
        assert_eq!(selected.latest_message_preview(), "Latest human message");
        let (app, _) = app_with(vec![selected, state("Empty title")]);
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
        assert!(
            sidebar_rows[title_row + 1].contains("Latest human"),
            "{:?}",
            &sidebar_rows[title_row..title_row + 3]
        );
        let empty_title_row = sidebar_rows
            .iter()
            .position(|row| row.starts_with('│') && row.contains("Empty title"))
            .unwrap();
        assert!(sidebar_rows[empty_title_row + 1].contains("New conversation"));
        let sidebar = sidebar_rows.join("\n");
        assert!(!sidebar.contains(internal_id));
        assert!(!sidebar.contains("internal failure"));
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
    fn ctrl_w_removes_virtual_conversations_and_replaces_the_last_one() {
        let (mut app, _) = app_with(vec![state("talk-1"), state("talk-2"), state("talk-3")]);
        // Replacing the last conversation mints a fresh id through the
        // transport, so point the app at a real (scratch) repo.
        app.repo_dir = throwaway_repo("ctrl-w");
        app.selected = 1;

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(
            app.conversations
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<Vec<_>>(),
            ["talk-1", "talk-3"]
        );
        assert_eq!(app.selected().id, "talk-3");

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.selected().id, "talk-1");

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.conversations.len(), 1);
        assert_eq!(app.selected().title, "talk-2");
        assert_ne!(app.selected().id, app.selected().title);
        assert!(app.selected().status.contains("new virtual conversation"));
    }

    #[test]
    fn ctrl_w_keeps_a_busy_conversation_open() {
        let mut running = state("talk-1");
        running.running = true;
        let (mut app, _) = app_with(vec![running, state("talk-2")]);

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));

        assert_eq!(app.conversations.len(), 2);
        assert_eq!(app.selected().id, "talk-1");
        assert!(app.selected().status.contains("before archiving"));
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

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

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
        let transport = GitTransport::discover(&dir).unwrap();
        conversation.reload(&transport);
        assert!(conversation.transcript.is_empty());
        assert!(conversation.diff.is_none());
        assert!(conversation.status.contains("loading conversation failed"));
        assert!(conversation.status.contains("no conversation"));

        std::fs::remove_dir_all(&dir).unwrap();
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
    fn ctrl_t_shows_the_selected_chat_tool_set() {
        let mut conversation = state("talk-1");
        conversation.tool_set = Some(Ok(ToolSetDescription {
            source: "refs/caos/conversations/talk-1:caos-tools".to_string(),
            tools: vec![caos::chat::ToolDescription {
                name: "build".to_string(),
                docs: "Build everything the tree defines.".to_string(),
                image: "/cas/std/bash".to_string(),
            }],
        }));
        let (mut app, _) = app_with(vec![conversation]);
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
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
        assert!(rendered.contains("talk-1:caos-tools"));
        assert!(rendered.contains("build"));
        assert!(rendered.contains("Build everything the tree defines."));
        assert!(rendered.contains("[/cas/std/bash]"));

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Chat);
    }
}
