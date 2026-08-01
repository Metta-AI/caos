//! Terminal rendering.

use ratatui_core::buffer::{Buffer, CellWidth};
use ratatui_core::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::terminal::Frame;
use ratatui_core::text::{Line, Span};
use ratatui_core::widgets::Widget;
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::list::{List, ListItem, ListState};
use ratatui_widgets::paragraph::{Paragraph, Wrap};

use super::{
    short_hash, ActivityState, App, Command, ConversationState, EntryRole, Focus, TranscriptPoint,
    View, COMMANDS,
};
use caos::chat::TurnPhase;

pub(super) const ACTIVITY_INDICATORS: [&str; 4] = ["·", "✦", "✽", "✦"];

pub(crate) fn render(app: &App, frame: &mut Frame<'_>) {
    let state = app.selected();
    let areas = layout(state, app.view == View::Chat, frame.area());

    render_header(app, state, frame, areas.header);
    render_conversations(app, frame, areas.sidebar);
    match app.view {
        View::Chat => render_chat(
            state,
            app.animation_frame,
            app.focus() == Focus::Conversation,
            frame,
            areas.content,
        ),
        View::Activity => render_activity_browser(state, frame, areas.content),
        View::Diff => render_diff(state, frame, areas.content),
        View::Tools => render_tools(state, frame, areas.content),
        View::Help => render_help(app, frame, areas.content),
    }
    render_composer(
        state,
        app.view,
        !app.selection_locked && app.focus() == Focus::Conversation,
        frame,
        areas.composer,
    );
    render_footer(app, frame, areas.footer);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Areas {
    header: Rect,
    sidebar: Rect,
    content: Rect,
    composer: Rect,
    footer: Rect,
}

fn layout(state: &ConversationState, show_commands: bool, area: Rect) -> Areas {
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
    let composer_width = body[1].width.saturating_sub(2);
    let input_height = composer_visual_height(&state.composer, composer_width).clamp(1, 8) as u16;
    let command_height = if show_commands {
        state.composer.command_matches().len() as u16
    } else {
        0
    };
    let conversation = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(input_height + command_height + 2),
        ])
        .split(body[1]);
    Areas {
        header: outer[0],
        sidebar: body[0],
        content: conversation[0],
        composer: conversation[1],
        footer: outer[2],
    }
}

pub(super) fn content_contains(
    state: &ConversationState,
    area: Rect,
    column: u16,
    row: u16,
) -> bool {
    layout(state, false, area)
        .content
        .contains(Position::new(column, row))
}

fn conversation_list_offset(selected: usize, count: usize, height: u16) -> usize {
    let visible = (height as usize / 2).max(1);
    selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(count.saturating_sub(visible))
}

pub(super) fn conversation_at(app: &App, terminal: Rect, column: u16, row: u16) -> Option<usize> {
    let areas = layout(app.selected(), app.view == View::Chat, terminal);
    let inner = Block::default().borders(Borders::ALL).inner(areas.sidebar);
    let position = Position::new(column, row);
    if !inner.contains(position) {
        return None;
    }
    let offset = conversation_list_offset(app.selected, app.conversations.len(), inner.height);
    let index = offset + ((row - inner.y) / 2) as usize;
    (index < app.conversations.len()).then_some(index)
}

fn chat_areas(state: &ConversationState, area: Rect) -> (Rect, Option<Rect>) {
    if !state.running && !state.publishing {
        return (area, None);
    }
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);
    (split[0], Some(split[1]))
}

pub(super) fn transcript_contains(
    state: &ConversationState,
    terminal: Rect,
    column: u16,
    row: u16,
) -> bool {
    let (transcript, _) = chat_areas(state, layout(state, true, terminal).content);
    transcript.contains(Position::new(column, row))
}

