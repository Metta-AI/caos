//! A pinned conversation filesystem, with shell proposals kept separate until applied.
use super::*;
use caos_cli::filesystem::{self as fs, Entry, SnapshotInfo};
use ratatui_core::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    terminal::Frame,
};
use ratatui_widgets::{
    block::Block,
    borders::Borders,
    clear::Clear,
    list::{List, ListItem, ListState},
    paragraph::{Paragraph, Wrap},
};

static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(super) struct Browser {
    pub visible: bool,
    conversation: String,
    snapshot: Option<SnapshotInfo>,
    baseline: Option<SnapshotInfo>,
    pending: Option<String>,
    path: String,
    entries: Vec<Entry>,
    selected: usize,
    changes: bool,
    content: String,
    content_title: String,
    scroll: u16,
    status: String,
    dialog: Option<Dialog>,
    busy: bool,
    request: u64,
}
enum Dialog {
    Search(String),
    Shell(String),
    History {
        entries: Vec<SnapshotInfo>,
        selected: usize,
        baseline: bool,
    },
    Apply,
    Discard,
}
pub(super) enum Update {
    Snapshot {
        head: SnapshotInfo,
        baseline: SnapshotInfo,
        entries: Vec<Entry>,
    },
    Rows(Vec<Entry>),
    File {
        text: String,
        title: String,
        line: usize,
    },
    Search(fs::SearchResults),
    History(Vec<SnapshotInfo>, bool),
    Shell(fs::ShellResult),
    Applied(SnapshotInfo),
}

impl Browser {
    fn new(conversation: String, changes: bool) -> Self {
        Self {
            visible: true,
            conversation,
            snapshot: None,
            baseline: None,
            pending: None,
            path: String::new(),
            entries: Vec::new(),
            selected: 0,
            changes,
            content: String::new(),
            content_title: String::new(),
            scroll: 0,
            status: String::new(),
            dialog: None,
            busy: false,
            request: 0,
        }
    }
    fn tree(&self) -> Option<&str> {
        self.pending
            .as_deref()
            .or_else(|| self.snapshot.as_ref().map(|s| s.tree.as_str()))
    }
    fn baseline_tree(&self) -> Option<&str> {
        self.baseline.as_ref().map(|s| s.tree.as_str())
    }
}

impl App {
    pub(crate) fn browser_visible(&self) -> bool {
        self.browser.as_ref().is_some_and(|b| b.visible)
    }

    pub(super) fn open_browser(&mut self, changes: bool) {
        let conversation = self.selected().id.clone();
        if let Some(browser) = self.browser.as_mut() {
            if browser.conversation == conversation {
                browser.visible = true;
                if changes {
                    browser.changes = true;
                    self.load_browser_rows();
                }
                return;
            }
            if browser.busy || browser.pending.is_some() {
                self.selected_mut().show_command_error("Close the previous filesystem session after applying or discarding its shell edits.");
                return;
            }
        }
        self.browser = Some(Browser::new(conversation, changes));
        self.palette = None;
        self.load_browser_snapshot(None);
    }

    fn browser_job(
        &mut self,
        label: &str,
        job: impl FnOnce(&GitTransport) -> Result<Update, String> + Send + 'static,
    ) {
        let Some(browser) = self.browser.as_mut() else {
            return;
        };
        if browser.busy {
            return;
        }
        browser.busy = true;
        browser.status = label.into();
        let request = NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        browser.request = request;
        spawn(self.repo_dir.clone(), self.tx.clone(), job, move |result| {
            UiMessage::Filesystem { request, result }
        });
    }

    fn load_browser_snapshot(&mut self, revision: Option<String>) {
        let Some(browser) = self.browser.as_ref() else {
            return;
        };
        if browser.pending.is_some() {
            self.browser.as_mut().unwrap().status =
                "Apply or discard shell edits before changing snapshots.".into();
            return;
        }
        let id = browser.conversation.clone();
        let changes = browser.changes;
        self.browser_job("Loading snapshot…", move |t| {
            let head = fs::snapshot(t, &id, revision.as_deref())?;
            let baseline = match &head.parent {
                Some(parent) => fs::snapshot(t, &id, Some(parent))?,
                None => SnapshotInfo {
                    head: "empty".into(),
                    tree: conversation_protocol::v3::oid::empty_tree().to_string(),
                    parent: None,
                },
            };
            let entries = fs::list(t, &head.tree, &baseline.tree, "", changes)?;
            Ok(Update::Snapshot {
                head,
                baseline,
                entries,
            })
        });
    }

