//! Terminal rendering.

use ratatui_core::buffer::{Buffer, CellWidth};
use ratatui_core::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::terminal::Frame;
use ratatui_core::text::{Line, Span};
use ratatui_core::widgets::Widget;
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::clear::Clear;
use ratatui_widgets::list::{List, ListItem, ListState};
use ratatui_widgets::paragraph::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    short_hash, ActivityState, App, Command, ConfirmAction, ConversationState, EntryRole, Focus,
    ScrollState, TranscriptPoint, View, COMMANDS,
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
    if let Some(notice) = areas.notice {
        render_notice(app, state, frame, notice);
    }
    render_composer(
        state,
        app.view,
        !app.selection_locked && app.palette.is_none() && app.focus() == Focus::Conversation,
        frame,
        areas.composer,
    );
    render_footer(app, frame, areas.footer);
    render_command_palette(app, frame);
    render_screen_selection(app, frame);
}

fn render_command_palette(app: &App, frame: &mut Frame<'_>) {
    let Some(palette) = app.palette.as_ref() else {
        return;
    };
    let matches = palette.matches();
    let terminal = frame.area();
    let width = terminal.width.saturating_sub(4).clamp(20, 72);
    let height = (matches.len() as u16 + 4)
        .min(terminal.height.saturating_sub(2))
        .max(5);
    let area = terminal.centered(Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Command palette ")
        .border_style(Style::default().fg(Color::Cyan))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(palette.query.clone()),
        ])),
        rows[0],
    );

    if matches.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching commands").style(Style::default().fg(Color::DarkGray)),
            rows[1],
        );
    } else {
        let label_width = rows[1].width.saturating_sub(24) as usize;
        let items = matches
            .iter()
            .map(|command| {
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{:<label_width$}", command.label)),
                    Span::styled(command.shortcut, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect::<Vec<_>>();
        let mut selected = ListState::default().with_selected(Some(palette.selected));
        frame.render_stateful_widget(
            List::new(items).highlight_symbol("> ").highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            rows[1],
            &mut selected,
        );
    }
    let cursor_x = rows[0]
        .x
        .saturating_add(2)
        .saturating_add(palette.query.cell_width())
        .min(rows[0].right().saturating_sub(1));
    frame.set_cursor_position(Position::new(cursor_x, rows[0].y));
}

fn render_screen_selection(app: &App, frame: &mut Frame<'_>) {
    let Some(selection) = app.screen_selection else {
        return;
    };
    let area = frame.area();
    let (start, end) = selection.ordered();
    for row in start.row..=end.row.min(area.bottom().saturating_sub(1)) {
        let start_column = if row == start.row {
            start.column
        } else {
            area.x
        };
        let end_column = if row == end.row {
            end.column
        } else {
            area.right().saturating_sub(1)
        };
        for column in start_column..=end_column.min(area.right().saturating_sub(1)) {
            if let Some(cell) = frame.buffer_mut().cell_mut((column, row)) {
                cell.set_fg(Color::Black).set_bg(Color::Cyan);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Areas {
    header: Rect,
    sidebar: Rect,
    content: Rect,
    notice: Option<Rect>,
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
        state.composer.completion_count() as u16
    } else {
        0
    };
    let notice_height = if state.command_error.is_some() || state.publish_prompt {
        3
    } else if state.reference_notice.is_some() {
        4
    } else {
        0
    };
    let conversation = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(notice_height),
            Constraint::Length(input_height + command_height + 2),
        ])
        .split(body[1]);
    Areas {
        header: outer[0],
        sidebar: body[0],
        content: conversation[0],
        notice: (notice_height > 0).then_some(conversation[1]),
        composer: conversation[2],
        footer: outer[2],
    }
}

fn render_notice(app: &App, state: &ConversationState, frame: &mut Frame<'_>, area: Rect) {
    if let Some(error) = state.command_error.as_deref() {
        frame.render_widget(
            Paragraph::new(error)
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(" Command error ")
                        .border_style(Style::default().fg(Color::Red))
                        .borders(Borders::ALL),
                ),
            area,
        );
        return;
    }
    if let Some(ConfirmAction::Publish {
        default_base,
        base_input,
    }) = app.confirm_action.as_ref()
    {
        let branch = if base_input.is_empty() {
            Span::styled(
                format!("{default_base} (default)"),
                Style::default().fg(Color::DarkGray),
            )
        } else {
            Span::styled(base_input.clone(), Style::default().fg(Color::Cyan))
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Base branch: ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                branch,
                Span::styled("│", Style::default().fg(Color::Cyan)),
            ]))
            .block(
                Block::default()
                    .title(" Publish PR — type a base, Ctrl+P confirms, Esc cancels ")
                    .border_style(Style::default().fg(Color::Cyan))
                    .borders(Borders::ALL),
            ),
            area,
        );
        return;
    }
    if let Some(reference) = state.reference_notice.as_ref() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Ref:  ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(reference.refname.clone()),
                ]),
                Line::from(vec![
                    Span::styled("Head: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(reference.head.clone()),
                ]),
            ])
            .block(
                Block::default()
                    .title(" Conversation reference — click a row to copy ")
                    .border_style(Style::default().fg(Color::Cyan))
                    .borders(Borders::ALL),
            ),
            area,
        );
    }
}

