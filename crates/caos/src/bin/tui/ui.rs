//! Terminal rendering.

use ratatui_core::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::terminal::Frame;
use ratatui_core::text::{Line, Span};
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::list::{List, ListItem, ListState};
use ratatui_widgets::paragraph::{Paragraph, Wrap};

use super::{short_hash, ActivityState, App, ConversationState, EntryRole, View};

pub(crate) fn render(app: &App, frame: &mut Frame<'_>) {
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
        View::Tools => render_tools(state, frame, conversation[0]),
    }
    render_activity(state, app.activity_expanded, frame, conversation[1]);
    render_composer(
        state,
        app.view,
        app.capabilities,
        !app.copy_mode,
        frame,
        conversation[2],
    );
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
    } else {
        match app.view {
            View::Chat => "chat",
            View::Diff => "diff",
            View::Tools => "tools",
        }
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

fn render_tools(state: &ConversationState, frame: &mut Frame<'_>, area: Rect) {
    let mut lines = vec![
        Line::styled(
            "Always available",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw("  read, ls, write, edit  — inline workspace operations"),
        Line::raw("  bash                  — commands in the workspace sandbox"),
        Line::raw("  grep                  — cached regular-expression search"),
        Line::raw(""),
        Line::styled(
            "Project tools",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    match &state.tool_set {
        None => lines.push(Line::styled(
            "  Tool metadata has not been loaded.",
            Style::default().fg(Color::DarkGray),
        )),
        Some(Err(error)) => lines.push(Line::styled(
            format!("  Unable to load tools: {error}"),
            Style::default().fg(Color::Red),
        )),
        Some(Ok(set)) => {
            lines.push(Line::from(vec![
                Span::styled("  source  ", Style::default().fg(Color::DarkGray)),
                Span::raw(set.source.clone()),
            ]));
            if set.tools.is_empty() {
                lines.push(Line::styled(
                    "  No additional tools.",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            for tool in &set.tools {
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}", tool.name),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  [{}]", tool_image_label(&tool.image)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                lines.extend(
                    tool.docs
                        .lines()
                        .map(|line| Line::raw(format!("    {line}"))),
                );
            }
        }
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let scroll = paragraph_scroll(&paragraph, area, state.scroll_from_bottom);
    frame.render_widget(
        paragraph
            .block(
                Block::default()
                    .title(" Tools (Ctrl+T returns) ")
                    .borders(Borders::ALL),
            )
            .scroll((scroll, 0)),
        area,
    );
}

fn tool_image_label(image: &str) -> &str {
    if image.len() >= 40 && image.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        short_hash(image)
    } else {
        image
    }
}

fn render_composer(
    state: &ConversationState,
    view: View,
    capabilities: super::super::backend::BackendCapabilities,
    show_cursor: bool,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let title = if state.running && !capabilities.cancellation {
        " Prompt (turn running; cancellation is not available) "
    } else if state.running {
        " Prompt (turn running) "
    } else if state.publishing {
        " Prompt (publishing PR) "
    } else if view == View::Tools {
        " Prompt (tool view; Ctrl+T returns) "
    } else if view == View::Diff {
        " Prompt (changes view; Ctrl+Q returns) "
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
        Line::raw(" ^Up/Dn chat  ^N new  ^W close  ^Q diff  ^T tools  ^A activity  ^L load  ^P PR  ^Y copy  ^C quit")
    };
    frame.render_widget(Paragraph::new(footer), area);
}

pub(crate) fn paragraph_scroll(paragraph: &Paragraph<'_>, area: Rect, from_bottom: usize) -> u16 {
    let line_count = paragraph.line_count(area.width.saturating_sub(2));
    scroll_offset(line_count, area.height, from_bottom)
}

pub(crate) fn scroll_offset(line_count: usize, height: u16, from_bottom: usize) -> u16 {
    let visible = height.saturating_sub(2) as usize;
    line_count
        .saturating_sub(visible)
        .saturating_sub(from_bottom)
        .min(u16::MAX as usize) as u16
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
