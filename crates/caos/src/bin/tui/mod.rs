use std::io::{self, IsTerminal};
use std::time::Duration;

use ratatui_core::terminal::Terminal;
use ratatui_crossterm::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TerminalEvent, MouseEventKind,
};
use ratatui_crossterm::crossterm::execute;
use ratatui_crossterm::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui_crossterm::CrosstermBackend;

mod app;
mod args;
mod backend;
mod workspace;

use app::{ui::render, App, View};
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
        // Copy mode deliberately freezes the frame: background turn messages
        // remain queued so redraws cannot invalidate a native terminal
        // selection. They are drained immediately when copy mode ends.
        let mut changed = if app.copy_mode() {
            false
        } else {
            app.drain_messages()
        };
        if event::poll(TICK).map_err(|error| format!("polling terminal input: {error}"))? {
            match event::read().map_err(|error| format!("reading terminal input: {error}"))? {
                TerminalEvent::Key(key) => {
                    let was_copy_mode = app.copy_mode();
                    app.handle_key(key);
                    if was_copy_mode != app.copy_mode() {
                        if app.copy_mode() {
                            execute!(terminal.backend_mut(), DisableMouseCapture)
                        } else {
                            execute!(terminal.backend_mut(), EnableMouseCapture)
                        }
                        .map_err(|error| format!("switching terminal copy mode: {error}"))?;
                    }
                    changed = true;
                }
                TerminalEvent::Paste(text) if app.view() == View::Chat && !app.copy_mode() => {
                    app.insert_paste(&text);
                    changed = true;
                }
                TerminalEvent::Mouse(mouse) if !app.copy_mode() => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        app.scroll_up(3);
                        changed = true;
                    }
                    MouseEventKind::ScrollDown => {
                        app.scroll_down(3);
                        changed = true;
                    }
                    _ => {}
                },
                TerminalEvent::Resize(_, _) if !app.copy_mode() => changed = true,
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

pub(crate) fn run(raw: &[String]) -> Result<(), String> {
    if raw
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        println!("{}", usage());
        return Ok(());
    }
    let args = Args::parse(raw)?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("requires an interactive terminal; use `caos talk` for pipes".to_string());
    }
    let mut app = App::new(args)?;

    enable_raw_mode().map_err(|error| format!("enabling terminal raw mode: {error}"))?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
        return Err(format!("entering alternate screen: {error}"));
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            return Err(format!("initializing terminal: {error}"));
        }
    };
    let result = run_app(&mut terminal, &mut app);

    let raw_result = disable_raw_mode().map_err(|error| error.to_string());
    let screen_result = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .and_then(|()| terminal.show_cursor())
    .map_err(|error| error.to_string());
    result?;
    raw_result.map_err(|error| format!("restoring terminal mode: {error}"))?;
    screen_result.map_err(|error| format!("leaving alternate screen: {error}"))?;
    Ok(())
}