    fn load_browser_rows(&mut self) {
        let Some(b) = self.browser.as_ref() else {
            return;
        };
        let (Some(tree), Some(base)) = (b.tree(), b.baseline_tree()) else {
            return;
        };
        let (tree, base, path, changes) = (
            tree.to_string(),
            base.to_string(),
            b.path.clone(),
            b.changes,
        );
        self.browser_job("Loading files…", move |t| {
            Ok(Update::Rows(fs::list(t, &tree, &base, &path, changes)?))
        });
    }

    pub(super) fn browser_update(&mut self, request: u64, result: Result<Update, String>) {
        let Some(b) = self.browser.as_mut() else {
            return;
        };
        if b.request != request {
            return;
        }
        b.busy = false;
        b.status = String::new();
        let update = match result {
            Ok(update) => update,
            Err(error) => {
                b.status = error;
                return;
            }
        };
        match update {
            Update::Snapshot {
                head,
                baseline,
                entries,
            } => {
                b.snapshot = Some(head);
                b.baseline = Some(baseline);
                b.entries = entries;
                b.path.clear();
                b.selected = 0;
                b.content.clear();
                b.content_title.clear();
                b.scroll = 0;
            }
            Update::Rows(entries) => {
                b.entries = entries;
                b.selected = b.selected.min(b.entries.len().saturating_sub(1));
            }
            Update::File { text, title, line } => {
                b.content = text;
                b.content_title = title;
                b.scroll = line.min(u16::MAX as usize) as u16;
            }
            Update::Search(result) => {
                b.entries = result.entries;
                b.selected = 0;
                b.status = if result.limited {
                    "Search limited to 200 matches, 5,000 entries and 8 MiB; binary/large files skipped.".into()
                } else {
                    format!("{} matches", b.entries.len())
                };
            }
            Update::History(entries, baseline) => {
                b.dialog = Some(Dialog::History {
                    entries,
                    selected: 0,
                    baseline,
                })
            }
            Update::Shell(result) => {
                let original = b.snapshot.as_ref().unwrap().tree.clone();
                b.pending = (result.tree != original).then_some(result.tree);
                b.baseline = b.snapshot.clone();
                b.changes = true;
                b.path.clear();
                b.selected = 0;
                b.content = result.output;
                b.content_title = "Shell output; edits are not applied".into();
                b.scroll = 0;
                self.load_browser_rows();
            }
            Update::Applied(head) => {
                b.pending = None;
                b.snapshot = Some(head);
                b.content = "Shell changes applied to the conversation.".into();
                b.content_title = "Applied".into();
                b.scroll = 0;
                self.load_browser_rows();
            }
        }
    }

    pub(super) fn browser_paste(&mut self, text: &str) {
        if let Some(b) = self.browser.as_mut() {
            if let Some(Dialog::Search(input) | Dialog::Shell(input)) = b.dialog.as_mut() {
                input.push_str(text);
            }
        }
    }

