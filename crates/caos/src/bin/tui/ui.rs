//! Rendering for the single-conversation TUI.

use ratatui_core::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::terminal::Frame;
use ratatui_core::text::{Line, Span};
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::paragraph::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{App, DisplayMessage};

pub(crate) fn render(app: &App, frame: &mut Frame<'_>) {
    let areas = layout(frame.area());
    render_header(app, frame, areas.header);
    render_transcript(app, frame, areas.transcript);
    render_composer(app, frame, areas.composer);
    render_footer(app, frame, areas.footer);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Areas {
    header: Rect,
    transcript: Rect,
    composer: Rect,
    footer: Rect,
}

fn layout(area: Rect) -> Areas {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .split(area);
    Areas {
        header: rows[0],
        transcript: rows[1],
        composer: rows[2],
        footer: rows[3],
    }
}

fn render_header(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let head = app.head().map(short_hash).unwrap_or("new");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {}", app.title()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {head}"), Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("  {}", app.status()),
                Style::default().fg(status_color(app.status())),
            ),
        ])),
        area,
    );
}

fn render_transcript(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .title(" Conversation ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let paragraph = transcript_paragraph(app.messages()).wrap(Wrap { trim: false });
    let lines = paragraph.line_count(inner.width).min(u16::MAX as usize) as u16;
    let maximum = lines.saturating_sub(inner.height);
    let from_bottom = app.scroll_from_bottom().min(maximum);
    let top = maximum.saturating_sub(from_bottom);
    frame.render_widget(paragraph.scroll((top, 0)), inner);
}

fn transcript_paragraph(messages: &[DisplayMessage]) -> Paragraph<'static> {
    if messages.is_empty() {
        return Paragraph::new(Line::styled(
            "No messages yet. Type below and press Enter.",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let mut lines = Vec::new();
    for message in messages {
        let label = if matches!(message.author.as_str(), "user" | "human") {
            message.username.as_deref().unwrap_or(&message.author)
        } else {
            &message.author
        };
        lines.push(Line::styled(
            label.to_string(),
            author_style(&message.author),
        ));
        if message.content.is_empty() {
            lines.push(Line::default());
        } else {
            lines.extend(
                message
                    .content
                    .lines()
                    .map(|line| Line::raw(line.to_string())),
            );
        }
        lines.push(Line::default());
    }
    Paragraph::new(lines)
}

fn author_style(author: &str) -> Style {
    let color = match author {
        "user" | "human" => Color::Cyan,
        "assistant" | "agent" => Color::Green,
        _ => Color::Yellow,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn render_composer(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .title(" Message ")
        .border_style(Style::default().fg(Color::Cyan))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.max(1);
    let (column, row) = text_position(app.composer(), app.cursor(), width);
    let scroll = row.saturating_sub(inner.height.saturating_sub(1));
    let lines: Vec<Line<'static>> = hard_wrap(app.composer(), width)
        .into_iter()
        .map(Line::raw)
        .collect();
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
    let visible_row = row.saturating_sub(scroll);
    if inner.width > 0 && inner.height > 0 {
        frame.set_cursor_position(Position::new(
            inner.x + column.min(inner.width.saturating_sub(1)),
            inner.y + visible_row.min(inner.height.saturating_sub(1)),
        ));
    }
}

fn text_position(text: &str, cursor: usize, width: u16) -> (u16, u16) {
    let lines = hard_wrap(&text[..cursor], width);
    let row = lines.len().saturating_sub(1).min(u16::MAX as usize) as u16;
    let column = UnicodeWidthStr::width(lines.last().map(String::as_str).unwrap_or(""))
        .min(u16::MAX as usize) as u16;
    (column, row)
}

/// Character-wrap the composer explicitly so its rendered lines and cursor
/// use one layout model. `Paragraph::wrap` is word-based and would move the
/// cursor away from the text whenever a word crosses the right edge.
fn hard_wrap(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut column = 0_u16;
    for character in text.chars() {
        if character == '\n' {
            lines.push(String::new());
            column = 0;
            continue;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0) as u16;
        if character_width > 0 && column > 0 && column.saturating_add(character_width) > width {
            lines.push(String::new());
            column = 0;
        }
        lines
            .last_mut()
            .expect("composer has a line")
            .push(character);
        column = column.saturating_add(character_width);
        if column >= width {
            lines.push(String::new());
            column = 0;
        }
    }
    lines
}

fn render_footer(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let line = match app.notice() {
        Some(notice) => Line::styled(
            format!(" {notice}"),
            Style::default().fg(if notice.starts_with("PR ready:") {
                Color::Green
            } else {
                Color::Red
            }),
        ),
        None => Line::styled(
            format!(
                " Enter send  Ctrl+P PR  Up/Down scroll  Ctrl+L refresh  Esc quit  [{} as {}]",
                app.conversation(),
                app.username()
            ),
            Style::default().fg(Color::DarkGray),
        ),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn status_color(status: &str) -> Color {
    match status {
        "failed" | "error" => Color::Red,
        "queued" | "running" | "submitting" => Color::Yellow,
        _ => Color::Green,
    }
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_keeps_authors_and_multiline_content() {
        let paragraph = transcript_paragraph(&[
            DisplayMessage {
                author: "user".to_string(),
                username: Some("Alice".to_string()),
                content: "one\ntwo".to_string(),
            },
            DisplayMessage {
                author: "assistant".to_string(),
                username: None,
                content: "three".to_string(),
            },
        ]);
        assert_eq!(paragraph.line_count(80), 7);
    }

    #[test]
    fn composer_cursor_wraps_and_handles_wide_characters() {
        assert_eq!(text_position("abcd", 4, 4), (0, 1));
        assert_eq!(text_position("a界", "a界".len(), 4), (3, 0));
        assert_eq!(text_position("a\nb", 3, 10), (1, 1));
        assert_eq!(text_position("hello world", 11, 10), (1, 1));
        assert_eq!(hard_wrap("hello world", 10), ["hello worl", "d"]);
        assert_eq!(hard_wrap("a界b", 3), ["a界", "b"]);
        assert_eq!(hard_wrap("a\nb", 10), ["a", "b"]);
        assert_eq!(hard_wrap("a", 0), ["a", ""]);
    }
}
