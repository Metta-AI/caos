//! Read-only navigation with automatic file and source-boundary previews.
use super::*;
use caos_cli::filesystem::{self as fs, Entry, SnapshotInfo};
use ratatui_core::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
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
    entries: Vec<Row>,
    selected: usize,
    selection_path: Option<String>,
    content: String,
    title: String,
    scroll: u16,
    status: String,
    request: u64,
}

pub(super) enum Update {
    Snapshot(SnapshotInfo, Vec<Row>),
    Rows(Vec<Row>),
    Preview(fs::Preview),
}

pub(super) struct Row {
    entry: Entry,
    depth: u8,
}

/// Expand just the immediate directories, including commit-valued source trees.
/// Children remain selectable and retain their full paths for previews/navigation.
fn load_rows(t: &GitTransport, tree: &str, path: &str) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    for entry in fs::list(t, tree, path)? {
        let children = if entry.directory {
            fs::list(t, tree, &entry.path)?
        } else {
            Vec::new()
        };
        rows.push(Row { entry, depth: 0 });
        rows.extend(children.into_iter().map(|entry| Row { entry, depth: 1 }));
    }
    Ok(rows)
}

fn row_item(row: &Row) -> ListItem<'static> {
    let entry = &row.entry;
    let color = if entry.commit.is_some() {
        Color::Magenta
    } else if entry.directory {
        Color::Cyan
    } else {
        Color::Reset
    };
    let change_color = match entry.change {
        '+' => Color::Green,
        '-' => Color::Red,
        '~' => Color::Yellow,
        _ => Color::Reset,
    };
    // Each row must occupy one line so mouse selection matches the visible list.
    let name = safe(&entry.name).replace('\n', "\\n").replace('\t', "\\t");
    ListItem::new(Line::from(vec![
        Span::raw("  ".repeat(row.depth as usize)),
        Span::styled(
            format!("{} ", entry.change),
            Style::default().fg(change_color),
        ),
        Span::styled(
            format!("{name}{}", if entry.directory { "/" } else { "" }),
            Style::default().fg(color),
        ),
        Span::styled(
            entry
                .commit
                .as_ref()
                .map(|c| format!(" {}", short_hash(c)))
                .unwrap_or_default(),
            Style::default().fg(color),
        ),
    ]))
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
            selection_path: None,
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
            let entries = load_rows(t, &snapshot.tree, "")?;
            Ok(Update::Snapshot(snapshot, entries))
        });
    }

    fn load_browser_rows(&mut self) {
        let Some(b) = &self.browser else { return };
        let Some(snapshot) = &b.snapshot else { return };
        let (tree, path) = (snapshot.tree.clone(), b.path.clone());
        self.browser_job("Loading files…", move |t| {
            Ok(Update::Rows(load_rows(t, &tree, &path)?))
        });
    }

    fn preview_browser_selection(&mut self) {
        let Some(b) = &mut self.browser else { return };
        let Some(snapshot) = &b.snapshot else { return };
        let path = b
            .entries
            .get(b.selected)
            .map(|row| row.entry.path.clone())
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
                        b.selection_path = None;
                        b.selected = 0;
                        b.entries = entries;
                    }
                    Update::Rows(entries) => {
                        b.entries = entries;
                        b.selected = b
                            .selection_path
                            .take()
                            .and_then(|path| {
                                b.entries.iter().position(|row| row.entry.path == path)
                            })
                            .unwrap_or_else(|| b.selected.min(b.entries.len().saturating_sub(1)));
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
                b.selection_path = Some(b.path.clone());
                b.path = b
                    .path
                    .rsplit_once('/')
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_default();
                b.selected = 0;
                b.entries.clear();
                self.load_browser_rows();
            }
            KeyCode::Right | KeyCode::Enter => {
                if let Some(row) = b.entries.get(b.selected).filter(|row| row.entry.directory) {
                    b.path = row.entry.path.clone();
                    b.selection_path = None;
                    b.selected = 0;
                    b.entries.clear();
                    self.load_browser_rows();
                }
            }
            KeyCode::PageUp => b.scroll = b.scroll.saturating_sub(15),
            KeyCode::PageDown => b.scroll = b.scroll.saturating_add(15),
            KeyCode::Char('r') => self.refresh_browser(),
            // This selects the source for tool descriptions and local edits; it does not
            // change the conversation filesystem or agent execution.
            KeyCode::Char('o') => {
                let Some(entry) = b.entries.get(b.selected) else {
                    return;
                };
                let name = entry.entry.path.clone();
                if self.selected().source_trees.iter().any(|s| s.name == name) {
                    let result = self.selected_mut().select_source_tree(&name);
                    self.browser.as_mut().unwrap().status = match result {
                        Ok(()) => format!("Selected {name} for tools and local edits."),
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
        Paragraph::new(vec![
            Line::raw(format!(
                "Files  /{}   ·   {} {head}",
                safe(&b.path),
                safe(&b.conversation)
            )),
            Line::from(vec![
                Span::styled("Gitlinks", Style::default().fg(Color::Magenta)),
                Span::raw(" · indented entries show one more level"),
            ]),
        ]),
        Rect::new(area.x, area.y, area.width, 2),
    );
    let panes = panes(area);
    let mut state = ListState::default()
        .with_selected((!b.entries.is_empty()).then_some(b.selected))
        .with_offset(list_offset(b.selected, panes[0].height));
    frame.render_stateful_widget(
        List::new(b.entries.iter().map(row_item))
            .highlight_symbol("> ")
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
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
    frame.render_widget(Paragraph::new("Up/Down select  Right/Enter open  Left back  PgUp/Dn scroll  r refresh  o select source  Esc close"),
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1));
}

#[cfg(test)]
mod tests {
    use super::super::tests::{app_with, state};
    use super::*;
    use ratatui_core::{backend::TestBackend, terminal::Terminal};
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn row(path: &str, directory: bool, gitlink: bool, depth: u8) -> Row {
        Row {
            entry: Entry {
                path: path.into(),
                name: path.rsplit('/').next().unwrap().into(),
                directory,
                commit: gitlink.then(|| "a".repeat(40)),
                change: ' ',
            },
            depth,
        }
    }

    fn browser(entries: Vec<Row>) -> Browser {
        Browser {
            visible: true,
            conversation: "browser-test".into(),
            snapshot: None,
            path: String::new(),
            entries,
            selected: 0,
            selection_path: None,
            content: String::new(),
            title: String::new(),
            scroll: 0,
            status: String::new(),
            request: 0,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn indented_directory_navigation_returns_to_actual_parent() {
        let (mut app, _) = app_with(vec![state("browser-test")]);
        app.browser = Some(browser(vec![
            row("source", true, true, 0),
            row("source/src", true, false, 1),
        ]));
        app.handle_browser_key(key(KeyCode::Down));
        app.handle_browser_key(key(KeyCode::Enter));
        assert_eq!(app.browser.as_ref().unwrap().path, "source/src");
        app.handle_browser_key(key(KeyCode::Left));
        assert_eq!(app.browser.as_ref().unwrap().path, "source");
        app.browser_update(
            0,
            Ok(Update::Rows(vec![
                row("source/z", false, false, 0),
                row("source/src", true, false, 0),
            ])),
        );
        assert_eq!(app.browser.as_ref().unwrap().selected, 1);
        app.handle_browser_key(key(KeyCode::Left));
        assert_eq!(app.browser.as_ref().unwrap().path, "");
        app.browser_update(0, Ok(Update::Rows(vec![row("source", true, true, 0)])));
        assert_eq!(app.browser.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn gitlink_color_survives_selection_and_mouse_selects_indented_rows() {
        let (mut app, _) = app_with(vec![state("browser-test")]);
        app.browser = Some(browser(vec![
            row("source", true, true, 0),
            row("source/src", true, false, 1),
            row("source/a\nb", false, false, 1),
            row("plain", false, false, 0),
        ]));
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(&app, f)).unwrap();
        let buffer = terminal.backend().buffer();
        let find = |name: &str, y: u16| {
            let line: String = (0..140).map(|x| buffer[(x, y)].symbol()).collect();
            line.find(name).unwrap() as u16
        };
        assert_eq!(buffer[(find("source/", 3), 3)].fg, Color::Magenta);
        assert_eq!(buffer[(find("src/", 4), 4)].fg, Color::Cyan);
        assert!(find("a\\nb", 5) > find("source/", 3));
        assert!(find("plain", 6) > 0);
        app.browser_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 10,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            Rect::new(0, 0, 140, 30),
        );
        assert_eq!(app.browser.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn browser_lists_exactly_one_extra_level_in_directories_and_gitlinks() {
        // Real tree objects exercise commit traversal and listing order without a server.
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str], input: &str| {
            let mut child = Command::new("git")
                .current_dir(repo.path())
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"], "");
        let blob = git(&["hash-object", "-w", "--stdin"], "hello\n");
        let deep = git(&["mktree"], &format!("100644 blob {blob}\ttoo-deep\n"));
        let child = git(
            &["mktree"],
            &format!("040000 tree {deep}\tnested\n100644 blob {blob}\tfile\n"),
        );
        let commit = git(&["commit-tree", &child], "source\n");
        let root = git(&["mktree"], &format!("040000 tree {child}\tdir\n160000 commit {commit}\tsource\n100644 blob {blob}\treadme\n"));
        let transport = GitTransport::discover(repo.path()).unwrap();
        let rows = load_rows(&transport, &root, "").unwrap();
        let paths: Vec<_> = rows
            .iter()
            .map(|r| (r.entry.path.as_str(), r.depth))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("source", 0),
                ("source/nested", 1),
                ("source/file", 1),
                ("dir", 0),
                ("dir/nested", 1),
                ("dir/file", 1),
                ("readme", 0),
            ]
        );
        assert_eq!(rows[0].entry.commit.as_deref(), Some(commit.as_str()));
        assert!(rows[3].entry.commit.is_none());
        let rows = load_rows(&transport, &root, "source").unwrap();
        assert!(rows
            .iter()
            .any(|r| r.entry.path == "source/nested/too-deep" && r.depth == 1));
        assert_eq!(
            fs::preview(&transport, &root, "source/file").unwrap().text,
            "hello\n"
        );
    }
}
