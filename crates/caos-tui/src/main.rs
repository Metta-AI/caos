use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use caos::{
    apply_conversation_workspace, conversation_history, conversation_workspace_diff,
    list_conversations, run_chat_turn, ConversationRole, ConversationSummary, GitTransport,
    Transport, TurnEvent, TurnOptions, WorkspaceDiff,
};
use ratatui_core::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::terminal::{Frame, Terminal};
use ratatui_core::text::{Line, Span};
use ratatui_crossterm::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TerminalEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui_crossterm::crossterm::execute;
use ratatui_crossterm::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui_crossterm::CrosstermBackend;
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::list::{List, ListItem, ListState};
use ratatui_widgets::paragraph::{Paragraph, Wrap};

const TICK: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Args {
    conversation: Option<String>,
    new_conversation: bool,
    from_commit: Option<String>,
    turn: TurnOptions,
}

impl Args {
    fn parse(raw: &[String]) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut args = raw.iter();
        while let Some(arg) = args.next() {
            let value = |args: &mut std::slice::Iter<'_, String>, flag: &str| {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value\n{}", usage()))
            };
            match arg.as_str() {
                "-c" | "--conversation" => parsed.conversation = Some(value(&mut args, arg)?),
                "--new" => parsed.new_conversation = true,
                "--from" => parsed.from_commit = Some(value(&mut args, arg)?),
                "--base" => parsed.turn.base = Some(value(&mut args, arg)?),
                "--system" => parsed.turn.system = Some(value(&mut args, arg)?),
                "--system-file" => parsed.turn.system_file = Some(value(&mut args, arg)?),
                "--model" => parsed.turn.model = Some(value(&mut args, arg)?),
                "--base-url" => parsed.turn.base_url = Some(value(&mut args, arg)?),
                "--llm-step-bin" => parsed.turn.llm_step_bin = Some(value(&mut args, arg)?),
                "--bash-tool-bin" => parsed.turn.bash_tool_bin = Some(value(&mut args, arg)?),
                "--rgrep-bin" => parsed.turn.rgrep_bin = Some(value(&mut args, arg)?),
                "-h" | "--help" => return Err(usage()),
                other => return Err(format!("unknown option {other:?}\n{}", usage())),
            }
        }
        if parsed.turn.system.is_some() && parsed.turn.system_file.is_some() {
            return Err("--system and --system-file are mutually exclusive".to_string());
        }
        if parsed.from_commit.is_some() && parsed.turn.base.is_some() {
            return Err("--from and --base are mutually exclusive".to_string());
        }
        if parsed.from_commit.is_some() && parsed.conversation.is_some() {
            return Err(
                "--from starts a fresh conversation and cannot be combined with -c".to_string(),
            );
        }
        if let Some(from) = &parsed.from_commit {
            parsed.new_conversation = true;
            parsed.turn.base = Some(from.clone());
        }
        Ok(parsed)
    }
}

