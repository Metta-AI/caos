use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use caos::chat::{
    conversation_history, conversation_workspace_diff, first_available_conversation_name,
    list_conversations, run_chat_turn, ConversationRole, ConversationSummary, TurnEvent,
    TurnOptions, WorkspaceDiff,
};
use caos::{GitTransport, Transport};
use ratatui_crossterm::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::args::Args;
use crate::workspace::{load_conversation_workspace, publish_conversation_pr};

#[path = "ui.rs"]
pub(crate) mod ui;

fn short_hash(hash: &str) -> &str {
    hash.get(..7).unwrap_or(hash)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Chat,
    Diff,
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Composer {
    text: String,
    cursor: usize,
}

impl Composer {
    fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn insert_str(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
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
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
    }

    fn delete(&mut self) {
        let Some(ch) = self.text[self.cursor..].chars().next() else {
            return;
        };
        self.text.drain(self.cursor..self.cursor + ch.len_utf8());
    }

    fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if let Some(ch) = self.text[self.cursor..].chars().next() {
            self.cursor += ch.len_utf8();
        }
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
        let message = self.text.trim().to_string();
        if message.is_empty() {
            return None;
        }
        self.text.clear();
        self.cursor = 0;
        Some(message)
    }
}

fn byte_at_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map(|(offset, _)| start + offset)
        .unwrap_or(end)
}

struct ConversationState {
    name: String,
    turn_options: TurnOptions,
    transcript: Vec<TranscriptEntry>,
    activities: Vec<Activity>,
    diff: Option<WorkspaceDiff>,
    composer: Composer,
    status: String,
    running: bool,
    publishing: bool,
    scroll_from_bottom: usize,
}

impl ConversationState {
    fn new(name: String, turn_options: TurnOptions, status: String) -> Self {
        Self {
            name,
            turn_options,
            transcript: Vec::new(),
            activities: Vec::new(),
            diff: None,
            composer: Composer::default(),
            status,
            running: false,
            publishing: false,
            scroll_from_bottom: 0,
        }
    }

