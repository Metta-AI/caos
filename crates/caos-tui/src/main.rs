use std::io::{self, IsTerminal};
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

enum UiMessage {
    Turn(TurnEvent),
    Failed(String),
}

struct App {
    conversation: String,
    turn_options: TurnOptions,
    transcript: Vec<TranscriptEntry>,
    activities: Vec<Activity>,
    diff: Option<WorkspaceDiff>,
    composer: Composer,
    status: String,
    running: bool,
    should_quit: bool,
    confirm_apply: bool,
    activity_expanded: bool,
    view: View,
    scroll_from_bottom: usize,
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
        let conversation = choose_conversation(
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
        let mut app = Self {
            conversation,
            turn_options: args.turn,
            transcript: Vec::new(),
            activities: Vec::new(),
            diff: None,
            composer: Composer::default(),
            status: initial_status,
            running: false,
            should_quit: false,
            confirm_apply: false,
            activity_expanded: false,
            view: View::Chat,
            scroll_from_bottom: 0,
            tx,
            rx,
        };
        app.reload_conversation();
        Ok(app)
    }

    fn reload_conversation(&mut self) {
        match conversation_history(&self.conversation) {
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
                self.diff = conversation_workspace_diff(&self.conversation).ok();
            }
            Err(_) => {
                self.transcript.clear();
                self.diff = None;
            }
        }
        self.scroll_from_bottom = 0;
    }

    fn start_turn(&mut self) {
        if self.running {
            self.status = "a turn is already running".to_string();
            return;
        }
        let Some(message) = self.composer.take_message() else {
            return;
        };
        if let Some(hash) = message.strip_prefix("/from ").map(str::trim) {
            self.start_from_hash(hash);
            return;
        }
        if message == "/from" {
            self.status = "usage: /from <commit>".to_string();
            return;
        }
        self.transcript.push(TranscriptEntry {
            role: EntryRole::Human,
            commit: None,
            text: message.clone(),
        });
        self.activities.clear();
        self.running = true;
        self.status = "starting turn".to_string();
        self.scroll_from_bottom = 0;

        let tx = self.tx.clone();
        let options = self.turn_options.clone();
        let conversation = self.conversation.clone();
        std::thread::spawn(move || {
            let result = GitTransport::from_cwd().and_then(|transport| {
                run_chat_turn(&transport, &options, &conversation, &message, |event| {
                    let _ = tx.send(UiMessage::Turn(event));
                })
                .map(|_| ())
            });
            if let Err(error) = result {
                let _ = tx.send(UiMessage::Failed(error));
            }
        });
    }

    fn drain_messages(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.rx.try_recv() {
            changed = true;
            match message {
                UiMessage::Turn(event) => self.on_turn_event(event),
                UiMessage::Failed(error) => {
                    self.running = false;
                    self.status = "turn failed".to_string();
                    self.transcript.push(TranscriptEntry {
                        role: EntryRole::Notice,
                        commit: None,
                        text: error,
                    });
                }
            }
        }
        changed
    }

    fn on_turn_event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::PhaseComplete {
                label,
                elapsed_secs,
            } => self.status = format!("{label}: {elapsed_secs:.1}s"),
            TurnEvent::Status(status) => self.status = status,
            TurnEvent::AssistantText(text) => {
                self.transcript.push(TranscriptEntry {
                    role: EntryRole::Agent,
                    commit: None,
                    text,
                });
                self.scroll_from_bottom = 0;
            }
            TurnEvent::ToolCall {
                step_commit,
                tool_use_id,
                summary,
                ..
            } => {
                self.activities.push(Activity {
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
                if let Some(activity) = self
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
                    self.activities.push(Activity {
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
                self.running = false;
                self.status = format!("completed {}", outcome.short_commit);
                self.reload_conversation();
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
        let is_apply =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a');
        if !is_apply {
            self.confirm_apply = false;
        }
        if is_apply {
            if self.running {
                self.status = "finish the running turn before applying changes".to_string();
            } else if self.diff.as_ref().is_none_or(|diff| diff.patch.is_empty()) {
                self.status = "there are no conversation changes to apply".to_string();
            } else if !self.confirm_apply {
                self.confirm_apply = true;
                self.status =
                    "press Ctrl+A again to apply this diff to a clean working tree".to_string();
            } else {
                self.confirm_apply = false;
                match apply_conversation_workspace(&self.conversation) {
                    Ok(()) => self.status = "workspace changes applied".to_string(),
                    Err(error) => self.status = error,
                }
            }
            return;
        }
        if key.code == KeyCode::F(2) {
            self.view = match self.view {
                View::Chat => View::Diff,
                View::Diff => View::Chat,
            };
            self.scroll_from_bottom = 0;
            return;
        }
        if key.code == KeyCode::F(3) {
            self.activity_expanded = !self.activity_expanded;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            if !self.running {
                self.reload_conversation();
                self.status = "reloaded".to_string();
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
                self.composer.insert_char('\n')
            }
            KeyCode::Enter => self.start_turn(),
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_char('\n')
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_char(ch)
            }
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left => self.composer.move_left(),
            KeyCode::Right => self.composer.move_right(),
            KeyCode::Up => self.composer.move_vertical(true),
            KeyCode::Down => self.composer.move_vertical(false),
            KeyCode::Home => self.composer.move_home(),
            KeyCode::End => self.composer.move_end(),
            _ => {}
        }
    }

    fn scroll_up(&mut self, rows: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(rows);
    }

    fn scroll_down(&mut self, rows: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(rows);
    }

    fn start_from_hash(&mut self, hash: &str) {
        if self.running {
            self.status = "finish the running turn before starting from a commit".to_string();
            return;
        }
        let resolved = GitTransport::from_cwd()
            .and_then(|transport| transport.resolve_revspec(hash))
            .and_then(|commit| commit.ok_or_else(|| format!("cannot resolve commit {hash:?}")));
        let commit = match resolved {
            Ok(commit) => commit.to_string(),
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let conversations = match list_conversations() {
            Ok(conversations) => conversations,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        self.conversation = next_auto_name(&conversations);
        self.turn_options.base = Some(commit.clone());
        self.activities.clear();
        self.transcript.clear();
        self.diff = None;
        self.scroll_from_bottom = 0;
        self.status = format!("ready from {}; enter a prompt", short_hash(&commit));
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
            Constraint::Min(6),
            Constraint::Length(activity_height),
            Constraint::Length(6),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(app, frame, outer[0]);
    match app.view {
        View::Chat => render_transcript(app, frame, outer[1]),
        View::Diff => render_diff(app, frame, outer[1]),
    }
    render_activity(app, frame, outer[2]);
    render_composer(app, frame, outer[3]);
    render_footer(frame, outer[4]);
}

fn render_header(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let state = if app.running { "running" } else { "idle" };
    let view = if app.view == View::Chat {
        "chat"
    } else {
        "diff"
    };
    let header = Line::from(vec![
        Span::styled(" caos ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(state, Style::default().fg(Color::Yellow)),
        Span::raw(format!("  [{view}]")),
        Span::raw("  "),
        Span::styled(
            current_hash(app)
                .map(|hash| format!("head {}", short_hash(hash)))
                .or_else(|| {
                    app.turn_options
                        .base
                        .as_deref()
                        .map(|hash| format!("from {}", short_hash(hash)))
                })
                .unwrap_or_else(|| "new conversation".to_string()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(header), area);
}

fn current_hash(app: &App) -> Option<&str> {
    app.transcript
        .iter()
        .rev()
        .find_map(|entry| entry.commit.as_deref())
}

fn render_transcript(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let mut lines = Vec::new();
    if app.transcript.is_empty() {
        lines.push(Line::styled(
            "No turns yet. Write a prompt below to start.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for entry in &app.transcript {
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
    let scroll = paragraph_scroll(&paragraph, area, app.scroll_from_bottom);
    let paragraph = paragraph
        .block(
            Block::default()
                .title(" Conversation ")
                .borders(Borders::ALL),
        )
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_activity(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let mut lines = Vec::new();
    if app.activity_expanded {
        lines.push(Line::from(vec![
            Span::styled("status  ", Style::default().fg(Color::Yellow)),
            Span::raw(app.status.clone()),
        ]));
        for item in &app.activities {
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
            Span::raw(app.status.clone()),
        ];
        if let Some(item) = app.activities.last() {
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
    let title = if app.activity_expanded {
        " Activity (F3 collapse) "
    } else {
        " Activity (F3 expand) "
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

fn render_diff(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let text = match &app.diff {
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
    let scroll = paragraph_scroll(&paragraph, area, app.scroll_from_bottom);
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

fn render_composer(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let title = if app.running {
        " Prompt (turn running; cancellation is not available) "
    } else {
        " Prompt (Enter sends, Alt+Enter/Ctrl+J adds a line) "
    };
    let (row, column) = app.composer.cursor_row_col();
    let inner_height = area.height.saturating_sub(2) as usize;
    let vertical_scroll = row.saturating_sub(inner_height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(app.composer.text.as_str())
            .block(Block::default().title(title).borders(Borders::ALL))
            .scroll((vertical_scroll.min(u16::MAX as usize) as u16, 0)),
        area,
    );
    if app.view == View::Chat {
        let cursor_row = row.saturating_sub(vertical_scroll);
        let x = area.x.saturating_add(1).saturating_add(column as u16);
        let y = area.y.saturating_add(1).saturating_add(cursor_row as u16);
        if x < area.right().saturating_sub(1) && y < area.bottom().saturating_sub(1) {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect) {
    let footer = Line::raw(
        " F2 diff  F3 activity  ^A apply  PgUp/Dn or wheel scroll  /from TURN_HASH branch  ^C quit",
    );
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
        let mut changed = app.drain_messages();
        if event::poll(TICK).map_err(|error| format!("polling terminal input: {error}"))? {
            match event::read().map_err(|error| format!("reading terminal input: {error}"))? {
                TerminalEvent::Key(key) => {
                    app.handle_key(key);
                    changed = true;
                }
                TerminalEvent::Paste(text) if app.view == View::Chat => {
                    app.composer.insert_str(&text);
                    changed = true;
                }
                TerminalEvent::Mouse(mouse) => match mouse.kind {
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
                TerminalEvent::Resize(_, _) => changed = true,
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
        let (tx, rx) = mpsc::channel();
        let mut app = App {
            conversation: "review-api".to_string(),
            turn_options: TurnOptions::default(),
            transcript: vec![
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
            ],
            activities: vec![Activity {
                id: "tool-1".to_string(),
                step_commit: "c".repeat(40),
                summary: "$ cargo test".to_string(),
                detail: "12 tests passed".to_string(),
                state: ActivityState::Running,
            }],
            diff: None,
            composer: Composer::default(),
            status: "calling model".to_string(),
            running: true,
            should_quit: false,
            confirm_apply: false,
            activity_expanded: false,
            view: View::Chat,
            scroll_from_bottom: 0,
            tx,
            rx,
        };
        app.composer.insert_str("follow-up");
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
        assert!(!rendered.contains("Tasks"));
        assert!(rendered.contains("head bbbbbbb"));
        assert!(rendered.contains("Please run the tests"));
        assert!(rendered.contains("ccccccc"));
        assert!(rendered.contains("$ cargo test"));
        assert!(rendered.contains("follow-up"));
        assert!(rendered.contains("cancellation is not available"));

        app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
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

        app.running = false;
        app.diff = Some(WorkspaceDiff {
            base: "a".repeat(40),
            head: "b".repeat(40),
            stat: "1 file changed".to_string(),
            patch: "diff --git a/a b/a".to_string(),
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(app.confirm_apply);
        assert!(app.status.contains("press Ctrl+A again"));
    }
}