fn usage() -> String {
    "usage: caos-tui [--new | --from <commit>] [--base <revspec>] \
     [--system <text> | --system-file <path>] [--model <model>] [--base-url <url>]"
        .to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
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

    fn reload(&mut self) {
        match conversation_history(&self.name) {
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
                self.diff = conversation_workspace_diff(&self.name).ok();
            }
            Err(_) => {
                self.transcript.clear();
                self.diff = None;
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

struct App {
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
    fn new(mut args: Args) -> Result<Self, String> {
        // Fail before taking over the terminal if the repo or remote is invalid.
        let transport = GitTransport::from_cwd()?;
        if let Some(from) = args.from_commit.clone() {
            let commit = transport
                .resolve_revspec(&from)?
                .ok_or_else(|| format!("cannot resolve --from {from:?}"))?
                .to_string();
            args.from_commit = Some(commit.clone());
            args.turn.base = Some(commit);
        }
        let conversations = list_conversations()?;
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
            state.reload();
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
        std::thread::spawn(move || {
            let result = GitTransport::from_cwd().and_then(|transport| {
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

    fn drain_messages(&mut self) -> bool {
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
            TurnEvent::Completed(outcome) => {
                state.running = false;
                state.status = format!("completed {}", outcome.short_commit);
                state.reload();
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
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
                self.selected_mut().reload();
                self.selected_mut().status = "reloaded".to_string();
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

    fn scroll_up(&mut self, rows: usize) {
        let state = self.selected_mut();
        state.scroll_from_bottom = state.scroll_from_bottom.saturating_add(rows);
    }

    fn scroll_down(&mut self, rows: usize) {
        let state = self.selected_mut();
        state.scroll_from_bottom = state.scroll_from_bottom.saturating_sub(rows);
    }

    fn start_from_hash(&mut self, hash: &str) {
        let resolved = GitTransport::from_cwd()
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
        let disk = match list_conversations() {
            Ok(conversations) => conversations,
            Err(error) => {
                self.selected_mut().status = error;
                return;
            }
        };
        let name = next_available_name(&disk, &self.conversations);
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
            let name = self.selected().name.clone();
            self.selected_mut().status = match apply_conversation_workspace(&name) {
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
    Ok(next_auto_name(conversations))
}

fn next_auto_name(conversations: &[ConversationSummary]) -> String {
    for number in 1.. {
        let candidate = format!("talk-{number}");
        if conversations
            .iter()
            .all(|conversation| conversation.name != candidate)
        {
            return candidate;
        }
    }
    unreachable!("some talk-<n> is always free")
}

fn next_available_name(disk: &[ConversationSummary], states: &[ConversationState]) -> String {
    for number in 1.. {
        let candidate = format!("talk-{number}");
        if disk.iter().all(|item| item.name != candidate)
            && states.iter().all(|item| item.name != candidate)
        {
            return candidate;
        }
    }
    unreachable!("some talk-<n> is always free")
}

/// Publish the virtual workspace as a clean branch without checking it out.
///
/// Conversation commits retain their internal step DAG as second parents. A
/// PR should not expose that implementation history, so the publish branch is
/// a clean sequence of snapshot commits whose trees match conversation heads.
fn publish_conversation_pr(name: &str, diff: &WorkspaceDiff) -> Result<String, String> {
    let cwd = Path::new(".");
    let branch = prepare_publish_branch(name, diff, cwd)?;
    let branch_ref = format!("refs/heads/{branch}");
    let push_ref = format!("{branch_ref}:refs/heads/{branch}");
    capture_required("git", &["push", "--set-upstream", "origin", &push_ref], cwd)?;

    if let Some(url) = capture_optional(
        "gh",
        &["pr", "view", &branch, "--json", "url", "--jq", ".url"],
        cwd,
    )?
    .filter(|url| !url.is_empty())
    {
        return Ok(url);
    }
    let body = format!(
        "Published from virtual CAOS conversation `{name}` at `{}`.\n\nThe working tree was not modified.",
        short_hash(&diff.head)
    );
    capture_required(
        "gh",
        &[
            "pr",
            "create",
            "--head",
            &branch,
            "--title",
            &format!("CAOS conversation {name}"),
            "--body",
            &body,
        ],
        cwd,
    )
}

fn prepare_publish_branch(name: &str, diff: &WorkspaceDiff, cwd: &Path) -> Result<String, String> {
    let branch = format!("caos/{name}");
    let branch_ref = format!("refs/heads/{branch}");
    let head_tree_spec = format!("{}^{{tree}}", diff.head);
    let head_tree = capture_required("git", &["rev-parse", &head_tree_spec], cwd)?;
    let previous = capture_optional("git", &["rev-parse", "--verify", &branch_ref], cwd)?;
    let publish_commit = if let Some(previous) = previous.as_deref() {
        let previous_tree_spec = format!("{previous}^{{tree}}");
        let previous_tree = capture_required("git", &["rev-parse", &previous_tree_spec], cwd)?;
        if previous_tree == head_tree {
            previous.to_string()
        } else {
            capture_required(
                "git",
                &[
                    "commit-tree",
                    &head_tree,
                    "-p",
                    previous,
                    "-m",
                    &format!("Update CAOS conversation {name}"),
                ],
                cwd,
            )?
        }
    } else {
        capture_required(
            "git",
            &[
                "commit-tree",
                &head_tree,
                "-p",
                &diff.base,
                "-m",
                &format!("CAOS conversation {name}"),
            ],
            cwd,
        )?
    };
    match previous.as_deref() {
        Some(old) if old != publish_commit => {
            capture_required(
                "git",
                &["update-ref", &branch_ref, &publish_commit, old],
                cwd,
            )?;
        }
        None => {
            capture_required("git", &["update-ref", &branch_ref, &publish_commit], cwd)?;
        }
        _ => {}
    }
    Ok(branch)
}

fn capture_required(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("running {program}: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("{program} exited with {}", output.status)
        } else {
            detail
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn capture_optional(program: &str, args: &[&str], cwd: &Path) -> Result<Option<String>, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("running {program}: {error}"))?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    const LIMIT: usize = 120;
    let mut chars = line.chars();
    let shortened: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

fn short_hash(hash: &str) -> &str {
    hash.get(..7).unwrap_or(hash)
}

fn render(app: &App, frame: &mut Frame<'_>) {
    let area = frame.area();
    let activity_height = if app.activity_expanded { 10 } else { 3 };
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(12),
            Constraint::Length(1),
        ])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(26), Constraint::Min(40)])
        .split(outer[1]);
    let conversation = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(activity_height),
            Constraint::Length(6),
        ])
        .split(body[1]);
    let state = app.selected();

    render_header(app, state, frame, outer[0]);
    render_conversations(app, frame, body[0]);
    match app.view {
        View::Chat => render_transcript(state, frame, conversation[0]),
        View::Diff => render_diff(state, frame, conversation[0]),
    }
    render_activity(state, app.activity_expanded, frame, conversation[1]);
    render_composer(state, app.view, !app.copy_mode, frame, conversation[2]);
    render_footer(app.copy_mode, frame, outer[2]);
}

fn render_header(app: &App, state: &ConversationState, frame: &mut Frame<'_>, area: Rect) {
    let operation = if state.running {
        "running"
    } else if state.publishing {
        "publishing"
    } else {
        "idle"
    };
    let view = if app.copy_mode {
        "copy"
    } else if app.view == View::Chat {
        "chat"
    } else {
        "diff"
    };
    let running = app
        .conversations
        .iter()
        .filter(|conversation| conversation.running)
        .count();
    let header = Line::from(vec![
        Span::styled(" caos ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(format!("  {}  ", state.name)),
        Span::styled(operation, Style::default().fg(Color::Yellow)),
        Span::raw(format!("  [{view}]")),
        Span::raw("  "),
        Span::styled(
            state
                .current_hash()
                .map(|hash| format!("head {}", short_hash(hash)))
                .or_else(|| {
                    state
                        .turn_options
                        .base
                        .as_deref()
                        .map(|hash| format!("from {}", short_hash(hash)))
                })
                .unwrap_or_else(|| "new conversation".to_string()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("  {running} running"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(header), area);
}

fn render_conversations(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let items: Vec<ListItem<'_>> = app
        .conversations
        .iter()
        .map(|state| {
            let (mark, color) = if state.running {
                ("*", Color::Yellow)
            } else if state.publishing {
                ("^", Color::Cyan)
            } else {
                (" ", Color::DarkGray)
            };
            let hash = state
                .current_hash()
                .or(state.turn_options.base.as_deref())
                .map(short_hash)
                .unwrap_or("new");
            ListItem::new(Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(color)),
                Span::raw(state.name.clone()),
                Span::styled(format!("  {hash}"), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let mut selected = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" Conversations ")
                    .borders(Borders::ALL),
            )
            .highlight_symbol("> ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut selected,
    );
}

fn render_transcript(state: &ConversationState, frame: &mut Frame<'_>, area: Rect) {
    let mut lines = Vec::new();
    if state.transcript.is_empty() {
        lines.push(Line::styled(
            "No turns yet. Write a prompt below to start.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for entry in &state.transcript {
        let (label, color) = match entry.role {
            EntryRole::Human => ("You", Color::Cyan),
            EntryRole::Agent => ("Agent", Color::Green),
            EntryRole::Notice => ("Error", Color::Red),
        };
        let mut heading = vec![Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        if let Some(commit) = &entry.commit {
            heading.push(Span::styled(
                format!("  {}", short_hash(commit)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(heading));
        lines.extend(entry.text.lines().map(|line| Line::raw(line.to_string())));
        lines.push(Line::raw(""));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let scroll = paragraph_scroll(&paragraph, area, state.scroll_from_bottom);
    let paragraph = paragraph
        .block(
            Block::default()
                .title(" Conversation ")
                .borders(Borders::ALL),
        )
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_activity(state: &ConversationState, expanded: bool, frame: &mut Frame<'_>, area: Rect) {
    let mut lines = Vec::new();
    if expanded {
        lines.push(Line::from(vec![
            Span::styled("status  ", Style::default().fg(Color::Yellow)),
            Span::raw(state.status.clone()),
        ]));
        for item in &state.activities {
            let (mark, color) = activity_mark(item.state);
            lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(color)),
                Span::styled(
                    format!("{}  ", short_hash(&item.step_commit)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(item.summary.clone()),
            ]));
            lines.extend(item.detail.lines().map(|line| {
                Line::styled(format!("    {line}"), Style::default().fg(Color::DarkGray))
            }));
        }
    } else {
        let mut spans = vec![
            Span::styled("status  ", Style::default().fg(Color::Yellow)),
            Span::raw(state.status.clone()),
        ];
        if let Some(item) = state.activities.last() {
            let (mark, color) = activity_mark(item.state);
            spans.extend([
                Span::raw("    "),
                Span::styled(format!("{mark} "), Style::default().fg(color)),
                Span::styled(
                    format!("{}  ", short_hash(&item.step_commit)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(item.summary.clone()),
            ]);
            let detail = first_line(&item.detail);
            if !detail.is_empty() {
                spans.push(Span::styled(
                    format!(" — {detail}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    let title = if expanded {
        " Activity (Ctrl+A collapse) "
    } else {
        " Activity (Ctrl+A expand) "
    };
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let line_count = paragraph.line_count(area.width.saturating_sub(2));
    let visible = area.height.saturating_sub(2) as usize;
    let scroll = line_count.saturating_sub(visible).min(u16::MAX as usize) as u16;
    frame.render_widget(
        paragraph
            .block(Block::default().title(title).borders(Borders::ALL))
            .scroll((scroll, 0)),
        area,
    );
}

fn activity_mark(state: ActivityState) -> (&'static str, Color) {
    match state {
        ActivityState::Running => ("·", Color::Yellow),
        ActivityState::Succeeded => ("+", Color::Green),
        ActivityState::Failed => ("!", Color::Red),
    }
}

fn render_diff(state: &ConversationState, frame: &mut Frame<'_>, area: Rect) {
    let text = match &state.diff {
        Some(diff) if !diff.patch.is_empty() => diff.patch.as_str(),
        Some(_) => "No workspace changes in this conversation.",
        None => "This conversation has no completed turn yet.",
    };
    let lines: Vec<Line<'_>> = text
        .lines()
        .map(|line| {
            let color = if line.starts_with('+') && !line.starts_with("+++") {
                Color::Green
            } else if line.starts_with('-') && !line.starts_with("---") {
                Color::Red
            } else if line.starts_with("@@") {
                Color::Cyan
            } else {
                Color::Reset
            };
            Line::styled(line, Style::default().fg(color))
        })
        .collect();
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let scroll = paragraph_scroll(&paragraph, area, state.scroll_from_bottom);
    frame.render_widget(
        paragraph
            .block(
                Block::default()
                    .title(" Workspace diff ")
                    .borders(Borders::ALL),
            )
            .scroll((scroll, 0)),
        area,
    );
}

fn render_composer(
    state: &ConversationState,
    view: View,
    show_cursor: bool,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let title = if state.running {
        " Prompt (turn running; cancellation is not available) "
    } else if state.publishing {
        " Prompt (publishing PR) "
    } else {
        " Prompt (Enter sends, Alt+Enter/Ctrl+J adds a line) "
    };
    let (row, column) = state.composer.cursor_row_col();
    let inner_height = area.height.saturating_sub(2) as usize;
    let vertical_scroll = row.saturating_sub(inner_height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(state.composer.text.as_str())
            .block(Block::default().title(title).borders(Borders::ALL))
            .scroll((vertical_scroll.min(u16::MAX as usize) as u16, 0)),
        area,
    );
    if view == View::Chat && show_cursor {
        let cursor_row = row.saturating_sub(vertical_scroll);
        let x = area.x.saturating_add(1).saturating_add(column as u16);
        let y = area.y.saturating_add(1).saturating_add(cursor_row as u16);
        if x < area.right().saturating_sub(1) && y < area.bottom().saturating_sub(1) {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

fn render_footer(copy_mode: bool, frame: &mut Frame<'_>, area: Rect) {
    let footer = if copy_mode {
        Line::styled(
            " Copy mode: drag to select, use terminal copy, ^Y/Esc resumes",
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )
    } else {
        Line::raw(" ^Up/Dn chat  ^N new  ^Q changes  ^A activity  ^L load  ^P PR  ^Y copy  ^C quit")
    };
    frame.render_widget(Paragraph::new(footer), area);
}

fn paragraph_scroll(paragraph: &Paragraph<'_>, area: Rect, from_bottom: usize) -> u16 {
    let line_count = paragraph.line_count(area.width.saturating_sub(2));
    scroll_offset(line_count, area.height, from_bottom)
}

fn scroll_offset(line_count: usize, height: u16, from_bottom: usize) -> u16 {
    let visible = height.saturating_sub(2) as usize;
    line_count
        .saturating_sub(visible)
        .saturating_sub(from_bottom)
        .min(u16::MAX as usize) as u16
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), String> {
    terminal
        .draw(|frame| render(app, frame))
        .map_err(|error| format!("drawing terminal: {error}"))?;
    while !app.should_quit {
        // Copy mode deliberately freezes the frame: background turn messages
        // remain queued so redraws cannot invalidate a native terminal
        // selection. They are drained immediately when copy mode ends.
        let mut changed = if app.copy_mode {
            false
        } else {
            app.drain_messages()
        };
        if event::poll(TICK).map_err(|error| format!("polling terminal input: {error}"))? {
            match event::read().map_err(|error| format!("reading terminal input: {error}"))? {
                TerminalEvent::Key(key) => {
                    let was_copy_mode = app.copy_mode;
                    app.handle_key(key);
                    if was_copy_mode != app.copy_mode {
                        if app.copy_mode {
                            execute!(terminal.backend_mut(), DisableMouseCapture)
                        } else {
                            execute!(terminal.backend_mut(), EnableMouseCapture)
                        }
                        .map_err(|error| format!("switching terminal copy mode: {error}"))?;
                    }
                    changed = true;
                }
                TerminalEvent::Paste(text) if app.view == View::Chat && !app.copy_mode => {
                    app.selected_mut().composer.insert_str(&text);
                    changed = true;
                }
                TerminalEvent::Mouse(mouse) if !app.copy_mode => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        app.scroll_up(3);
                        changed = true;
                    }
                    MouseEventKind::ScrollDown => {
                        app.scroll_down(3);
                        changed = true;
                    }
                    _ => {}
                },
                TerminalEvent::Resize(_, _) if !app.copy_mode => changed = true,
                _ => {}
            }
        }
        if changed {
            terminal
                .draw(|frame| render(app, frame))
                .map_err(|error| format!("drawing terminal: {error}"))?;
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("caos-tui: {error}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        println!("{}", usage());
        return Ok(());
    }
    let args = Args::parse(&raw)?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("requires an interactive terminal; use `caos talk` for pipes".to_string());
    }
    let mut app = App::new(args)?;

    enable_raw_mode().map_err(|error| format!("enabling terminal raw mode: {error}"))?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
        return Err(format!("entering alternate screen: {error}"));
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            return Err(format!("initializing terminal: {error}"));
        }
    };
    let result = run_app(&mut terminal, &mut app);

    let raw_result = disable_raw_mode().map_err(|error| error.to_string());
    let screen_result = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .and_then(|()| terminal.show_cursor())
    .map_err(|error| error.to_string());
    result?;
    raw_result.map_err(|error| format!("restoring terminal mode: {error}"))?;
    screen_result.map_err(|error| format!("leaving alternate screen: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_core::backend::TestBackend;

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
            next_available_name(&conversations, &[state("talk-2")]),
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
    fn publish_branch_is_a_clean_snapshot_without_checkout_changes() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "caos-tui-publish-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        capture_required("git", &["init", "-q"], &dir).unwrap();
        capture_required("git", &["config", "user.name", "Test User"], &dir).unwrap();
        capture_required("git", &["config", "user.email", "test@example.com"], &dir).unwrap();
        std::fs::write(dir.join("file.txt"), "base\n").unwrap();
        capture_required("git", &["add", "file.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "base"], &dir).unwrap();
        let base = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();
        std::fs::write(dir.join("file.txt"), "conversation result\n").unwrap();
        capture_required("git", &["add", "file.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "internal turn"], &dir).unwrap();
        let head = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();
        let before = std::fs::read_to_string(dir.join("file.txt")).unwrap();
        let diff = WorkspaceDiff {
            base: base.clone(),
            head: head.clone(),
            stat: String::new(),
            patch: "changed".to_string(),
        };

        let branch = prepare_publish_branch("publish-test", &diff, &dir).unwrap();
        assert_eq!(branch, "caos/publish-test");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            before
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test^{tree}"], &dir,).unwrap(),
            capture_required("git", &["rev-parse", &format!("{head}^{{tree}}")], &dir).unwrap()
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test^"], &dir).unwrap(),
            base
        );
        let first = capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap();
        prepare_publish_branch("publish-test", &diff, &dir).unwrap();
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap(),
            first
        );

        std::fs::remove_dir_all(dir).unwrap();
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
