use std::io::{self, IsTerminal, Write};
#[cfg(any(target_os = "macos", test))]
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
mod launcher;
mod setup;
use caos_cli::host_git as source_tree;

use app::{ui::render, App, MouseAction, View};
use args::{usage, Args};

const TICK: Duration = Duration::from_millis(50);
const ANIMATION_TICK: Duration = Duration::from_millis(250);
const REMOTE_POLL_TICK: Duration = Duration::from_millis(500);

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), String> {
    let completed = terminal
        .draw(|frame| render(app, frame))
        .map_err(|error| format!("drawing terminal: {error}"))?;
    app.capture_screen(completed.buffer);
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
                        let result = copy_to_clipboard(terminal.backend_mut(), &text);
                        app.note_copy(&text, result);
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
                    if (app.view() == View::Chat
                        || app.browser_visible()
                        || app.publication_visible())
                        && !app.selection_locked() =>
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
                            let result = copy_to_clipboard(terminal.backend_mut(), &text);
                            app.note_copy(&text, result);
                            changed = true;
                        }
                    }
                }
                TerminalEvent::Resize(_, _) if !app.selection_locked() => changed = true,
                _ => {}
            }
        }
        if changed {
            let completed = terminal
                .draw(|frame| render(app, frame))
                .map_err(|error| format!("drawing terminal: {error}"))?;
            app.capture_screen(completed.buffer);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyOutcome {
    Copied,
    Requested,
}

fn copy_to_clipboard(writer: &mut impl Write, text: &str) -> io::Result<CopyOutcome> {
    copy_with_native(writer, text, || {
        #[cfg(target_os = "macos")]
        if !["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return copy_with_command(Command::new("pbcopy"), text);
        }
        Ok(false)
    })
}

fn copy_with_native(
    writer: &mut impl Write,
    text: &str,
    native: impl FnOnce() -> io::Result<bool>,
) -> io::Result<CopyOutcome> {
    if native()? {
        return Ok(CopyOutcome::Copied);
    }
    // OSC 52 has no write acknowledgement. A successful flush says nothing
    // about terminal permissions or whether the clipboard changed.
    write_osc52(writer, text)?;
    Ok(CopyOutcome::Requested)
}

fn write_osc52(writer: &mut impl Write, text: &str) -> io::Result<()> {
    write!(
        writer,
        "\u{1b}]52;c;{}\u{7}",
        base64_encode(text.as_bytes())
    )?;
    writer.flush()
}

#[cfg(any(target_os = "macos", test))]
fn copy_with_command(mut command: Command, text: &str) -> io::Result<bool> {
    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let write_result = child
        .stdin
        .take()
        .expect("clipboard helper was started with piped stdin")
        .write_all(text.as_bytes());
    // Close stdin and reap the helper even when it rejected the input early.
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "clipboard helper exited with {status}"
        )));
    }
    write_result?;
    Ok(true)
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
    let mut args = Args::parse(raw)?;
    let archive_in_checkout = (args.list_archived || args.unarchive.is_some())
        && args.server.is_none()
        && args.harness.is_none()
        && GitTransport::from_cwd().is_ok();
    if !archive_in_checkout {
        let repo = launcher::prepare(&mut args)?;
        std::env::set_current_dir(&repo)
            .map_err(|error| format!("opening client store: {error}"))?;
    }
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
    setup::ensure_model_secret(&transport, &args.turn)?;
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
    use ratatui_core::backend::TestBackend;
    use ratatui_core::text::Span;

    use super::*;

    #[test]
    fn capture_screen_must_read_the_completed_frame_because_draw_swaps_buffers() {
        let mut terminal = Terminal::new(TestBackend::new(4, 1)).unwrap();

        let drawn = terminal
            .draw(|frame| frame.render_widget(Span::raw("x"), frame.area()))
            .unwrap()
            .buffer[(0, 0)]
            .symbol()
            .to_string();

        assert_eq!(drawn, "x");
        assert_eq!(terminal.current_buffer_mut()[(0, 0)].symbol(), " ");
    }

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
    fn clipboard_native_completion_and_terminal_request_are_distinct() {
        let mut output = Vec::new();
        assert_eq!(
            copy_with_native(&mut output, "selected text", || Ok(true)).unwrap(),
            CopyOutcome::Copied,
        );
        assert!(output.is_empty());
        assert_eq!(
            copy_with_native(&mut output, "héllo\n世界", || Ok(false)).unwrap(),
            CopyOutcome::Requested,
        );
        assert_eq!(output, b"\x1b]52;c;aMOpbGxvCuS4lueVjA==\x07");
    }

    #[test]
    fn clipboard_failures_never_report_success() {
        let mut output = Vec::new();
        let failure = copy_with_native(&mut output, "selected text", || {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        });
        assert_eq!(failure.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(output.is_empty());

        struct BrokenWriter {
            fail_flush: bool,
        }
        impl Write for BrokenWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.fail_flush {
                    Ok(buf.len())
                } else {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "write failed"))
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "flush failed"))
            }
        }
        for fail_flush in [false, true] {
            let error = copy_with_native(&mut BrokenWriter { fail_flush }, "text", || Ok(false))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }
    }

    #[cfg(unix)]
    fn clipboard_test_command(mode: &str) -> Command {
        // The cargo worker has no shell or coreutils. Reuse this test binary
        // as the helper, with environment changes confined to its subprocess.
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tui::tests::clipboard_helper_process",
                "--nocapture",
            ])
            .env("CAOS_TEST_CLIPBOARD_HELPER", mode);
        command
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_helper_process() {
        use std::io::Read;

        let Ok(mode) = std::env::var("CAOS_TEST_CLIPBOARD_HELPER") else {
            return;
        };
        if mode == "early-exit" {
            std::process::exit(9);
        }
        let mut bytes = Vec::new();
        io::stdin().read_to_end(&mut bytes).unwrap();
        if mode == "failure" {
            std::process::exit(7);
        }
        assert_eq!(mode, "success");
        let path = std::env::var_os("CAOS_TEST_CLIPBOARD_OUTPUT").unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_helper_receives_exact_bytes_and_waits_for_completion() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("clipboard");
        let mut command = clipboard_test_command("success");
        command.env("CAOS_TEST_CLIPBOARD_OUTPUT", &output);
        assert!(copy_with_command(command, "héllo\n世界\n").unwrap());
        assert_eq!(std::fs::read(output).unwrap(), "héllo\n世界\n".as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_helper_reports_unavailable_and_failed_commands() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !copy_with_command(Command::new(dir.path().join("missing-helper")), "text").unwrap()
        );
        let error = copy_with_command(clipboard_test_command("failure"), "text").unwrap_err();
        assert!(error.to_string().contains("exit status: 7"));

        // A helper that closes stdin immediately must still be waited for.
        let error = copy_with_command(
            clipboard_test_command("early-exit"),
            &"x".repeat(1024 * 1024),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exit status: 9"));
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