    pub(super) fn handle_browser_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.selection_locked = true;
            return;
        }
        let current_head = self.selected().remote_head.clone();
        let Some(b) = self.browser.as_mut() else {
            return;
        };
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            if b.dialog.take().is_some() {
                return;
            }
            if !b.content.is_empty() {
                b.content.clear();
                b.content_title.clear();
                return;
            }
            b.visible = false;
            return;
        }
        if b.busy {
            return;
        }
        if let Some(mut dialog) = b.dialog.take() {
            match &mut dialog {
                Dialog::Search(input) | Dialog::Shell(input) => match key.code {
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        input.push(ch)
                    }
                    KeyCode::Enter => {
                        let input = input.clone();
                        let Some(tree) = b.tree().map(str::to_string) else {
                            return;
                        };
                        match dialog {
                            Dialog::Search(_) => {
                                let base = b.baseline_tree().unwrap().to_string();
                                let changes = b.changes;
                                self.browser_job("Searching snapshot…", move |t| {
                                    Ok(Update::Search(fs::search(
                                        t, &tree, &base, &input, changes,
                                    )?))
                                });
                            }
                            Dialog::Shell(_) => {
                                if input.trim().is_empty() {
                                    return;
                                }
                                let options = self.selected().turn_options.clone();
                                self.browser_job(
                                    "Running shell (30 second command limit)…",
                                    move |t| {
                                        Ok(Update::Shell(fs::shell(t, &options, &tree, &input)?))
                                    },
                                );
                            }
                            _ => unreachable!(),
                        }
                        return;
                    }
                    _ => {}
                },
                Dialog::History {
                    entries,
                    selected,
                    baseline,
                } => match key.code {
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => {
                        *selected = (*selected + 1).min(entries.len().saturating_sub(1))
                    }
                    KeyCode::Enter => {
                        if let Some(info) = entries.get(*selected).cloned() {
                            if *baseline {
                                b.baseline = Some(info);
                                b.changes = true;
                                self.load_browser_rows();
                            } else {
                                self.load_browser_snapshot(Some(info.head));
                            }
                        }
                        return;
                    }
                    _ => {}
                },
                Dialog::Apply if key.code == KeyCode::Enter => {
                    let (Some(base), Some(proposal)) = (
                        b.snapshot.as_ref().map(|s| s.head.clone()),
                        b.pending.clone(),
                    ) else {
                        return;
                    };
                    let id = b.conversation.clone();
                    self.browser_job("Applying reviewed edits…", move |t| {
                        let head = fs::apply_shell(t, &id, &base, &proposal)?;
                        Ok(Update::Applied(fs::snapshot(t, &id, Some(&head))?))
                    });
                    return;
                }
                Dialog::Discard if key.code == KeyCode::Enter => {
                    b.pending = None;
                    b.content.clear();
                    b.content_title.clear();
                    self.load_browser_rows();
                    return;
                }
                _ => {}
            }
            self.browser.as_mut().unwrap().dialog = Some(dialog);
            return;
        }
        match key.code {
            KeyCode::Up => b.selected = b.selected.saturating_sub(1),
            KeyCode::Down => b.selected = (b.selected + 1).min(b.entries.len().saturating_sub(1)),
            KeyCode::PageUp => b.scroll = b.scroll.saturating_sub(15),
            KeyCode::PageDown => b.scroll = b.scroll.saturating_add(15),
            KeyCode::Backspace | KeyCode::Left => {
                b.path = b
                    .path
                    .rsplit_once('/')
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_default();
                b.selected = 0;
                b.content.clear();
                self.load_browser_rows();
            }
            KeyCode::Enter | KeyCode::Right => {
                let Some(entry) = b.entries.get(b.selected).cloned() else {
                    return;
                };
                if entry.directory {
                    b.path = entry.path;
                    b.selected = 0;
                    b.content.clear();
                    self.load_browser_rows();
                } else {
                    let (Some(tree), Some(base)) = (
                        b.tree().map(str::to_string),
                        b.baseline_tree().map(str::to_string),
                    ) else {
                        return;
                    };
                    let changes = b.changes;
                    self.browser_job("Reading file…", move |t| {
                        Ok(Update::File {
                            text: fs::read(t, &tree, &base, &entry.path, changes)?,
                            title: entry.path,
                            line: if changes { 0 } else { entry.line },
                        })
                    });
                }
            }
            KeyCode::Char('r') => self.load_browser_snapshot(None),
            KeyCode::Char('d') => {
                b.changes = !b.changes;
                b.content.clear();
                self.load_browser_rows();
            }
            KeyCode::Char('/') => b.dialog = Some(Dialog::Search(String::new())),
            KeyCode::Char('s') => b.dialog = Some(Dialog::Shell(String::new())),
            KeyCode::Char('a') if b.pending.is_some() => b.dialog = Some(Dialog::Apply),
            KeyCode::Char('x') if b.pending.is_some() => b.dialog = Some(Dialog::Discard),
            KeyCode::Char('h' | 'b') => {
                let Some(head) = b.snapshot.as_ref().map(|s| s.head.clone()) else {
                    return;
                };
                let baseline = key.code == KeyCode::Char('b');
                self.browser_job("Loading snapshot history…", move |t| {
                    Ok(Update::History(fs::history(t, &head)?, baseline))
                });
            }
            KeyCode::Char('o') => {
                if b.pending.is_some()
                    || b.snapshot.as_ref().map(|s| &s.head) != current_head.as_ref()
                {
                    b.status =
                        "Refresh to the current snapshot before selecting a checkout/PR target."
                            .into();
                    return;
                }
                let Some(entry) = b.entries.get(b.selected) else {
                    return;
                };
                let name = entry.path.clone();
                if self.selected().source_trees.iter().any(|s| s.name == name) {
                    if let Err(error) = self.selected_mut().select_source_tree(&name) {
                        self.browser.as_mut().unwrap().status = error;
                    } else {
                        self.browser.as_mut().unwrap().status =
                            format!("Selected {name} for checkout/publication.");
                    }
                } else {
                    self.browser.as_mut().unwrap().status =
                        "Select an outer source-tree entry for checkout/publication.".into();
                }
            }
            _ => {}
        }
    }
}

fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

pub(super) fn render(app: &App, frame: &mut Frame<'_>) {
    let Some(b) = app.browser.as_ref().filter(|b| b.visible) else {
        return;
    };
    let area = frame.area();
    frame.render_widget(Clear, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(area);
    let head = b
        .snapshot
        .as_ref()
        .map(|s| s.head.as_str())
        .unwrap_or("loading");
    let base = b
        .baseline
        .as_ref()
        .map(|s| s.head.as_str())
        .unwrap_or("loading");
    let proposal = b
        .pending
        .as_ref()
        .map(|p| format!("  shell proposal {}", short_hash(p)))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!(
            "Conversation {}  snapshot {head}{proposal}\nCompare {} -> {}   {} /{}",
            b.conversation,
            short_hash(base),
            short_hash(head),
            if b.changes { "Changes" } else { "Files" },
            safe(&b.path)
        ))
        .block(
            Block::default()
                .title(" Conversation filesystem (pinned) ")
                .borders(Borders::BOTTOM),
        ),
        chunks[0],
    );
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(chunks[1]);
    let items = b.entries.iter().map(|e| {
        let commit = e
            .commit
            .as_ref()
            .map(|c| format!(" @{}", short_hash(c)))
            .unwrap_or_default();
        ListItem::new(format!(
            "{} {}{}{commit} {}",
            e.change,
            safe(&collapse_whitespace(&e.name)),
            if e.directory { "/" } else { "" },
            safe(&collapse_whitespace(&e.detail))
        ))
    });
    let mut state =
        ListState::default().with_selected((!b.entries.is_empty()).then_some(b.selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(Style::default().fg(Color::Cyan))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(if b.entries.is_empty() {
                        " Files (empty) "
                    } else {
                        " Files "
                    }),
            ),
        panes[0],
        &mut state,
    );
    frame.render_widget(
        Paragraph::new(safe(&b.content))
            .wrap(Wrap { trim: false })
            .scroll((b.scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(safe(&b.content_title)),
            ),
        panes[1],
    );
    frame.render_widget(
        Paragraph::new(safe(&b.status))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if b.busy { Color::Yellow } else { Color::Cyan })),
        chunks[2],
    );
    frame.render_widget(Paragraph::new("Enter open  Backspace up  / search  d files/changes  h snapshot  b compare  r refresh\ns shell  a review/apply  x discard  o select live checkout/PR target  PgUp/Dn scroll  Esc back"),chunks[3]);
    if let Some(dialog) = &b.dialog {
        let popup = Rect::new(
            area.x + area.width / 10,
            area.y + area.height / 5,
            area.width * 8 / 10,
            area.height * 3 / 5,
        );
        frame.render_widget(Clear, popup);
        match dialog {
            Dialog::History {
                entries,
                selected,
                baseline,
            } => {
                let items = entries
                    .iter()
                    .map(|s| ListItem::new(format!("{}  tree {}", s.head, short_hash(&s.tree))));
                let mut state = ListState::default().with_selected(Some(*selected));
                frame.render_stateful_widget(
                    List::new(items)
                        .highlight_symbol("> ")
                        .highlight_style(Style::default().fg(Color::Cyan))
                        .block(Block::default().borders(Borders::ALL).title(if *baseline {
                            " Compare from snapshot (latest 200; Enter selects) "
                        } else {
                            " Pin snapshot (latest 200; Enter selects) "
                        })),
                    popup,
                    &mut state,
                );
            }
            _ => {
                let (title,text)=match dialog {
                    Dialog::Search(input)=>("Search text across this snapshot (Enter searches)",input.clone()),
                    Dialog::Shell(input)=>("Shell command at / (Enter runs; non-interactive; 30s limit)",input.clone()),
                    Dialog::Apply=>("Apply shell edits?",format!("Apply proposal {} to conversation {}.\n\nChanges are reconciled against its current head. Conflicts apply nothing; .caos edits are rejected.\n\nEnter applies. Esc returns to review.",b.pending.as_deref().unwrap_or(""),b.conversation)),
                    Dialog::Discard=>("Discard shell edits?","Enter discards this proposal from the browser. Esc keeps it.".into()),
                    _=>unreachable!(),
                };
                frame.render_widget(
                    Paragraph::new(safe(&text))
                        .wrap(Wrap { trim: false })
                        .block(Block::default().borders(Borders::ALL).title(title)),
                    popup,
                );
            }
        }
    }
}
