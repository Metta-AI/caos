use std::io::{self, IsTerminal};
use std::time::Duration;

use caos::chat::{
    list_user_conversations, publish_unindexed_conversations, unarchive_user_conversation,
    UserConversationStatus,
};
use caos::GitTransport;
use ratatui_core::layout::Rect;
use ratatui_core::terminal::Terminal;
use ratatui_crossterm::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TerminalEvent, MouseEventKind,
};
use ratatui_crossterm::crossterm::execute;
use ratatui_crossterm::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui_crossterm::CrosstermBackend;

mod app;
mod args;
mod workspace;

use app::{
    ui::{self, render},
    App, View,
};
use args::{usage, Args};

const TICK: Duration = Duration::from_millis(50);

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), String> {
    terminal
        .draw(|frame| render(app, frame))
        .map_err(|error| format!("drawing terminal: {error}"))?;
    while !app.should_quit() {
        // Selection lock deliberately freezes the frame: background turn messages
        // remain queued so redraws cannot invalidate a native terminal
        // selection. They are drained immediately when the lock ends.
        let mut changed = if app.selection_locked() {
            false
        } else {
            app.drain_messages()
        };
        if event::poll(TICK).map_err(|error| format!("polling terminal input: {error}"))? {
            match event::read().map_err(|error| format!("reading terminal input: {error}"))? {
                TerminalEvent::Key(key) => {
                    let was_locked = app.selection_locked();
                    app.handle_key(key);
                    if was_locked != app.selection_locked() {
                        set_mouse_capture(terminal.backend_mut(), !app.selection_locked())
                            .map_err(|error| {
                                format!("switching terminal selection mode: {error}")
                            })?;
                    }
                    changed |= selection_lock_allows_redraw(was_locked, app.selection_locked());
                }
                TerminalEvent::Paste(text)
                    if app.view() == View::Chat && !app.selection_locked() =>
                {
                    app.insert_paste(&text);
                    changed = true;
                }
                TerminalEvent::Mouse(mouse) if !app.selection_locked() => {
                    let size = terminal
                        .size()
                        .map_err(|error| format!("reading terminal size: {error}"))?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    if app.showing_transcript()
                        && ui::content_contains(area, mouse.column, mouse.row)
                    {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                app.scroll_up(3);
                                changed = true;
                            }
                            MouseEventKind::ScrollDown => {
                                app.scroll_down(3);
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                }
                TerminalEvent::Resize(_, _) if !app.selection_locked() => changed = true,
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

fn selection_lock_allows_redraw(was_locked: bool, is_locked: bool) -> bool {
    !is_locked || was_locked != is_locked
}

fn enter_screen(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )
}

fn leave_screen(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
}

fn set_mouse_capture(writer: &mut impl io::Write, enabled: bool) -> io::Result<()> {
    if enabled {
        execute!(writer, EnableMouseCapture)
    } else {
        execute!(writer, DisableMouseCapture)
    }
}

pub(crate) fn run(raw: &[String]) -> Result<(), String> {
    if raw
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        println!("{}", usage());
        return Ok(());
    }
    let args = Args::parse(raw)?;
    if args.list_archived || args.unarchive.is_some() {
        let transport = GitTransport::from_cwd()?;
        publish_unindexed_conversations(&transport, &args.user)?;
        if args.list_archived {
            for conversation in
                list_user_conversations(&transport, &args.user, UserConversationStatus::Archived)?
            {
                println!("{}\t{}", conversation.id, conversation.title);
            }
        } else if let Some(id) = &args.unarchive {
            unarchive_user_conversation(&transport, &args.user, id)?;
            println!("unarchived {id}");
        }
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("requires an interactive terminal; use `caos talk` for pipes".to_string());
    }
    let mut app = App::new(args)?;

    enable_raw_mode().map_err(|error| format!("enabling terminal raw mode: {error}"))?;
    let mut stdout = io::stdout();
    if let Err(error) = enter_screen(&mut stdout) {
        let _ = disable_raw_mode();
        let _ = leave_screen(&mut stdout);
        return Err(format!("entering alternate screen: {error}"));
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = leave_screen(&mut io::stdout());
            return Err(format!("initializing terminal: {error}"));
        }
    };
    let result = run_app(&mut terminal, &mut app);

    let raw_result = disable_raw_mode().map_err(|error| error.to_string());
    let screen_result = leave_screen(terminal.backend_mut())
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

    #[test]
    fn terminal_lifecycle_enables_input_modes_and_restores_the_terminal() {
        let mut output = Vec::new();
        enter_screen(&mut output).unwrap();
        leave_screen(&mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\u{1b}[?1049h"));
        assert!(output.contains("\u{1b}[?1049l"));
        assert!(output.contains("\u{1b}[?2004h"));
        assert!(output.contains("\u{1b}[?2004l"));
        assert!(output.find("\u{1b}[?2004h") < output.find("\u{1b}[?2004l"));
        assert!(output.contains("\u{1b}[?1000h"));
        assert!(output.contains("\u{1b}[?1000l"));
    }

    #[test]
    fn selection_mode_releases_and_restores_mouse_capture() {
        let mut output = Vec::new();
        set_mouse_capture(&mut output, false).unwrap();
        set_mouse_capture(&mut output, true).unwrap();

        let output = String::from_utf8(output).unwrap();
        let disabled = output.find("\u{1b}[?1000l").unwrap();
        let enabled = output.rfind("\u{1b}[?1000h").unwrap();
        assert!(disabled < enabled);
    }

    #[test]
    fn selection_lock_redraws_only_when_entering_or_leaving() {
        assert!(selection_lock_allows_redraw(false, false));
        assert!(selection_lock_allows_redraw(false, true));
        assert!(!selection_lock_allows_redraw(true, true));
        assert!(selection_lock_allows_redraw(true, false));
    }
}
