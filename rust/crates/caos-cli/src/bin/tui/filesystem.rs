//! Read-only navigation with automatic file and source-boundary previews.
use super::*;
use caos_cli::filesystem::{self as fs, Entry, SnapshotInfo};
use ratatui_core::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    terminal::Frame,
    text::{Line, Span},
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
    path: String,
    entries: Vec<Entry>,
    selected: usize,
    parents: Vec<usize>,
    content: String,
    title: String,
    scroll: u16,
    status: String,
    request: u64,
}

pub(super) enum Update {
    Snapshot(SnapshotInfo, Vec<Entry>),
    Rows(Vec<Entry>),
    Preview(fs::Preview),
}

impl App {
    pub(crate) fn browser_visible(&self) -> bool {
        self.browser.as_ref().is_some_and(|b| b.visible)
    }

    pub(super) fn open_browser(&mut self) {
        let conversation = self.selected().id.clone();
        self.browser = Some(Browser {
            visible: true,
            conversation,
            snapshot: None,
            path: String::new(),
            entries: Vec::new(),
            selected: 0,
            parents: Vec::new(),
            content: String::new(),
            title: String::new(),
            scroll: 0,
            status: String::new(),
            request: 0,
        });
        self.palette = None;
        self.refresh_browser();
    }

    fn browser_job(
        &mut self,
        label: &str,
        job: impl FnOnce(&GitTransport) -> Result<Update, String> + Send + 'static,
    ) {
        let Some(b) = self.browser.as_mut() else {
            return;
        };
        b.status = label.into();
        let request = NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        b.request = request;
        spawn(self.repo_dir.clone(), self.tx.clone(), job, move |result| {
            UiMessage::Filesystem { request, result }
        });
    }

    fn refresh_browser(&mut self) {
        let Some(b) = &self.browser else { return };
        let id = b.conversation.clone();
        self.browser_job("Loading files…", move |t| {
            let snapshot = fs::snapshot(t, &id)?;
            let entries = fs::list(t, &snapshot.tree, "")?;
            Ok(Update::Snapshot(snapshot, entries))
        });
    }

    fn load_browser_rows(&mut self) {
        let Some(b) = &self.browser else { return };
        let Some(snapshot) = &b.snapshot else { return };
        let (tree, path) = (snapshot.tree.clone(), b.path.clone());
        self.browser_job("Loading files…", move |t| {
            Ok(Update::Rows(fs::list(t, &tree, &path)?))
        });
    }

    fn preview_browser_selection(&mut self) {
        let Some(b) = &mut self.browser else { return };
        let Some(snapshot) = &b.snapshot else { return };
        let path = b
            .entries
            .get(b.selected)
            .map(|e| e.path.clone())
            .unwrap_or_else(|| b.path.clone());
        let tree = snapshot.tree.clone();
        b.content.clear();
        b.title = path.clone();
        b.scroll = 0;
        self.browser_job("Loading preview…", move |t| {
            Ok(Update::Preview(fs::preview(t, &tree, &path)?))
        });
    }

    pub(super) fn browser_update(&mut self, request: u64, result: Result<Update, String>) {
        let Some(b) = &mut self.browser else { return };
        if b.request != request {
            return;
        }
        b.status.clear();
        match result {
            Err(error) => b.status = error,
            Ok(Update::Preview(preview)) => {
                b.content = preview.text;
                b.title = preview.title;
            }
            Ok(update) => {
                match update {
                    Update::Snapshot(snapshot, entries) => {
                        b.snapshot = Some(snapshot);
                        b.path.clear();
                        b.parents.clear();
                        b.selected = 0;
                        b.entries = entries;
                    }
                    Update::Rows(entries) => {
                        b.entries = entries;
                        b.selected = b.selected.min(b.entries.len().saturating_sub(1));
                    }
                    Update::Preview(_) => unreachable!(),
                }
                self.preview_browser_selection();
            }
        }
    }

