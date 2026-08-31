use std::io::{self, IsTerminal, Write};
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use caos::GitTransport;
use caos_cli::{list_user_conversations, unarchive_user_conversation, UserConversationStatus};
use ratatui_core::layout::Rect;
use ratatui_core::terminal::Terminal;
use ratatui_crossterm::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TerminalEvent, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui_crossterm::crossterm::execute;
use ratatui_crossterm::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui_crossterm::CrosstermBackend;

mod app;
mod args;
mod setup;
mod workspace;

use app::{ui::render, App, MouseAction, View};
use args::{usage, Args};

const TICK: Duration = Duration::from_millis(50);
const ANIMATION_TICK: Duration = Duration::from_millis(250);
const REMOTE_POLL_TICK: Duration = Duration::from_millis(500);

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), String> {
    terminal
        .draw(|frame| render(app, frame))
        .map_err(|error| format!("drawing terminal: {error}"))?;
    app.capture_screen(terminal.current_buffer_mut());
    let mut next_animation = Instant::now() + ANIMATION_TICK;
    let mut next_remote_poll = Instant::now();
    while !app.should_quit() {
        // Selection lock deliberately freezes the frame: background turn messages
        // remain queued so redraws cannot invalidate a native terminal
        // selection. They are drained immediately when the lock ends.
        let mut changed = if app.selection_locked() {
            false
        } else {
            app.drain_messages()
        };
        let now = Instant::now();
        if !app.selection_locked() && now >= next_remote_poll {
            app.poll_remote();
            next_remote_poll = now + REMOTE_POLL_TICK;
        }
        let animating = app.has_visible_animation();
        if !app.selection_locked() && animating && now >= next_animation {
            app.advance_animation();
            next_animation = now + ANIMATION_TICK;
            changed = true;
        } else if !animating {
            next_animation = now + ANIMATION_TICK;
        }
        if event::poll(TICK).map_err(|error| format!("polling terminal input: {error}"))? {
            match event::read().map_err(|error| format!("reading terminal input: {error}"))? {
                TerminalEvent::Key(key) => {
                    app.clear_copy_notice();
                    let was_locked = app.selection_locked();
                    let selected_text = if !app.selection_locked()
                        && key.kind == KeyEventKind::Press
                        && key.modifiers.contains(KeyModifiers::SUPER)
                        && key.code == KeyCode::Char('c')
                    {
                        app.selected_composer_text().map(str::to_owned)
                    } else {
                        None
                    };
                    if let Some(text) = selected_text {
                        copy_to_clipboard(terminal.backend_mut(), &text)
                            .map_err(|error| format!("copying composer selection: {error}"))?;
                        app.note_copy(&text);
                    } else {
                        app.handle_key(key);
                    }
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
                    app.clear_copy_notice();
                    app.insert_paste(&text);
                    changed = true;
                }
                TerminalEvent::Mouse(mouse) if !app.selection_locked() => {
                    app.clear_copy_notice();
                    let size = terminal
                        .size()
                        .map_err(|error| format!("reading terminal size: {error}"))?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    match app.handle_mouse(mouse, area) {
                        MouseAction::Ignored => {}
                        MouseAction::Redraw => changed = true,
                        MouseAction::Copy(text) => {
                            copy_to_clipboard(terminal.backend_mut(), &text).map_err(|error| {
                                format!("copying transcript selection: {error}")
                            })?;
                            app.note_copy(&text);
                            changed = true;
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
            app.capture_screen(terminal.current_buffer_mut());
        }
    }
    Ok(())
}

fn selection_lock_allows_redraw(was_locked: bool, is_locked: bool) -> bool {
    !is_locked || was_locked != is_locked
}

/// While the TUI owns the terminal, anything written to stderr — a library
/// warning from a background thread (the 500ms remote poll can emit
/// "skipping malformed conversation" lines), a dependency, a panicking
/// thread — lands at the terminal cursor inside the alternate screen. The
/// cursor rests in the composer, a few rows above the bottom, so a stray
/// line and its newline overwrite and scroll exactly the rows the renderer
/// believes are intact: the composer and footer vanish until something
/// forces a full repaint. Redirect fd 2 into a log file for the TUI's
/// lifetime instead; dropping the guard restores the real stderr.
#[cfg(unix)]
mod stderr_guard {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;

    pub(super) struct StderrRedirect {
        saved: libc::c_int,
        path: PathBuf,
    }

    impl StderrRedirect {
        /// Start capturing stderr. `None` (no capture) on any failure: a
        /// terminal session must never be blocked on the log file.
        pub(super) fn begin() -> Option<Self> {
            let path =
                std::env::temp_dir().join(format!("caos-tui-{}.stderr.log", std::process::id()));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()?;
            let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
            if saved < 0 {
                return None;
            }
            if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
                unsafe { libc::close(saved) };
                return None;
            }
            Some(Self { saved, path })
        }

        /// Restore the real stderr. Returns the log's path when anything was
        /// captured; an untouched log is deleted.
        pub(super) fn finish(self) -> Option<PathBuf> {
            let grew = std::fs::metadata(&self.path).is_ok_and(|meta| meta.len() > 0);
            if !grew {
                let _ = std::fs::remove_file(&self.path);
            }
            grew.then(|| self.path.clone())
        }
    }

    impl Drop for StderrRedirect {
        fn drop(&mut self) {
            unsafe {
                libc::dup2(self.saved, libc::STDERR_FILENO);
                libc::close(self.saved);
            }
        }
    }
}

#[cfg(not(unix))]
mod stderr_guard {
    use std::path::PathBuf;

    pub(super) struct StderrRedirect;

    impl StderrRedirect {
        pub(super) fn begin() -> Option<Self> {
            None
        }

        pub(super) fn finish(self) -> Option<PathBuf> {
            None
        }
    }
}

fn enter_screen(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
}

fn leave_screen(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        PopKeyboardEnhancementFlags,
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

fn copy_to_clipboard(writer: &mut impl Write, text: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if copy_with_pbcopy(text)? {
        return Ok(());
    }

    write_osc52(writer, text)
}

fn write_osc52(writer: &mut impl Write, text: &str) -> io::Result<()> {
    write!(
        writer,
        "\u{1b}]52;c;{}\u{7}",
        base64_encode(text.as_bytes())
    )?;
    writer.flush()
}

#[cfg(target_os = "macos")]
fn copy_with_pbcopy(text: &str) -> io::Result<bool> {
    let mut child = match Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    child
        .stdin
        .take()
        .expect("pbcopy was started with piped stdin")
        .write_all(text.as_bytes())?;
    child.wait().map(|status| status.success())
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let bits = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        encoded.push(ALPHABET[((bits >> 18) & 0x3f) as usize] as char);
        encoded.push(ALPHABET[((bits >> 12) & 0x3f) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((bits >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(bits & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
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
        transport.ensure_server_reachable()?;
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
    let transport = GitTransport::from_cwd()?;
    transport.ensure_server_reachable()?;
    // Missing model credential? Ask for one and install it right here, while
    // the shell still has the terminal (setup::ensure_model_secret).
    setup::ensure_model_secret(&transport)?;
    let mut app = App::new(args)?;

    // From here until the terminal is restored, stderr must not reach the
    // screen (see stderr_guard). The guard restores fd 2 when dropped, on
    // every exit path.
    let stderr_redirect = stderr_guard::StderrRedirect::begin();
    enable_raw_mode().map_err(|error| format!("enabling terminal raw mode: {error}"))?;
    app.set_enhanced_keyboard(supports_keyboard_enhancement().unwrap_or(false));
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
    if let Some(redirect) = stderr_redirect {
        if let Some(path) = redirect.finish() {
            eprintln!(
                "caos tui: stderr from the session was captured in {}",
                path.display()
            );
        }
    }
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
        assert!(output.contains("\u{1b}[>1u"));
        assert!(output.contains("\u{1b}[<1u"));
        assert!(output.find("\u{1b}[>1u") < output.find("\u{1b}[<1u"));
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

    #[cfg(unix)]
    #[test]
    fn stderr_redirect_captures_direct_writes_and_reports_the_log() {
        let redirect = stderr_guard::StderrRedirect::begin().expect("stderr redirect starts");
        // A raw fd-2 write, the same route a background thread's eprintln
        // takes in a real session (libtest's capture shim only wraps the
        // std macros, not the Stderr handle).
        std::io::stderr()
            .write_all(b"probe: redirected stderr line\n")
            .unwrap();
        std::io::stderr().flush().unwrap();
        let path = redirect
            .finish()
            .expect("captured output reports the log path");
        let contents = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(contents.contains("probe: redirected stderr line"));

        // An untouched log is deleted and nothing is reported.
        let redirect = stderr_guard::StderrRedirect::begin().expect("stderr redirect restarts");
        assert_eq!(redirect.finish(), None);
    }

    #[test]
    fn osc52_clipboard_payload_uses_base64() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");

        let mut output = Vec::new();
        write_osc52(&mut output, "selected text").unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output, "\u{1b}]52;c;c2VsZWN0ZWQgdGV4dA==\u{7}");
    }
}