fn render_header(app: &App, state: &ConversationState, frame: &mut Frame<'_>, area: Rect) {
    let operation = if state.running {
        "running"
    } else if state.publishing {
        "publishing"
    } else {
        "idle"
    };
    let view = if app.selection_locked {
        "selection lock"
    } else {
        match app.view {
            View::Chat => "chat",
            View::Activity => "activity",
            View::Diff => "diff",
            View::Tools => "tools",
            View::Help => "help",
        }
    };
    let running = app
        .conversations
        .iter()
        .filter(|conversation| conversation.running)
        .count();
    let header = Line::from(vec![
        Span::styled(" caos ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(format!("  {}  ", state.title)),
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
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("{mark} "), Style::default().fg(color)),
                    Span::raw(state.title.clone()),
                ]),
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        state.latest_message_preview(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ),
                ]),
            ])
        })
        .collect();
    let inner_height = Block::default().borders(Borders::ALL).inner(area).height;
    let offset = conversation_list_offset(app.selected, app.conversations.len(), inner_height);
    let mut selected = ListState::default()
        .with_offset(offset)
        .with_selected(Some(app.selected));
    let focused = app.focus() == Focus::List;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" Conversations ")
                    .border_style(border_style)
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

fn render_chat(
    state: &ConversationState,
    animation_frame: usize,
    focused: bool,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let (transcript, activity) = chat_areas(state, area);
    render_transcript(state, focused, frame, transcript);
    if let Some(activity) = activity {
        render_live_activity(state, animation_frame, frame, activity);
    }
}

fn render_live_activity(
    state: &ConversationState,
    animation_frame: usize,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let (verb, summary) = if state.publishing {
        ("Publishing", state.status.as_str())
    } else if let Some(activity) = state.running_activity() {
        (activity.running_verb(), activity.running_summary())
    } else {
        (
            match state.turn_phase {
                TurnPhase::System => "Chugging",
                TurnPhase::Model => "Thinking",
            },
            state.status.as_str(),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{} {verb}…", ACTIVITY_INDICATORS[animation_frame]),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {summary}"), Style::default().fg(Color::DarkGray)),
            Span::styled("  Ctrl+T expands", Style::default().fg(Color::DarkGray)),
        ]))
        .block(Block::default().title(" Activity ").borders(Borders::ALL)),
        area,
    );
}

fn render_transcript(state: &ConversationState, focused: bool, frame: &mut Frame<'_>, area: Rect) {
    let paragraph = transcript_paragraph(state);
    let scroll = paragraph_scroll(&paragraph, area, state.scroll_from_bottom);
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    frame.render_widget(
        paragraph
            .block(
                Block::default()
                    .title(" Conversation ")
                    .border_style(border_style)
                    .borders(Borders::ALL),
            )
            .scroll((scroll, 0)),
        area,
    );
    render_transcript_selection(state, frame, area);
}

fn transcript_paragraph(state: &ConversationState) -> Paragraph<'static> {
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
        lines.extend(entry.text.lines().map(inline_markdown_line));
        lines.push(Line::raw(""));
    }
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

fn inline_markdown_line(text: &str) -> Line<'static> {
    Line::from(inline_markdown_spans(text, Style::default()))
}

fn inline_markdown_spans(text: &str, style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut index = 0;

    while index < text.len() {
        let rest = &text[index..];
        if let Some(after_tick) = rest.strip_prefix('`') {
            if let Some(end) = after_tick.find('`') {
                let end = index + end + 2;
                plain.push_str(&text[index..end]);
                index = end;
                continue;
            }
        }
        if rest.starts_with("**")
            && text[index + 2..]
                .chars()
                .next()
                .is_some_and(|ch| !ch.is_whitespace())
        {
            if let Some(end) =
                find_closing_marker(text, index + 2, "**", |before, _| !before.is_whitespace())
            {
                push_plain(&mut spans, &mut plain, style);
                spans.extend(inline_markdown_spans(
                    &text[index + 2..end],
                    style.add_modifier(Modifier::BOLD),
                ));
                index = end + 2;
                continue;
            }
        }
        if rest.starts_with('_') && underscore_can_open(text, index) {
            if let Some(end) = find_closing_marker(text, index + 1, "_", |before, after| {
                !before.is_whitespace()
                    && before != '_'
                    && after.is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
            }) {
                push_plain(&mut spans, &mut plain, style);
                spans.extend(inline_markdown_spans(
                    &text[index + 1..end],
                    style.add_modifier(Modifier::ITALIC),
                ));
                index = end + 1;
                continue;
            }
        }

        let ch = rest.chars().next().expect("index is within text");
        plain.push(ch);
        index += ch.len_utf8();
    }

    push_plain(&mut spans, &mut plain, style);
    spans
}