    pub(super) fn handle_browser_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.selection_locked = true;
            return;
        }
        let Some(b) = &mut self.browser else { return };
        match key.code {
            KeyCode::Esc | KeyCode::Char('c')
                if key.code == KeyCode::Esc || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                b.visible = false
            }
            KeyCode::Up | KeyCode::Down => {
                if b.entries.is_empty() {
                    return;
                }
                b.selected = if key.code == KeyCode::Up {
                    b.selected.saturating_sub(1)
                } else {
                    (b.selected + 1).min(b.entries.len().saturating_sub(1))
                };
                self.preview_browser_selection();
            }
            KeyCode::Left | KeyCode::Backspace => {
                if b.path.is_empty() {
                    return;
                }
                b.path = b
                    .path
                    .rsplit_once('/')
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_default();
                b.selected = b.parents.pop().unwrap_or(0);
                b.entries.clear();
                self.load_browser_rows();
            }
            KeyCode::Right | KeyCode::Enter => {
                if let Some(entry) = b.entries.get(b.selected).filter(|e| e.directory) {
                    b.path = entry.path.clone();
                    b.parents.push(b.selected);
                    b.selected = 0;
                    b.entries.clear();
                    self.load_browser_rows();
                }
            }
            KeyCode::PageUp => b.scroll = b.scroll.saturating_sub(15),
            KeyCode::PageDown => b.scroll = b.scroll.saturating_add(15),
            KeyCode::Char('r') => self.refresh_browser(),
            // This selects a host-side checkout/publication target; it does not
            // change the conversation filesystem or agent execution.
            KeyCode::Char('o') => {
                let Some(entry) = b.entries.get(b.selected) else {
                    return;
                };
                let name = entry.path.clone();
                if self.selected().source_trees.iter().any(|s| s.name == name) {
                    let result = self.selected_mut().select_source_tree(&name);
                    self.browser.as_mut().unwrap().status = match result {
                        Ok(()) => format!("Selected {name} for checkout/publication."),
                        Err(error) => error,
                    };
                }
            }
            _ => {}
        }
    }

    pub(super) fn browser_mouse(&mut self, mouse: MouseEvent, area: Rect) -> MouseAction {
        let panes = panes(area);
        let Some(b) = &mut self.browser else {
            return MouseAction::Ignored;
        };
        let left = panes[0].contains((mouse.column, mouse.row).into());
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                if left {
                    self.handle_browser_key(KeyEvent::new(
                        if up { KeyCode::Up } else { KeyCode::Down },
                        KeyModifiers::NONE,
                    ));
                } else {
                    b.scroll = if up {
                        b.scroll.saturating_sub(3)
                    } else {
                        b.scroll.saturating_add(3)
                    };
                }
            }
            MouseEventKind::Down(MouseButton::Left) if left => {
                let row = mouse.row.saturating_sub(panes[0].y + 1) as usize;
                let selected = row + list_offset(b.selected, panes[0].height);
                if selected < b.entries.len() {
                    b.selected = selected;
                    self.preview_browser_selection();
                }
            }
            _ => return MouseAction::Ignored,
        }
        MouseAction::Redraw
    }
}

fn panes(area: Rect) -> std::rc::Rc<[Rect]> {
    let body = Rect::new(
        area.x,
        area.y + 2,
        area.width,
        area.height.saturating_sub(4),
    );
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
        .split(body)
}

fn list_offset(selected: usize, height: u16) -> usize {
    selected.saturating_sub(height.saturating_sub(3) as usize)
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
    let head = b
        .snapshot
        .as_ref()
        .map(|s| short_hash(&s.head))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!(
            "Files  /{}   ·   {} {head}",
            safe(&b.path),
            safe(&b.conversation)
        )),
        Rect::new(area.x, area.y, area.width, 2),
    );
    let panes = panes(area);
    let mut state = ListState::default()
        .with_selected((!b.entries.is_empty()).then_some(b.selected))
        .with_offset(list_offset(b.selected, panes[0].height));
    frame.render_stateful_widget(
        List::new(b.entries.iter().map(|e| {
            ListItem::new(format!(
                "{} {}{}{}",
                e.change,
                safe(&e.name),
                if e.directory { "/" } else { "" },
                e.commit
                    .as_ref()
                    .map(|c| format!(" {}", short_hash(c)))
                    .unwrap_or_default()
            ))
        }))
        .highlight_symbol("> ")
        .highlight_style(Style::default().fg(Color::Cyan))
        .block(Block::default().borders(Borders::ALL).title(" Files ")),
        panes[0],
        &mut state,
    );
    let content: Vec<_> = safe(&b.content)
        .lines()
        .map(|line| {
            let color = if line.starts_with('+') {
                Color::Green
            } else if line.starts_with('-') {
                Color::Red
            } else if line.starts_with("@@") {
                Color::Cyan
            } else {
                Color::Reset
            };
            Line::from(Span::styled(line.to_string(), Style::default().fg(color)))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(content)
            .wrap(Wrap { trim: false })
            .scroll((b.scroll, 0))
            .block(Block::default().borders(Borders::ALL).title(safe(&b.title))),
        panes[1],
    );
    frame.render_widget(
        Paragraph::new(safe(&b.status)),
        Rect::new(area.x, area.bottom().saturating_sub(2), area.width, 1),
    );
    frame.render_widget(Paragraph::new("Up/Down select  Right/Enter open  Left back  PgUp/Dn scroll  r refresh  o checkout/PR target  Esc close"),
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1));
}