    fn reload(&mut self, transport: &GitTransport) {
        match conversation_history(transport, &self.name) {
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
                match conversation_workspace_diff(transport, &self.name) {
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

pub(crate) struct App {
    repo_dir: PathBuf,
    conversations: Vec<ConversationState>,
    selected: usize,
    should_quit: bool,
    copy_mode: bool,
    confirm_action: Option<ConfirmAction>,
    activity_expanded: bool,
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
        let conversations = list_conversations(&transport)?;
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
                ConversationState::new(summary.name.clone(), args.turn.clone(), "ready".to_string())
            })
            .collect();
        for state in &mut states {
            state.reload(&transport);
        }
        if states.iter().all(|state| state.name != selected_name) {
            states.insert(
                0,
                ConversationState::new(selected_name.clone(), args.turn, initial_status),
            );
        }
        let selected = states
            .iter()
            .position(|state| state.name == selected_name)
            .expect("the selected conversation was inserted");
        Ok(Self {
            repo_dir,
            conversations: states,
            selected,
            should_quit: false,
            copy_mode: false,
            confirm_action: None,
            activity_expanded: false,
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

    pub(crate) fn copy_mode(&self) -> bool {
        self.copy_mode
    }

    pub(crate) fn view(&self) -> View {
        self.view
    }

    pub(crate) fn insert_paste(&mut self, text: &str) {
        self.selected_mut().composer.insert_str(text);
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
        if let Some(hash) = message.strip_prefix("/from ").map(str::trim) {
            self.start_from_hash(hash);
            return;
        }
        if message == "/from" {
            self.selected_mut().status = "usage: /from <commit>".to_string();
            return;
        }
        {
            let state = self.selected_mut();
            state.transcript.push(TranscriptEntry {
                role: EntryRole::Human,
                commit: None,
                text: message.clone(),
            });
            state.activities.clear();
            state.running = true;
            state.status = "starting turn".to_string();
            state.scroll_from_bottom = 0;
        }

        let tx = self.tx.clone();
        let options = self.selected().turn_options.clone();
        let conversation = self.selected().name.clone();
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

    fn conversation_index(&self, name: &str) -> Option<usize> {
        self.conversations
            .iter()
            .position(|state| state.name == name)
    }

    fn on_turn_event(&mut self, index: usize, event: TurnEvent) {
        if let TurnEvent::Completed(outcome) = event {
            let transport = self.transport();
            let state = &mut self.conversations[index];
            state.running = false;
            state.status = format!("completed {}", outcome.short_commit);
            match transport {
                Ok(transport) => state.reload(&transport),
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
            }
            TurnEvent::ToolCall {
                step_commit,
                tool_use_id,
                summary,
                ..
            } => {
                state.activities.push(Activity {
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
                    state.activities.push(Activity {
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
            self.should_quit = true;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
            self.copy_mode = !self.copy_mode;
            return;
        }
        if self.copy_mode {
            if key.code == KeyCode::Esc {
                self.copy_mode = false;
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
                View::Chat => View::Diff,
                View::Diff => View::Chat,
            };
            self.selected_mut().scroll_from_bottom = 0;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.activity_expanded = !self.activity_expanded;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            self.start_new_conversation(None);
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
        match key.code {
            KeyCode::PageUp => self.scroll_up(8),
            KeyCode::PageDown => self.scroll_down(8),
            _ if self.view == View::Diff => {}
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.selected_mut().composer.insert_char('\n')
            }
            KeyCode::Enter => self.start_turn(),
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
            KeyCode::Up => self.selected_mut().composer.move_vertical(true),
            KeyCode::Down => self.selected_mut().composer.move_vertical(false),
            KeyCode::Home => self.selected_mut().composer.move_home(),
            KeyCode::End => self.selected_mut().composer.move_end(),
            _ => {}
        }
    }

    pub(crate) fn scroll_up(&mut self, rows: usize) {
        let state = self.selected_mut();
        state.scroll_from_bottom = state.scroll_from_bottom.saturating_add(rows);
    }

    pub(crate) fn scroll_down(&mut self, rows: usize) {
        let state = self.selected_mut();
        state.scroll_from_bottom = state.scroll_from_bottom.saturating_sub(rows);
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
        let disk = match list_conversations(&transport) {
            Ok(conversations) => conversations,
            Err(error) => {
                self.selected_mut().status = error;
                return;
            }
        };
        let name = first_available_conversation_name(
            disk.iter()
                .map(|item| item.name.as_str())
                .chain(self.conversations.iter().map(|item| item.name.as_str())),
        );
        let mut options = self.selected().turn_options.clone();
        options.base = base.clone();
        let status = base
            .as_deref()
            .map(|hash| format!("ready from {}; enter a prompt", short_hash(hash)))
            .unwrap_or_else(|| "new virtual conversation; enter a prompt".to_string());
        self.conversations
            .insert(0, ConversationState::new(name, options, status));
        self.selected = 0;
        self.view = View::Chat;
        self.confirm_action = None;
    }

    fn select_relative(&mut self, amount: isize) {
        let len = self.conversations.len() as isize;
        self.selected = (self.selected as isize + amount).rem_euclid(len) as usize;
        self.confirm_action = None;
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
            let name = self.selected().name.clone();
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

fn choose_conversation(
    requested: Option<&str>,
    new: bool,
    conversations: &[ConversationSummary],
) -> Result<String, String> {
    if let Some(requested) = requested {
        if new
            && conversations
                .iter()
                .any(|conversation| conversation.name == requested)
        {
            return Err(format!(
                "--new: conversation {requested:?} already exists; omit --new to continue it"
            ));
        }
        return Ok(requested.to_string());
    }
    if !new {
        if let Some(latest) = conversations.first() {
            return Ok(latest.name.clone());
        }
    }
    Ok(first_available_conversation_name(
        conversations
            .iter()
            .map(|conversation| conversation.name.as_str()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_core::backend::TestBackend;
    use ratatui_core::layout::Rect;
    use ratatui_core::terminal::Terminal;
    use ratatui_widgets::paragraph::{Paragraph, Wrap};

    use super::ui::{paragraph_scroll, render, scroll_offset};

    fn summary(name: &str) -> ConversationSummary {
        ConversationSummary {
            name: name.to_string(),
            head: "a".repeat(40),
            updated_unix: 1,
        }
    }

    fn state(name: &str) -> ConversationState {
        ConversationState::new(
            name.to_string(),
            TurnOptions::default(),
            "ready".to_string(),
        )
    }

    fn app_with(conversations: Vec<ConversationState>) -> (App, Sender<UiMessage>) {
        let (tx, rx) = mpsc::channel();
        (
            App {
                repo_dir: PathBuf::from("."),
                conversations,
                selected: 0,
                should_quit: false,
                copy_mode: false,
                confirm_action: None,
                activity_expanded: false,
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
                    .map(|conversation| conversation.name.as_str())
                    .chain(std::iter::once("talk-2")),
            ),
            "talk-3"
        );
    }

    #[test]
    fn cli_options_match_the_line_client_surface() {
        let args = Args::parse(&[
            "--from".into(),
            "5ec3751".into(),
            "--model".into(),
            "test-model".into(),
        ])
        .unwrap();
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
        assert!(rendered.contains("ccccccc"));
        assert!(rendered.contains("$ cargo test"));
        assert!(rendered.contains("follow-up"));
        assert!(rendered.contains("cancellation is not available"));

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(app.activity_expanded);
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let expanded: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
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
    fn switching_conversations_keeps_background_turn_state() {
        let mut first = state("talk-1");
        first.running = true;
        first.status = "calling model".to_string();
        let (mut app, tx) = app_with(vec![first, state("talk-2")]);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(app.selected().name, "talk-2");
        assert!(app.conversations[0].running);

        tx.send(UiMessage::Turn {
            conversation: "talk-1".to_string(),
            event: TurnEvent::Status("running a tool".to_string()),
        })
        .unwrap();
        assert!(app.drain_messages());
        assert_eq!(app.conversations[0].status, "running a tool");
        assert_eq!(app.selected().name, "talk-2");
    }

    #[test]
    fn reload_surfaces_history_errors_instead_of_showing_an_empty_chat() {
        let mut conversation = state("missing-conversation-for-reload-test");
        let transport = GitTransport::from_cwd().unwrap();
        conversation.reload(&transport);
        assert!(conversation.transcript.is_empty());
        assert!(conversation.diff.is_none());
        assert!(conversation.status.contains("loading conversation failed"));
        assert!(conversation.status.contains("no conversation"));
    }

    #[test]
    fn copy_mode_blocks_edits_and_ctrl_q_toggles_changes() {
        let (mut app, _) = app_with(vec![state("talk-1")]);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Diff);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Chat);

        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(app.copy_mode);
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.selected().composer.text.is_empty());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.copy_mode);
    }
}