fn find_closing_marker(
    text: &str,
    mut index: usize,
    marker: &str,
    can_close: impl Fn(char, Option<char>) -> bool,
) -> Option<usize> {
    let content_start = index;
    while index < text.len() {
        let rest = &text[index..];
        if let Some(after_tick) = rest.strip_prefix('`') {
            if let Some(end) = after_tick.find('`') {
                index += end + 2;
                continue;
            }
        }
        if index > content_start && rest.starts_with(marker) {
            let before = text[..index]
                .chars()
                .next_back()
                .expect("closing marker follows content");
            let after = text[index + marker.len()..].chars().next();
            if can_close(before, after) {
                return Some(index);
            }
        }
        let ch = rest.chars().next().expect("index is within text");
        index += ch.len_utf8();
    }
    None
}

fn underscore_can_open(text: &str, index: usize) -> bool {
    let before = text[..index].chars().next_back();
    let after = text[index + 1..].chars().next();
    before.is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
        && after.is_some_and(|ch| !ch.is_whitespace() && ch != '_')
}

fn push_plain(spans: &mut Vec<Span<'static>>, plain: &mut String, style: Style) {
    if !plain.is_empty() {
        spans.push(Span::styled(std::mem::take(plain), style));
    }
}

fn transcript_inner(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

fn transcript_scroll(state: &ConversationState, area: Rect) -> u16 {
    paragraph_scroll(&transcript_paragraph(state), area, state.scroll_from_bottom)
}

pub(super) fn transcript_point(
    state: &ConversationState,
    terminal: Rect,
    column: u16,
    row: u16,
) -> Option<TranscriptPoint> {
    let (area, _) = chat_areas(state, layout(state, true, terminal).content);
    let inner = transcript_inner(area);
    let position = Position::new(column, row);
    if !inner.contains(position) {
        return None;
    }
    let point = TranscriptPoint {
        row: row - inner.y,
        column: column - inner.x,
    };
    let absolute_row = transcript_scroll(state, area).saturating_add(point.row);
    let line_count = transcript_paragraph(state).line_count(inner.width);
    ((absolute_row as usize) < line_count).then_some(point)
}

pub(super) fn transcript_selection_text(
    state: &ConversationState,
    terminal: Rect,
) -> Option<String> {
    let selection = state.transcript_selection?;
    let area = layout(state, true, terminal).content;
    let inner = transcript_inner(area);
    if inner.is_empty() {
        return None;
    }
    let paragraph = transcript_paragraph(state);
    let line_count = paragraph.line_count(inner.width).min(u16::MAX as usize) as u16;
    let mut buffer = Buffer::empty(Rect::new(0, 0, inner.width, line_count));
    paragraph.render(buffer.area, &mut buffer);

    let scroll = transcript_scroll(state, area);
    let (start, end) = selection.ordered();
    let mut rows = Vec::new();
    for selected_row in start.row..=end.row {
        let absolute_row = scroll.saturating_add(selected_row);
        if absolute_row >= line_count {
            break;
        }
        let start_column = if selected_row == start.row {
            start.column
        } else {
            0
        };
        let end_column = if selected_row == end.row {
            end.column
        } else {
            inner.width.saturating_sub(1)
        };
        let mut text = String::new();
        for column in start_column..=end_column.min(inner.width.saturating_sub(1)) {
            if let Some(cell) = buffer.cell((column, absolute_row)) {
                text.push_str(cell.symbol());
            }
        }
        rows.push(text.trim_end().to_string());
    }
    let text = rows.join("\n");
    (!text.is_empty()).then_some(text)
}

fn render_transcript_selection(state: &ConversationState, frame: &mut Frame<'_>, area: Rect) {
    let Some(selection) = state.transcript_selection else {
        return;
    };
    let inner = transcript_inner(area);
    let (start, end) = selection.ordered();
    for row in start.row..=end.row.min(inner.height.saturating_sub(1)) {
        let start_column = if row == start.row { start.column } else { 0 };
        let end_column = if row == end.row {
            end.column
        } else {
            inner.width.saturating_sub(1)
        };
        for column in start_column..=end_column.min(inner.width.saturating_sub(1)) {
            if let Some(cell) = frame
                .buffer_mut()
                .cell_mut((inner.x + column, inner.y + row))
            {
                cell.set_fg(Color::Black).set_bg(Color::Cyan);
            }
        }
    }
}

fn render_activity_browser(state: &ConversationState, frame: &mut Frame<'_>, area: Rect) {
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);
    let items: Vec<ListItem<'_>> = state
        .activities
        .iter()
        .map(|activity| {
            let (mark, color) = activity_mark(activity.state);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(color)),
                Span::styled(
                    format!("{}  ", short_hash(&activity.step_commit)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(activity.summary.clone()),
            ]))
        })
        .collect();
    let mut selection = ListState::default().with_selected(state.activity_selection);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(" Activity — {} ", state.status))
                    .borders(Borders::ALL),
            )
            .highlight_symbol("> ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        panes[0],
        &mut selection,
    );

    let lines = state
        .activity_selection
        .and_then(|selected| state.activities.get(selected))
        .map(|activity| {
            let (mark, color) = activity_mark(activity.state);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(format!("{mark} "), Style::default().fg(color)),
                    Span::styled(
                        short_hash(&activity.step_commit).to_string(),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(format!("  {}", activity.summary)),
                ]),
                Line::raw(""),
            ];
            if activity.detail.is_empty() {
                lines.push(Line::styled(
                    "Waiting for the tool result.",
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                lines.extend(
                    activity
                        .detail
                        .lines()
                        .map(|line| Line::raw(line.to_string())),
                );
            }
            lines
        })
        .unwrap_or_else(|| {
            vec![Line::styled(
                "No activity for this turn.",
                Style::default().fg(Color::DarkGray),
            )]
        });
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let line_count = paragraph.line_count(panes[1].width.saturating_sub(2));
    let visible = panes[1].height.saturating_sub(2) as usize;
    let max_scroll = line_count.saturating_sub(visible);
    let scroll = state
        .activity_detail_scroll
        .min(max_scroll)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(
        paragraph
            .block(
                Block::default()
                    .title(" Detail (PgUp/PgDn or wheel; Esc returns) ")
                    .borders(Borders::ALL),
            )
            .scroll((scroll, 0)),
        panes[1],
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

fn render_help(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let send_shortcut = if app.enhanced_keyboard() {
        "Ctrl+Enter"
    } else {
        "Ctrl+S"
    };
    let mut lines = vec![
        Line::styled(
            "Keyboard shortcuts",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw("  Ctrl+H          toggle this help"),
        Line::raw(format!("  {send_shortcut:<16}send the prompt")),
        Line::raw("  Enter/Ctrl+J    insert a newline"),
        Line::raw("  Ctrl+A/Ctrl+E   move to the start/end of the line"),
        Line::raw("  Ctrl+W          delete the previous word"),
        Line::raw("  Ctrl+K          delete to the end of the line"),
        Line::raw("  Ctrl+L          check out the conversation commit locally"),
        Line::raw("  Ctrl+P twice    publish a clean branch and open a PR"),
        Line::raw("  Ctrl+N          start a new conversation"),
        Line::raw("  Esc             focus the conversation list"),
        Line::raw("  Ctrl+E          archive from the conversation list"),
        Line::raw("  Ctrl+Up/Down    switch conversations"),
        Line::raw("  Ctrl+T          toggle activity details"),
        Line::raw("  Ctrl+Shift+T    toggle available tools"),
        Line::raw("  Ctrl+Q          toggle the workspace diff"),
        Line::raw("  Ctrl+Y          pause redraws for native terminal selection"),
        Line::raw("  Ctrl+C          clear the prompt, then quit"),
        Line::raw(""),
        Line::styled(
            "Slash commands",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    lines.extend(
        COMMANDS
            .iter()
            .map(|command| Line::raw(format!("  {:<24} {}", command.usage, command.description))),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(" Help ").borders(Borders::ALL)),
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
    show_cursor: bool,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let commands = if view == View::Chat {
        state.composer.command_matches()
    } else {
        Vec::new()
    };
    let block = Block::default().borders(Borders::TOP | Borders::BOTTOM);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let command_height = commands.len().min(inner.height as usize) as u16;
    let composer_height = inner.height.saturating_sub(command_height);
    let composer_area = Rect::new(
        inner.x.saturating_add(2),
        inner.y,
        inner.width.saturating_sub(2),
        composer_height,
    );
    let command_area = Rect::new(
        inner.x.saturating_add(2),
        inner.y.saturating_add(composer_height),
        inner.width.saturating_sub(2),
        command_height,
    );
    let (row, column) = composer_cursor(&state.composer, composer_area.width);
    let inner_height = composer_height as usize;
    let vertical_scroll = row.saturating_sub(inner_height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(composer_lines(&state.composer, composer_area.width))
            .scroll((vertical_scroll.min(u16::MAX as usize) as u16, 0)),
        composer_area,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            ">",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Rect::new(inner.x, inner.y, 1, 1),
    );
    render_command_menu(
        &commands,
        state.composer.command_selection,
        frame,
        command_area,
    );
    if view == View::Chat && show_cursor {
        let cursor_row = row.saturating_sub(vertical_scroll);
        let x = composer_area.x.saturating_add(column as u16);
        let y = composer_area.y.saturating_add(cursor_row as u16);
        if x < composer_area.right() && y < composer_area.bottom() {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

fn composer_visual_ranges(text: &str, width: u16) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut ranges = Vec::new();
    let mut logical_start = 0;
    loop {
        let logical_end = text[logical_start..]
            .find('\n')
            .map(|offset| logical_start + offset)
            .unwrap_or(text.len());
        let mut visual_start = logical_start;
        let mut cells: u16 = 0;
        for (offset, ch) in text[logical_start..logical_end].char_indices() {
            let index = logical_start + offset;
            let char_width = ch.to_string().cell_width();
            if cells > 0 && cells.saturating_add(char_width) > width {
                ranges.push((visual_start, index));
                visual_start = index;
                cells = 0;
            }
            cells = cells.saturating_add(char_width);
        }
        ranges.push((visual_start, logical_end));
        if logical_end == text.len() {
            break;
        }
        logical_start = logical_end + 1;
    }
    ranges
}

fn composer_visual_height(composer: &super::Composer, width: u16) -> usize {
    let ranges = composer_visual_ranges(&composer.text, width);
    let (row, _) = composer_cursor(composer, width);
    ranges.len().max(row + 1)
}

fn composer_cursor(composer: &super::Composer, width: u16) -> (usize, usize) {
    let width = width.max(1);
    let ranges = composer_visual_ranges(&composer.text, width);
    let row = ranges
        .iter()
        .rposition(|(start, end)| composer.cursor >= *start && composer.cursor <= *end)
        .expect("the composer cursor is within its text");
    let column = composer.text[ranges[row].0..composer.cursor].cell_width() as usize;
    if column >= width as usize {
        (row + 1, 0)
    } else {
        (row, column)
    }
}

fn composer_lines(composer: &super::Composer, width: u16) -> Vec<Line<'_>> {
    let selection = composer.selection_range();
    let selection_style = Style::default().fg(Color::Black).bg(Color::Cyan);
    composer_visual_ranges(&composer.text, width)
        .into_iter()
        .map(|(line_start, line_end)| {
            let line = &composer.text[line_start..line_end];
            let Some((selection_start, selection_end)) = selection else {
                return Line::raw(line);
            };
            let selected_start = selection_start.clamp(line_start, line_end);
            let selected_end = selection_end.clamp(line_start, line_end);
            if selected_start >= selected_end {
                return Line::raw(line);
            }
            Line::from(vec![
                Span::raw(&composer.text[line_start..selected_start]),
                Span::styled(
                    &composer.text[selected_start..selected_end],
                    selection_style,
                ),
                Span::raw(&composer.text[selected_end..line_end]),
            ])
        })
        .collect()
}

fn render_command_menu(commands: &[&Command], selected: usize, frame: &mut Frame<'_>, area: Rect) {
    let lines = commands.iter().enumerate().map(|(index, command)| {
        let marker = if index == selected { "> " } else { "  " };
        let style = if index == selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Line::styled(
            format!("{marker}{} — {}", command.usage, command.description),
            style,
        )
    });
    frame.render_widget(Paragraph::new(lines.collect::<Vec<_>>()), area);
}

fn render_footer(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let footer = if app.selection_locked {
        Line::styled(
            " Selection lock: redraws paused, ^Y/Esc resumes",
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )
    } else if app.focus() == Focus::List {
        Line::raw(
            " Conversations: Up/Dn select  Enter opens  ^N new  ^E archive  ^Up/Dn switch  ^C quit",
        )
    } else if app.view == View::Activity {
        Line::raw(
            " Activity: Up/Dn select  PgUp/PgDn/wheel detail  ^T/Esc return  ^Up/Dn chat  ^C quit",
        )
    } else if app.view == View::Help {
        Line::raw(" Help: Ctrl+H/Esc returns  ^C quit")
    } else {
        let send_shortcut = if app.enhanced_keyboard() {
            "^Enter"
        } else {
            "^S"
        };
        Line::raw(format!(
            " {send_shortcut} send  Enter/^J newline  ^L checkout  ^Q changes  ^T activity  ^H help  Esc list  ^C quit"
        ))
    };
    frame.render_widget(Paragraph::new(footer), area);
    if let Some(chars) = app.copied_chars {
        let noun = if chars == 1 { "char" } else { "chars" };
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" {chars} {noun} copied "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Right),
            area,
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_markdown_styles_bold_italic_and_nested_emphasis() {
        assert_eq!(
            inline_markdown_line("plain **bold _and italic_**"),
            Line::from(vec![
                Span::raw("plain "),
                Span::styled("bold ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    "and italic",
                    Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ),
            ])
        );
    }

    #[test]
    fn inline_markdown_preserves_unmatched_and_intraword_markers() {
        assert_eq!(
            inline_markdown_line("**open _still open snake_case __literal__"),
            Line::raw("**open _still open snake_case __literal__")
        );
    }

    #[test]
    fn inline_markdown_does_not_parse_markers_inside_backticks() {
        assert_eq!(
            inline_markdown_line("`**not bold** _not italic_` and **bold**"),
            Line::from(vec![
                Span::raw("`**not bold** _not italic_` and "),
                Span::styled("bold", Style::default().add_modifier(Modifier::BOLD)),
            ])
        );
    }

    #[test]
    fn composer_selection_is_highlighted() {
        let mut composer = super::super::Composer::default();
        composer.insert_str("one two");
        composer.select_word_left();

        let lines = composer_lines(&composer, 80);

        assert_eq!(lines[0].spans[1].content, "two");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Black));
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Cyan));
    }

    #[test]
    fn composer_soft_wraps_and_places_the_cursor_on_visual_rows() {
        let mut composer = super::super::Composer::default();
        composer.insert_str("abcdefghij");

        let lines = composer_lines(&composer, 4);

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].content, "abcd");
        assert_eq!(lines[1].spans[0].content, "efgh");
        assert_eq!(lines[2].spans[0].content, "ij");
        assert_eq!(composer_cursor(&composer, 4), (2, 2));
        assert_eq!(composer_visual_height(&composer, 4), 3);
    }

    #[test]
    fn composer_wrap_counts_terminal_cell_width() {
        let mut composer = super::super::Composer::default();
        composer.insert_str("ab界c");

        let lines = composer_lines(&composer, 4);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content, "ab界");
        assert_eq!(lines[1].spans[0].content, "c");
        assert_eq!(composer_cursor(&composer, 4), (1, 1));
    }
}
