//! State and background work for the conversation-ref follower.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use caos::chat::{
    conversation_head, conversation_snapshot, pick_conversation, prepare_queued_request,
    resolve_username, resume_request, submit_message, submit_new_message, ConversationSnapshot,
    TurnOptions,
};
use caos::GitTransport;
use ratatui_crossterm::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::args::Args;
use super::workspace::publish_conversation_pr;

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayMessage {
    pub(crate) author: String,
    pub(crate) username: Option<String>,
    pub(crate) content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotView {
    head: String,
    title: String,
    status: String,
    request: Option<String>,
    request_head: Option<String>,
    messages: Vec<DisplayMessage>,
}

impl From<ConversationSnapshot> for SnapshotView {
    fn from(snapshot: ConversationSnapshot) -> Self {
        Self {
            head: snapshot.head,
            title: snapshot.title,
            status: snapshot.status,
            request: snapshot.request,
            request_head: snapshot.request_head,
            messages: snapshot
                .messages
                .into_iter()
                .map(|message| DisplayMessage {
                    author: message.author,
                    username: message.username,
                    content: message.content,
                })
                .collect(),
        }
    }
}

enum BackgroundMessage {
    Snapshot {
        generation: u64,
        result: Result<SnapshotRefresh, String>,
    },
    RequestFinished {
        request: String,
        result: Result<(), String>,
    },
    Published(Result<String, String>),
}

enum SnapshotRefresh {
    Unchanged,
    Changed(Option<ConversationSnapshot>),
}

pub(crate) struct App {
    repo_dir: PathBuf,
    conversation: String,
    turn_options: TurnOptions,
    require_absent_on_next_submit: bool,
    snapshot: Option<SnapshotView>,
    composer: String,
    cursor: usize,
    scroll_from_bottom: u16,
    submitting: bool,
    publishing: bool,
    notice: Option<String>,
    should_quit: bool,
    tx: Sender<BackgroundMessage>,
    rx: Receiver<BackgroundMessage>,
    next_refresh: Instant,
    refresh_in_flight: bool,
    refresh_generation: u64,
    applied_generation: u64,
    requests_in_flight: HashSet<String>,
}

impl App {
    pub(crate) fn new(mut args: Args) -> Result<Self, String> {
        let transport = GitTransport::from_cwd()?;
        args.turn.username = Some(resolve_username(&transport, args.turn.username.as_deref())?);
        let repo_dir = transport.work_dir().to_path_buf();
        let (conversation, fresh) = pick_conversation(
            &transport,
            args.conversation.as_deref(),
            args.new_conversation,
        )?;
        let snapshot = conversation_snapshot(&transport, &conversation)?.map(SnapshotView::from);
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            repo_dir,
            conversation,
            turn_options: args.turn,
            require_absent_on_next_submit: args.new_conversation && fresh,
            snapshot,
            composer: String::new(),
            cursor: 0,
            scroll_from_bottom: 0,
            submitting: false,
            publishing: false,
            notice: None,
            should_quit: false,
            tx,
            rx,
            next_refresh: Instant::now(),
            refresh_in_flight: false,
            refresh_generation: 0,
            applied_generation: 0,
            requests_in_flight: HashSet::new(),
        };
        app.resume_snapshot_request();
        Ok(app)
    }

    pub(crate) fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub(crate) fn conversation(&self) -> &str {
        &self.conversation
    }

    pub(crate) fn username(&self) -> &str {
        self.turn_options
            .username
            .as_deref()
            .expect("App::new resolves a username")
    }

    pub(crate) fn title(&self) -> &str {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.title.as_str())
            .filter(|title| !title.is_empty())
            .unwrap_or(&self.conversation)
    }

    pub(crate) fn head(&self) -> Option<&str> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.head.as_str())
    }

    pub(crate) fn status(&self) -> &str {
        if self.publishing {
            "publishing"
        } else if self.submitting {
            "submitting"
        } else {
            self.snapshot
                .as_ref()
                .map(|snapshot| snapshot.status.as_str())
                .filter(|status| !status.is_empty())
                .unwrap_or("ready")
        }
    }

    pub(crate) fn messages(&self) -> &[DisplayMessage] {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.messages.as_slice())
            .unwrap_or(&[])
    }

    pub(crate) fn composer(&self) -> &str {
        &self.composer
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn scroll_from_bottom(&self) -> u16 {
        self.scroll_from_bottom
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Start a refresh when due and apply any completed background work.
    pub(crate) fn tick(&mut self) -> bool {
        self.start_refresh_if_due();
        let mut changed = false;
        while let Ok(message) = self.rx.try_recv() {
            changed = true;
            self.handle_background(message);
        }
        changed
    }

    pub(crate) fn force_refresh(&mut self) {
        self.next_refresh = Instant::now();
    }

    fn start_refresh_if_due(&mut self) {
        if self.refresh_in_flight || Instant::now() < self.next_refresh {
            return;
        }
        self.refresh_in_flight = true;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let tx = self.tx.clone();
        let repo_dir = self.repo_dir.clone();
        let conversation = self.conversation.clone();
        let known_head = self.snapshot.as_ref().map(|snapshot| snapshot.head.clone());
        std::thread::spawn(move || {
            let result = GitTransport::discover(repo_dir).and_then(|transport| {
                let head = conversation_head(&transport, &conversation)?;
                if head == known_head {
                    Ok(SnapshotRefresh::Unchanged)
                } else {
                    conversation_snapshot(&transport, &conversation).map(SnapshotRefresh::Changed)
                }
            });
            let _ = tx.send(BackgroundMessage::Snapshot { generation, result });
        });
    }

    fn handle_background(&mut self, message: BackgroundMessage) {
        match message {
            BackgroundMessage::Snapshot { generation, result } => {
                self.refresh_in_flight = false;
                self.next_refresh = Instant::now() + REFRESH_INTERVAL;
                if generation < self.applied_generation {
                    return;
                }
                self.applied_generation = generation;
                match result {
                    Ok(SnapshotRefresh::Unchanged) => {}
                    Ok(SnapshotRefresh::Changed(snapshot)) => {
                        self.snapshot = snapshot.map(SnapshotView::from);
                    }
                    Err(error) => self.notice = Some(format!("refresh failed: {error}")),
                }
            }
            BackgroundMessage::RequestFinished { request, result } => {
                self.requests_in_flight.remove(&request);
                if let Err(error) = result {
                    self.notice = Some(format!("request {request} failed: {error}"));
                }
                self.force_refresh();
            }
            BackgroundMessage::Published(result) => {
                self.publishing = false;
                self.notice = Some(match result {
                    Ok(url) => format!("PR ready: {url}"),
                    Err(error) => format!("PR failed: {error}"),
                });
            }
        }
    }

    fn resume_snapshot_request(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        if !request_is_active(&snapshot.status) {
            return;
        }
        let request = match snapshot.request.clone() {
            Some(request) => Ok(request),
            None if snapshot.status == "queued" => snapshot
                .request_head
                .as_deref()
                .ok_or_else(|| "queued conversation has no request head".to_string())
                .and_then(|queued_head| {
                    GitTransport::discover(&self.repo_dir).and_then(|transport| {
                        prepare_queued_request(
                            &transport,
                            &self.turn_options,
                            &self.conversation,
                            queued_head,
                        )
                    })
                }),
            None => Err(format!(
                "conversation is {:?} but has no active request",
                snapshot.status
            )),
        };
        match request {
            Ok(request) => self.spawn_resume(request),
            Err(error) => self.notice = Some(format!("request preparation failed: {error}")),
        }
    }

    fn spawn_resume(&mut self, request: String) {
        if self.requests_in_flight.contains(&request) {
            return;
        }
        self.requests_in_flight.insert(request.clone());
        let tx = self.tx.clone();
        let repo_dir = self.repo_dir.clone();
        std::thread::spawn(move || {
            let result = GitTransport::discover(repo_dir)
                .and_then(|transport| resume_request(&transport, &request));
            let _ = tx.send(BackgroundMessage::RequestFinished { request, result });
        });
    }

    fn submit(&mut self) {
        if self.submitting {
            return;
        }
        let Some(message) = self.composed_message() else {
            return;
        };
        // Submission owns the UI thread through the durable user append. Until
        // `submit_message` returns, the draft remains in the composer and no
        // later input (including quit) can be handled. Request preparation
        // happens only after that append; the recoverable wait is detached.
        self.submitting = true;
        self.notice = None;
        let result = GitTransport::discover(&self.repo_dir).and_then(|transport| {
            if self.require_absent_on_next_submit {
                submit_new_message(&transport, &self.turn_options, &self.conversation, &message)
            } else {
                submit_message(&transport, &self.turn_options, &self.conversation, &message)
            }
        });
        self.submitting = false;

        match result {
            Ok(submitted) => {
                self.require_absent_on_next_submit = false;
                self.composer.clear();
                self.cursor = 0;
                self.scroll_from_bottom = 0;
                if let Some(queued_head) = submitted {
                    match GitTransport::discover(&self.repo_dir).and_then(|transport| {
                        prepare_queued_request(
                            &transport,
                            &self.turn_options,
                            &self.conversation,
                            &queued_head,
                        )
                    }) {
                        Ok(request) => self.spawn_resume(request),
                        Err(error) => {
                            self.notice = Some(format!("request preparation failed: {error}"))
                        }
                    }
                }
                self.force_refresh();
            }
            Err(error) => {
                // The original bytes and cursor stay intact so a failed append
                // never turns into a lost draft.
                self.notice = Some(format!("submit failed: {error}"));
            }
        }
    }

    fn publish(&mut self) {
        if self.publishing {
            return;
        }
        let Some(snapshot) = self.snapshot.as_ref() else {
            self.notice = Some("PR failed: conversation has no workspace yet".to_string());
            return;
        };
        if request_is_active(&snapshot.status) {
            self.notice = Some("PR failed: wait for the active turn to finish".to_string());
            return;
        }
        self.publishing = true;
        self.notice = None;
        let tx = self.tx.clone();
        let cwd = self.repo_dir.clone();
        let conversation = self.conversation.clone();
        let head = snapshot.head.clone();
        std::thread::spawn(move || {
            let result = publish_conversation_pr(&conversation, &head, &cwd);
            let _ = tx.send(BackgroundMessage::Published(result));
        });
    }

    fn composed_message(&self) -> Option<String> {
        (!self.composer.trim().is_empty()).then(|| self.composer.clone())
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        self.composer.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn move_cursor_left(&mut self) {
        if let Some((offset, _)) = self.composer[..self.cursor].char_indices().next_back() {
            self.cursor = offset;
        }
    }

    fn move_cursor_right(&mut self) {
        if self.cursor >= self.composer.len() {
            return;
        }
        let width = self.composer[self.cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
        self.cursor += width;
    }

    fn backspace(&mut self) {
        let end = self.cursor;
        self.move_cursor_left();
        if self.cursor < end {
            self.composer.drain(self.cursor..end);
        }
    }

    fn delete(&mut self) {
        if self.cursor >= self.composer.len() {
            return;
        }
        let end = self.cursor
            + self.composer[self.cursor..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(0);
        self.composer.drain(self.cursor..end);
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
        {
            self.should_quit = true;
            return true;
        }

        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => self.publish(),
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.force_refresh()
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_text(&character.to_string())
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_cursor_left(),
            KeyCode::Right => self.move_cursor_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.composer.len(),
            KeyCode::Up => self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(3),
            KeyCode::Down => self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(3),
            KeyCode::PageUp => self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(12),
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(12)
            }
            _ => return false,
        }
        true
    }

    #[cfg(test)]
    fn test() -> Self {
        let (tx, rx) = mpsc::channel();
        let turn_options = TurnOptions {
            username: Some("test-user".to_string()),
            ..TurnOptions::default()
        };
        Self {
            repo_dir: PathBuf::from("."),
            conversation: "test-conversation".to_string(),
            turn_options,
            require_absent_on_next_submit: false,
            snapshot: None,
            composer: String::new(),
            cursor: 0,
            scroll_from_bottom: 0,
            submitting: false,
            publishing: false,
            notice: None,
            should_quit: false,
            tx,
            rx,
            next_refresh: Instant::now() + Duration::from_secs(60),
            refresh_in_flight: false,
            refresh_generation: 0,
            applied_generation: 0,
            requests_in_flight: HashSet::new(),
        }
    }

    #[cfg(test)]
    fn apply_test_snapshot(&mut self, head: &str, messages: &[(&str, &str)]) {
        self.snapshot = Some(SnapshotView {
            head: head.to_string(),
            title: "Test".to_string(),
            status: "idle".to_string(),
            request: None,
            request_head: None,
            messages: messages
                .iter()
                .map(|(author, content)| DisplayMessage {
                    author: (*author).to_string(),
                    username: None,
                    content: (*content).to_string(),
                })
                .collect(),
        });
    }
}

fn request_is_active(status: &str) -> bool {
    matches!(status, "queued" | "running")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successive_head_snapshots_replace_the_visible_transcript() {
        let mut app = App::test();
        app.apply_test_snapshot("aaaaaaaa", &[("user", "first")]);
        assert_eq!(app.head(), Some("aaaaaaaa"));
        assert_eq!(app.messages().len(), 1);

        app.apply_test_snapshot("bbbbbbbb", &[("user", "first"), ("assistant", "second")]);
        assert_eq!(app.head(), Some("bbbbbbbb"));
        assert_eq!(app.messages().len(), 2);
        assert_eq!(app.messages()[1].content, "second");
    }

    #[test]
    fn composer_edits_on_utf8_boundaries() {
        let mut app = App::test();
        app.insert_text("aé");
        app.move_cursor_left();
        app.backspace();
        assert_eq!(app.composer(), "é");
        assert_eq!(app.cursor(), 0);

        app.delete();
        assert_eq!(app.composer(), "");
        assert_eq!(app.cursor(), 0);
    }

    #[test]
    fn escape_and_control_c_quit() {
        let mut escape = App::test();
        assert!(escape.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(escape.should_quit());

        let mut control_c = App::test();
        assert!(control_c.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL,)));
        assert!(control_c.should_quit());
    }

    #[test]
    fn only_recoverable_states_resume_requests() {
        assert!(request_is_active("queued"));
        assert!(request_is_active("running"));
        assert!(!request_is_active("idle"));
        assert!(!request_is_active("failed"));
        assert!(!request_is_active("canceled"));
    }

    #[test]
    fn snapshot_keeps_distinct_usernames() {
        let mut app = App::test();
        app.snapshot = Some(SnapshotView {
            head: "aaaaaaaa".to_string(),
            title: "Test".to_string(),
            status: "running".to_string(),
            request: Some("b".repeat(40)),
            request_head: Some("a".repeat(40)),
            messages: vec![
                DisplayMessage {
                    author: "user".to_string(),
                    username: Some("Alice".to_string()),
                    content: "first".to_string(),
                },
                DisplayMessage {
                    author: "user".to_string(),
                    username: Some("Bob".to_string()),
                    content: "also check this".to_string(),
                },
            ],
        });

        assert_eq!(app.messages()[0].username.as_deref(), Some("Alice"));
        assert_eq!(app.messages()[1].username.as_deref(), Some("Bob"));
        assert_eq!(app.status(), "running");
    }

    #[test]
    fn composer_preserves_nonblank_message_bytes() {
        let mut app = App::test();
        app.insert_text("  exact code block\n");
        assert_eq!(
            app.composed_message().as_deref(),
            Some("  exact code block\n")
        );

        app.composer = " \n\t".to_string();
        assert_eq!(app.composed_message(), None);
    }

    #[test]
    fn unchanged_poll_keeps_the_visible_snapshot() {
        let mut app = App::test();
        app.apply_test_snapshot("aaaaaaaa", &[("user", "still here")]);
        app.handle_background(BackgroundMessage::Snapshot {
            generation: 1,
            result: Ok(SnapshotRefresh::Unchanged),
        });
        assert_eq!(app.head(), Some("aaaaaaaa"));
        assert_eq!(app.messages()[0].content, "still here");
    }
}