pub(super) fn reference_copy_at(
    app: &App,
    terminal: Rect,
    column: u16,
    row: u16,
) -> Option<String> {
    let state = app.selected();
    if state.command_error.is_some() || state.publish_prompt || app.palette.is_some() {
        return None;
    }
    let reference = state.reference_notice.as_ref()?;
    let notice = layout(state, app.view == View::Chat, terminal).notice?;
    let inner = Block::default().borders(Borders::ALL).inner(notice);
    let position = Position::new(column, row);
    if !inner.contains(position) {
        return None;
    }
    match row.saturating_sub(inner.y) {
        0 => Some(reference.refname.clone()),
        1 => Some(reference.head.clone()),
        _ => None,
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
    let active = app
        .conversations
        .iter()
        .filter(|conversation| conversation.is_busy())
        .count();
    let left = Line::from(vec![
        Span::styled(" caos ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::styled(
            format!("  user {}", app.user),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("  {}", state.title),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]);
    let mut metadata = Vec::new();
    let mut push_metadata = |text: String, style: Style| {
        if !metadata.is_empty() {
            metadata.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        }
        metadata.push(Span::styled(text, style));
    };
    if app.selection_locked {
        push_metadata(
            "selection lock".to_string(),
            Style::default().fg(Color::Cyan),
        );
    } else if app.view != View::Chat {
        push_metadata(
            match app.view {
                View::Activity => "activity",
                View::Diff => "changes",
                View::Tools => "tools",
                View::Help => "help",
                View::Chat => unreachable!("chat is omitted from the header"),
            }
            .to_string(),
            Style::default().fg(Color::Cyan),
        );
    }
    if state.running || state.publishing {
        push_metadata(
            if state.publishing {
                "publishing"
            } else {
                "running"
            }
            .to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }
    push_metadata(
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
    );
    if active > 0 {
        push_metadata(
            format!("{active} active"),
            Style::default().fg(Color::DarkGray),
        );
    }
    let metadata_width = metadata
        .iter()
        .map(|span| span.content.cell_width())
        .sum::<u16>()
        .saturating_add(1)
        .min(area.width.saturating_sub(12));
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(12), Constraint::Length(metadata_width)])
        .split(area);
    frame.render_widget(Paragraph::new(left), columns[0]);
    frame.render_widget(
        Paragraph::new(Line::from(metadata).right_aligned()),
        columns[1],
    );
}

fn render_conversations(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let inner = Block::default().borders(Borders::ALL).inner(area);
    // The list's two-cell highlight symbol and our two-cell status prefix are
    // both inside the border, even for unselected rows.
    let detail_width = inner.width.saturating_sub(4);
    let items: Vec<ListItem<'_>> = app
        .conversations
        .iter()
        .map(|state| {
            let is_child = state.parent.as_ref().is_some_and(|parent| {
                app.conversations
                    .iter()
                    .any(|candidate| candidate.id == *parent)
            });
            let indent = if is_child { "  " } else { "" };
            let (mark, color) = if state.running {
                ("*", Color::Yellow)
            } else if state.generating_title {
                ("~", Color::Magenta)
            } else if state.publishing {
                ("^", Color::Cyan)
            } else {
                (" ", Color::DarkGray)
            };
            let (title, detail) = state.sidebar_text(
                detail_width.saturating_sub(u16::try_from(indent.len()).unwrap_or(u16::MAX)),
            );
            ListItem::new(vec![
                Line::from(vec![
                    Span::raw(indent),
                    Span::styled(format!("{mark} "), Style::default().fg(color)),
                    Span::raw(title),
                ]),
                Line::from(vec![
                    Span::raw(format!("{indent}  ")),
                    Span::styled(
                        detail,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ),
                ]),
            ])
        })
        .collect();
    let inner_height = inner.height;
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
    let paragraph = transcript_paragraph(state, transcript_inner(area).width);
    let scroll = paragraph_scroll(&paragraph, area, &state.scroll);
    let rows_below = state
        .scroll
        .rendered_max
        .get()
        .saturating_sub(scroll as usize);
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let mut block = Block::default()
        .title(" Conversation ")
        .border_style(border_style)
        .borders(Borders::ALL);
    if rows_below > 0 {
        let noun = if rows_below == 1 { "line" } else { "lines" };
        let label = if state.unread_below {
            format!(" New message · {rows_below} {noun} below ↓ ")
        } else {
            format!(" {rows_below} {noun} below ↓ ")
        };
        let style = if state.unread_below {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        block = block.title_bottom(Line::styled(label, style).right_aligned());
    }
    frame.render_widget(paragraph.block(block).scroll((scroll, 0)), area);
    render_transcript_selection(state, frame, area);
}

fn transcript_paragraph(state: &ConversationState, width: u16) -> Paragraph<'static> {
    let mut lines = Vec::new();
    if state.transcript.is_empty() {
        lines.push(Line::styled(
            "No turns yet. Write a prompt below to start.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for entry in &state.transcript {
        let (label, color, model) = match &entry.role {
            EntryRole::Human => ("You".to_string(), Color::Cyan, None),
            EntryRole::Peer(author) => (author.clone(), Color::Magenta, None),
            EntryRole::Agent(model) => ("Agent".to_string(), Color::Green, model.as_deref()),
            EntryRole::Info => ("CAOS".to_string(), Color::Cyan, None),
            EntryRole::Notice => ("Error".to_string(), Color::Red, None),
        };
        let mut heading = vec![Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        if let Some(model) = model {
            heading.push(Span::styled(
                format!(" ({})", model.strip_prefix("claude-").unwrap_or(model)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(commit) = &entry.commit {
            heading.push(Span::styled(
                format!("  {}", short_hash(commit)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(heading));
        lines.extend(markdown_block_lines(&entry.text, width));
        lines.push(Line::raw(""));
    }
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

/// Expand one entry's body into transcript lines: GitHub-style markdown tables
/// are laid out to fit `width`, everything else is rendered as inline markdown.
/// Over-long table cells wrap *within* their column (rather than truncating) so
/// that a select-and-copy — which screen-scrapes the rendered cells — still
/// yields every value in full.
fn markdown_block_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let raw: Vec<&str> = text.lines().collect();
    let mut lines = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        if let Some(consumed) = render_table(&raw[index..], width, &mut lines) {
            index += consumed;
        } else {
            lines.push(inline_markdown_line(raw[index]));
            index += 1;
        }
    }
    lines
}

fn inline_markdown_line(text: &str) -> Line<'static> {
    Line::from(inline_markdown_spans(text, Style::default()))
}

/// Column alignment parsed from a table's delimiter row (`:--`, `--:`, `:-:`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ColumnAlign {
    Left,
    Center,
    Right,
}

/// If `block` starts with a GitHub-style table (a header row, a `---` delimiter
/// row, then zero or more body rows), render it into `out` and return how many
/// input lines it consumed. Otherwise return `None`.
fn render_table(block: &[&str], width: u16, out: &mut Vec<Line<'static>>) -> Option<usize> {
    let header = *block.first()?;
    if !header.contains('|') {
        return None;
    }
    let aligns = table_alignments(block.get(1)?)?;

    let mut rows = vec![split_table_cells(header)];
    let mut consumed = 2;
    while let Some(&line) = block.get(consumed) {
        if line.trim().is_empty() || !line.contains('|') {
            break;
        }
        rows.push(split_table_cells(line));
        consumed += 1;
    }

    let columns = rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .max(aligns.len());
    if columns == 0 {
        return None;
    }
    for row in &mut rows {
        row.resize(columns, String::new());
    }
    let mut aligns = aligns;
    aligns.resize(columns, ColumnAlign::Left);

    // A cell that renders to N columns needs N; every column is at least 1 wide.
    let natural: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| rendered_width(&row[column]))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();

    // Each column is drawn as `| cell `, and the row closes with a final `|`:
    // that is 3 cells of chrome per column plus the closing bar.
    let overhead = 3 * columns + 1;
    let budget = width as usize;
    if budget <= overhead + columns {
        // Too narrow to seat even one cell per column — fall back to inline text.
        return None;
    }
    let widths = fit_widths(&natural, budget - overhead);

    let border = Style::default().fg(Color::DarkGray);
    push_table_row(out, &rows[0], &widths, &aligns, border, true);
    out.push(table_divider(&widths, border));
    for row in &rows[1..] {
        push_table_row(out, row, &widths, &aligns, border, false);
    }
    Some(consumed)
}

/// Split one table row on unescaped `|`, dropping the optional outer borders and
/// trimming each cell.
fn split_table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                current.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut current).trim().to_string()),
            _ => current.push(ch),
        }
    }
    cells.push(current.trim().to_string());
    cells
}

/// Parse a delimiter row (`| :--- | ---: |`) into per-column alignments, or
/// `None` if the line is not a delimiter row.
fn table_alignments(line: &str) -> Option<Vec<ColumnAlign>> {
    if !line.contains('|') {
        return None;
    }
    let cells = split_table_cells(line);
    if cells.is_empty() {
        return None;
    }
    cells
        .iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            let dashes = &cell[usize::from(left)..cell.len() - usize::from(right)];
            if dashes.is_empty() || !dashes.bytes().all(|b| b == b'-') {
                return None;
            }
            Some(match (left, right) {
                (true, true) => ColumnAlign::Center,
                (false, true) => ColumnAlign::Right,
                _ => ColumnAlign::Left,
            })
        })
        .collect()
}

