//! A small conversation UI whose only durable state is the remote head ref.

use std::io::{self, IsTerminal};
use std::time::Duration;

use caos::GitTransport;
use ratatui_core::terminal::Terminal;
use ratatui_crossterm::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as TerminalEvent, KeyEventKind,
};
use ratatui_crossterm::crossterm::execute;
use ratatui_crossterm::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui_crossterm::CrosstermBackend;

mod app;
mod args;
mod ui;

use app::App;
use args::{usage, Args};

const TICK: Duration = Duration::from_millis(50);

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), String> {
    terminal
        .draw(|frame| ui::render(app, frame))
        .map_err(|error| format!("drawing terminal: {error}"))?;
    while !app.should_quit() {
        let mut changed = app.tick();
        if event::poll(TICK).map_err(|error| format!("polling terminal input: {error}"))? {
            match event::read().map_err(|error| format!("reading terminal input: {error}"))? {
                TerminalEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    changed |= app.handle_key(key)
                }
                TerminalEvent::Paste(text) => {
                    app.insert_text(&text);
                    changed = true;
                }
                TerminalEvent::Resize(_, _) => changed = true,
                _ => {}
            }
        }
        if changed {
            terminal
                .draw(|frame| ui::render(app, frame))
                .map_err(|error| format!("drawing terminal: {error}"))?;
        }
    }
    Ok(())
}

fn enter_screen(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(writer, EnterAlternateScreen, EnableBracketedPaste)
}

fn leave_screen(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(writer, DisableBracketedPaste, LeaveAlternateScreen)
}

pub(crate) fn run(raw: &[String]) -> Result<(), String> {
    if raw
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        println!("{}", usage());
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("requires an interactive terminal; use `caos talk` for pipes".to_string());
    }
    let args = Args::parse(raw)?;
    GitTransport::from_cwd()?.ensure_server_reachable()?;
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

    let app_result = run_app(&mut terminal, &mut app);
    let raw_result = disable_raw_mode().map_err(|error| error.to_string());
    let screen_result = leave_screen(terminal.backend_mut())
        .and_then(|()| terminal.show_cursor())
        .map_err(|error| error.to_string());
    app_result?;
    raw_result.map_err(|error| format!("restoring terminal mode: {error}"))?;
    screen_result.map_err(|error| format!("leaving alternate screen: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_lifecycle_enables_paste_and_restores_the_screen() {
        let mut output = Vec::new();
        enter_screen(&mut output).unwrap();
        leave_screen(&mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\u{1b}[?1049h"));
        assert!(output.contains("\u{1b}[?1049l"));
        assert!(output.contains("\u{1b}[?2004h"));
        assert!(output.contains("\u{1b}[?2004l"));
        assert!(output.find("\u{1b}[?2004h") < output.find("\u{1b}[?2004l"));
    }
}