/// Water-fill `natural` column widths into `budget`: columns keep their natural
/// width until a shared cap forces the widest ones to give room back, so only
/// genuinely over-long columns get narrowed (and thus wrapped).
fn fit_widths(natural: &[usize], budget: usize) -> Vec<usize> {
    if natural.iter().sum::<usize>() <= budget {
        return natural.to_vec();
    }
    let max = natural.iter().copied().max().unwrap_or(0);
    let cap = (1..=max)
        .take_while(|cap| natural.iter().map(|&w| w.min(*cap)).sum::<usize>() <= budget)
        .last()
        .unwrap_or(1);
    let mut widths: Vec<usize> = natural.iter().map(|&w| w.min(cap)).collect();
    // Hand any leftover budget back to still-capped columns, one column at a time.
    let mut remaining = budget - widths.iter().sum::<usize>();
    while remaining > 0 {
        let mut progressed = false;
        for (width, &nat) in widths.iter_mut().zip(natural) {
            if remaining == 0 {
                break;
            }
            if *width < nat {
                *width += 1;
                remaining -= 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    widths
}

/// Append one table row to `out`, wrapping each cell to its column width; a row
/// occupies as many transcript lines as its tallest wrapped cell.
fn push_table_row(
    out: &mut Vec<Line<'static>>,
    row: &[String],
    widths: &[usize],
    aligns: &[ColumnAlign],
    border: Style,
    header: bool,
) {
    let base = if header {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let cells: Vec<Vec<Vec<Span<'static>>>> = row
        .iter()
        .enumerate()
        .map(|(column, text)| wrap_cell(text, widths[column], aligns[column], base))
        .collect();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for visual in 0..height {
        let mut spans = vec![Span::styled("|", border)];
        for (column, cell) in cells.iter().enumerate() {
            spans.push(Span::raw(" "));
            match cell.get(visual) {
                Some(line) => spans.extend(line.iter().cloned()),
                None => spans.push(Span::raw(" ".repeat(widths[column]))),
            }
            spans.push(Span::styled(" |", border));
        }
        out.push(Line::from(spans));
    }
}

/// The `|-----|-----|` rule under a table's header.
fn table_divider(widths: &[usize], border: Style) -> Line<'static> {
    let mut spans = vec![Span::styled("|", border)];
    for &width in widths {
        spans.push(Span::styled("-".repeat(width + 2), border));
        spans.push(Span::styled("|", border));
    }
    Line::from(spans)
}

/// Render a cell's inline markdown, then wrap it to `width` display columns
/// (breaking at spaces where possible, hard-breaking an over-long word), padding
/// each visual line to `width` per `align`.
fn wrap_cell(text: &str, width: usize, align: ColumnAlign, base: Style) -> Vec<Vec<Span<'static>>> {
    let styled: Vec<(char, Style)> = inline_markdown_spans(text, base)
        .into_iter()
        .flat_map(|span| {
            let style = span.style;
            span.content
                .chars()
                .map(|ch| (ch, style))
                .collect::<Vec<_>>()
        })
        .collect();

    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut line: Vec<(char, Style)> = Vec::new();
    let mut line_width = 0usize;
    for (ch, style) in styled {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if line_width + ch_width > width && !line.is_empty() {
            if let Some(space) = line.iter().rposition(|&(c, _)| c == ' ') {
                let mut rest = line.split_off(space);
                while rest.first().is_some_and(|&(c, _)| c == ' ') {
                    rest.remove(0);
                }
                while line.last().is_some_and(|&(c, _)| c == ' ') {
                    line.pop();
                }
                lines.push(std::mem::replace(&mut line, rest));
            } else {
                lines.push(std::mem::take(&mut line));
            }
            line_width = line
                .iter()
                .map(|&(c, _)| UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
        }
        line.push((ch, style));
        line_width += ch_width;
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }

    lines
        .into_iter()
        .map(|line| pad_cell_line(line, width, align))
        .collect()
}

/// Coalesce a wrapped cell line's chars back into styled spans and pad it to
/// `width` per `align`.
fn pad_cell_line(line: Vec<(char, Style)>, width: usize, align: ColumnAlign) -> Vec<Span<'static>> {
    let text_width: usize = line
        .iter()
        .map(|&(c, _)| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    let pad = width.saturating_sub(text_width);
    let (left, right) = match align {
        ColumnAlign::Left => (0, pad),
        ColumnAlign::Right => (pad, 0),
        ColumnAlign::Center => (pad / 2, pad - pad / 2),
    };

    let mut spans = Vec::new();
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, style) in line {
        if run_style != Some(style) {
            if let Some(previous) = run_style.take() {
                spans.push(Span::styled(std::mem::take(&mut run), previous));
            }
            run_style = Some(style);
        }
        run.push(ch);
    }
    if let Some(style) = run_style {
        spans.push(Span::styled(run, style));
    }
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }
    spans
}

/// Display width of a cell's text *after* inline markdown markers are stripped,
/// so column widths match what actually renders.
fn rendered_width(text: &str) -> usize {
    inline_markdown_spans(text, Style::default())
        .iter()
        .map(|span| UnicodeWidthStr::width(&*span.content))
        .sum()
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
    paragraph_scroll(
        &transcript_paragraph(state, transcript_inner(area).width),
        area,
        &state.scroll,
    )
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
    let line_count = transcript_paragraph(state, inner.width).line_count(inner.width);
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
    let paragraph = transcript_paragraph(state, inner.width);
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
    let scroll = paragraph_scroll(&paragraph, area, &state.scroll);
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
    let scroll = paragraph_scroll(&paragraph, area, &state.scroll);
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
        Line::raw("  Ctrl+Shift+P    open the command palette"),
        Line::raw(format!("  {send_shortcut:<16}send the prompt")),
        Line::raw("  Enter/Ctrl+J    insert a newline"),
        Line::raw("  Ctrl+A/Ctrl+E   move to the start/end of the line"),
        Line::raw("  Ctrl+W          delete the previous word"),
        Line::raw("  Ctrl+K          delete to the end of the line"),
        Line::raw("  Ctrl+L          check out the conversation commit locally"),
        Line::raw("  Ctrl+P twice    publish a replaceable snapshot and open a PR"),
        Line::raw("  Ctrl+N          start a new conversation"),
        Line::raw("  Esc             stop a running agent or dismiss the current layer"),
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
    let models = if view == View::Chat {
        state.composer.model_matches()
    } else {
        Vec::new()
    };
    let block = Block::default().borders(Borders::TOP | Borders::BOTTOM);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let command_height = (commands.len() + models.len()).min(inner.height as usize) as u16;
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
        &models,
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

fn render_command_menu(
    commands: &[&Command],
    models: &[&str],
    selected: usize,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let command_lines = commands.iter().enumerate().map(|(index, command)| {
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
    let model_lines = models.iter().enumerate().map(|(index, model)| {
        let index = index + commands.len();
        let marker = if index == selected { "> " } else { "  " };
        let style = if index == selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Line::styled(format!("{marker}{model}"), style)
    });
    frame.render_widget(
        Paragraph::new(command_lines.chain(model_lines).collect::<Vec<_>>()),
        area,
    );
}

fn render_footer(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let footer = if app.selection_locked {
        Line::styled(
            " Selection lock: redraws paused, ^Y/Esc resumes",
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )
    } else if app.selected().running
        && (app.focus() != Focus::Conversation
            || app.view != View::Chat
            || app.palette.is_some()
            || app.confirm_action.is_some())
    {
        let send_shortcut = if app.enhanced_keyboard() {
            "^Enter"
        } else {
            "^S"
        };
        Line::raw(format!(
            " Agent running: {send_shortcut} interject  Esc stop  ^T activity  ^Up/Dn switch  ^C quit"
        ))
    } else if app.palette.is_some() {
        Line::raw(" Command palette: type to filter  Up/Dn select  Enter runs  Esc closes")
    } else if matches!(app.confirm_action, Some(ConfirmAction::Publish { .. })) {
        Line::raw(
            " Publish PR: type base branch  Backspace edits  ^U clears  ^P confirms  Esc cancels",
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
        let escape = if app.selected().running {
            "  Esc stop"
        } else {
            ""
        };
        Line::raw(format!(
            " {send_shortcut} send  Enter/^J newline  ^Shift+P commands  ^L checkout  ^P×2 publish  ^Q changes  ^T activity  ^H help{escape}  ^C quit"
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

pub(super) fn paragraph_scroll(paragraph: &Paragraph<'_>, area: Rect, scroll: &ScrollState) -> u16 {
    let line_count = paragraph.line_count(area.width.saturating_sub(2));
    scroll_offset(line_count, area.height, scroll)
}

pub(super) fn scroll_offset(line_count: usize, height: u16, scroll: &ScrollState) -> u16 {
    let visible = height.saturating_sub(2) as usize;
    scroll.resolve(line_count.saturating_sub(visible))
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

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn markdown_table_aligns_columns_and_draws_a_divider() {
        let table = "| Name | Role |\n| :--- | ---: |\n| Ann | dev |\n| Bo | lead |";
        let lines = markdown_block_lines(table, 40);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();

        assert_eq!(
            rendered,
            vec![
                "| Name | Role |".to_string(),
                "|------|------|".to_string(),
                "| Ann  |  dev |".to_string(),
                "| Bo   | lead |".to_string(),
            ]
        );
        // Every row is padded to the same display width.
        let widths: Vec<usize> = rendered.iter().map(|row| row.chars().count()).collect();
        assert!(widths.iter().all(|&w| w == widths[0]));
    }

    #[test]
    fn markdown_table_wraps_over_long_cells_instead_of_truncating() {
        let table = "| Item | Note |\n| --- | --- |\n| a | one two three four five |";
        let lines = markdown_block_lines(table, 24);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();

        // The long cell spills onto extra visual rows; no content is dropped.
        let note: String = rendered
            .iter()
            .flat_map(|row| row.split('|').nth(2).map(str::trim).map(str::to_string))
            .filter(|cell| !cell.is_empty() && !cell.starts_with('-') && *cell != "Note")
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(note, "one two three four five");
        // Nothing exceeds the pane width.
        assert!(rendered.iter().all(|row| row.chars().count() <= 24));
    }

    #[test]
    fn markdown_table_falls_back_to_inline_when_too_narrow() {
        let table = "| Name | Role |\n| --- | --- |\n| Ann | dev |";
        let lines = markdown_block_lines(table, 6);
        // Too narrow to seat the columns: rows stay as raw inline text.
        assert_eq!(line_text(&lines[0]), "| Name | Role |");
    }

    #[test]
    fn markdown_block_leaves_non_tables_as_inline_markdown() {
        let text = "just a **bold** line\nand another";
        let lines = markdown_block_lines(text, 80);
        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[0]), "just a bold line");
        assert_eq!(line_text(&lines[1]), "and another");
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
