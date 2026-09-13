use super::{ui, App, Composer, ConversationState, EntryRole};
use ratatui_core::buffer::CellWidth;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// Local attempted input is independent of whether a turn reaches the transcript.
#[derive(Default)]
pub(super) struct InputLog {
    path: Option<PathBuf>,
    entries: Option<Vec<String>>,
}

impl InputLog {
    fn open(path: PathBuf) -> Result<Self, String> {
        let entries = match File::open(&path) {
            Ok(mut file) => {
                file.lock_shared().map_err(|e| e.to_string())?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                let complete = complete_records(&bytes);
                if complete.is_empty() {
                    None
                } else {
                    Some(
                        complete
                            .split_inclusive(|byte| *byte == b'\n')
                            .map(serde_json::from_slice)
                            .collect::<Result<_, _>>()
                            .map_err(|e| format!("reading input history: {e}"))?,
                    )
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("reading input history: {e}")),
        };
        Ok(Self {
            path: Some(path),
            entries,
        })
    }

    fn record(&mut self, text: &str, previous: impl Iterator<Item = String>) -> io::Result<()> {
        let entries = self.entries.get_or_insert_with(|| previous.collect());
        entries.push(text.to_string());
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        // Serialize concurrent clients' complete records, including a first seed
        // from saved prompts, without replacing one another's attempted input.
        file.lock()?;
        // A stopped process can leave its last record incomplete. Trim only
        // that unfinished suffix before appending another complete record.
        let mut len = file.metadata()?.len();
        if len > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.rewind()?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                len = complete_records(&bytes).len() as u64;
                file.set_len(len)?;
            }
        }
        let start = if len == 0 { 0 } else { entries.len() - 1 };
        let mut bytes = Vec::new();
        for entry in &entries[start..] {
            serde_json::to_writer(&mut bytes, entry)?;
            bytes.push(b'\n');
        }
        file.write_all(&bytes)
    }
}

fn complete_records(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |i| i + 1)]
}

/// A browsing session freezes input order while remote updates arrive.
/// Each entry retains edits, and the last entry is the user's original draft.
pub(super) struct InputHistory {
    entries: Vec<Composer>,
    position: usize,
}

impl App {
    fn load_input_log(&mut self) -> Result<(), String> {
        if self.selected().input_log.path.is_some() || self.selected().input_log.entries.is_some() {
            return Ok(());
        }
        let Some(dir) = &self.input_log_dir else {
            return Ok(());
        };
        let key = super::super::launcher::hash_key(
            &self.repo_dir,
            &serde_json::json!(self.selected().id),
        )?;
        let log = InputLog::open(dir.join(format!("{key}.jsonl")))?;
        self.selected_mut().input_log = log;
        Ok(())
    }

    pub(super) fn remember_input(&mut self, text: &str) {
        let load_error = self.load_input_log().err();
        let state = self.selected_mut();
        let previous = state
            .transcript
            .iter()
            .filter(|entry| entry.role == EntryRole::Human && !entry.text.trim().is_empty())
            .map(|entry| entry.text.clone());
        let save_error = state
            .input_log
            .record(text, previous)
            .err()
            .map(|e| e.to_string());
        self.selected_mut().input_history = None;
        if let Some(error) = load_error.or(save_error) {
            self.selected_mut()
                .push_error(format!("Could not save input history: {error}"));
        }
    }

    pub(super) fn navigate_input(&mut self, up: bool) {
        if let Err(error) = self.load_input_log() {
            self.selected_mut().show_command_error(error);
            return;
        }
        let width = self
            .rendered_screen
            .as_ref()
            .map(|screen| ui::composer_width(self.selected(), screen.area));
        self.selected_mut().navigate_input(up, width);
    }
}

impl ConversationState {
    fn navigate_input(&mut self, up: bool, width: Option<u16>) {
        let moved = match width {
            Some(width) => self.composer.move_visual_vertical(up, width),
            None => self.composer.move_vertical(up),
        };
        if moved {
            return;
        }
        if self.input_history.is_none() {
            if !up {
                return;
            }
            let saved: Vec<_> = self
                .transcript
                .iter()
                .filter(|entry| entry.role == EntryRole::Human && !entry.text.trim().is_empty())
                .map(|entry| entry.text.clone())
                .collect();
            let inputs = self.input_log.entries.as_deref().unwrap_or(&saved);
            let mut entries: Vec<_> = inputs
                .iter()
                .map(|text| {
                    let mut composer = Composer::default();
                    composer.insert_str(text);
                    composer.dismiss_command_menu();
                    composer
                })
                .collect();
            if entries.is_empty() {
                return;
            }
            let position = entries.len();
            entries.push(self.composer.clone());
            self.input_history = Some(InputHistory { entries, position });
        }
        let history = self
            .input_history
            .as_mut()
            .expect("history was initialized");
        let next = if up {
            history.position.saturating_sub(1)
        } else {
            (history.position + 1).min(history.entries.len() - 1)
        };
        history.entries[history.position] = self.composer.clone();
        history.position = next;
        self.composer = history.entries[next].clone();
        if next + 1 == history.entries.len() {
            self.input_history = None;
        }
    }
}

impl Composer {
    /// Use the same wrapped rows and terminal cell widths as the renderer.
    /// Reaching a visual boundary permits history navigation; moving within
    /// the draft, including a wrapped logical line, never replaces its text.
    fn move_visual_vertical(&mut self, up: bool, width: u16) -> bool {
        let (row, column) = ui::composer_cursor(self, width);
        let mut ranges = ui::composer_visual_ranges(&self.text, width);
        // An exactly full final row puts the cursor on an empty row below it.
        if row == ranges.len() {
            ranges.push((self.text.len(), self.text.len()));
        }
        let target = if up {
            row.checked_sub(1)
        } else {
            (row + 1 < ranges.len()).then_some(row + 1)
        };
        let Some(target) = target else {
            return false;
        };
        let (start, end) = ranges[target];
        let mut cursor = start;
        let mut cells = 0;
        for ch in self.text[start..end].chars() {
            let next_cells = cells + usize::from(ch.to_string().cell_width());
            if next_cells > column {
                break;
            }
            cells = next_cells;
            cursor += ch.len_utf8();
        }
        self.move_cursor(cursor, false);
        self.snap_cursor_after_placeholder();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::TranscriptEntry;
    use super::*;
    use caos_cli::TurnOptions;
    use std::fs;

    fn conversation(messages: &[&str]) -> ConversationState {
        let mut state = ConversationState::new(
            "history".into(),
            "History".into(),
            TurnOptions::default(),
            "ready".into(),
        );
        state.transcript = messages
            .iter()
            .map(|message| TranscriptEntry {
                role: EntryRole::Human,
                commit: None,
                text: (*message).into(),
                pending_id: None,
            })
            .collect();
        state
    }

    #[test]
    fn history_preserves_draft_pastes_cursor_and_edits_to_recalled_messages() {
        let mut state = conversation(&["first", "second"]);
        state
            .composer
            .insert_paste(&"long pasted text ".repeat(100));
        state.composer.insert_str(" draft");
        state.composer.select_word_left();
        let draft = state.composer.clone();

        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "second");
        state.composer.insert_str(" edited");
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "first");
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "first");
        state.navigate_input(false, None);
        assert_eq!(state.composer.text, "second edited");
        state.navigate_input(false, None);
        assert_eq!(state.composer, draft);
        assert!(state.input_history.is_none());
    }

    #[test]
    fn history_excludes_peers_agents_and_notices() {
        let mut state = conversation(&["mine", "peer", "agent", "notice", " "]);
        state.transcript[1].role = EntryRole::Peer("someone else".into());
        state.transcript[2].role = EntryRole::Agent(None);
        state.transcript[3].role = EntryRole::Info;
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "mine");
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "mine");
        state.navigate_input(false, None);
        assert!(state.composer.text.is_empty());
    }

    #[test]
    fn history_keeps_multiline_editing_until_the_first_or_last_line() {
        let mut state = conversation(&["older", "one\ntwo"]);
        state.composer.insert_str("draft\nline");
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "draft\nline");
        assert_eq!(state.composer.cursor_row_col(), (0, 4));
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "one\ntwo");
        state.navigate_input(true, None);
        assert_eq!(state.composer.cursor_row_col(), (0, 3));
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "older");
        state.navigate_input(false, None);
        assert_eq!(state.composer.cursor_row_col(), (0, 3));
        state.navigate_input(false, None);
        assert_eq!(state.composer.cursor_row_col(), (1, 3));
        state.navigate_input(false, None);
        assert_eq!(state.composer.text, "draft\nline");
        assert_eq!(state.composer.cursor_row_col(), (0, 4));
    }

    #[test]
    fn history_waits_until_the_first_wrapped_row_and_handles_wide_characters() {
        let mut state = conversation(&["previous"]);
        state.composer.insert_str("ab界cdef");
        state.navigate_input(true, Some(4));
        assert_eq!(state.composer.text, "ab界cdef");
        assert_eq!(state.composer.cursor, "ab界".len());
        state.navigate_input(true, Some(4));
        assert_eq!(state.composer.cursor, 0);
        state.navigate_input(true, Some(4));
        assert_eq!(state.composer.text, "previous");
        // A full last row places the cursor on the extra empty visual row.
        state.navigate_input(false, Some(4));
        assert_eq!(state.composer.text, "ab界cdef");
        assert_eq!(state.composer.cursor, 0);
    }

    #[test]
    fn history_snapshot_is_stable_when_messages_arrive_and_refreshes_after_submission() {
        let mut state = conversation(&["first", "second"]);
        state.navigate_input(true, None);
        state.transcript.push(TranscriptEntry {
            role: EntryRole::Human,
            commit: None,
            text: "remote update".into(),
            pending_id: None,
        });
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "first");
        state.navigate_input(false, None);
        assert_eq!(state.composer.text, "second");
        state.navigate_input(false, None);
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "remote update");
        state.composer.clear();
        state.queue_pending_submission("latest".into());
        state.navigate_input(true, None);
        assert_eq!(state.composer.text, "latest");
        state.navigate_input(false, None);
        assert!(state.composer.text.is_empty());
    }

    #[test]
    fn attempted_commands_and_rejected_messages_survive_reopening() {
        use super::super::tests::app_with;
        use ratatui_crossterm::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let dir = tempfile::tempdir().unwrap();
        let (mut app, _) = app_with(vec![conversation(&["saved prompt"])]);
        app.selected_mut().virtual_conversation = true;
        app.input_log_dir = Some(dir.path().to_path_buf());

        for text in [
            "/title",
            "/title Renamed",
            "rejected prompt",
            "/update-tree missing edit",
        ] {
            app.selected_mut().composer.clear();
            app.selected_mut().composer.insert_str(text);
            app.selected_mut().forking = text == "rejected prompt";
            app.start_turn();
            if text != "/title Renamed" {
                assert!(app.selected().command_error.is_some());
            }
        }
        assert_eq!(app.selected().title, "Renamed");
        assert_eq!(app.selected().transcript.len(), 1);
        let (mut reopened, _) = app_with(vec![conversation(&["saved prompt"])]);
        reopened.input_log_dir = Some(dir.path().to_path_buf());
        reopened
            .selected_mut()
            .composer
            .insert_str("unfinished draft");
        for expected in [
            "/update-tree missing edit",
            "rejected prompt",
            "/title Renamed",
            "/title",
            "saved prompt",
        ] {
            reopened.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            assert_eq!(reopened.selected().composer.text, expected);
        }
        for _ in 0..5 {
            reopened.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(reopened.selected().composer.text, "unfinished draft");
        // A second conversation never inherits the first one's local input.
        let mut other = conversation(&[]);
        other.id = "other".into();
        let (mut other, _) = app_with(vec![other]);
        other.input_log_dir = Some(dir.path().to_path_buf());
        other.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(other.selected().composer.text.is_empty());
    }

    #[test]
    fn failed_submission_remains_in_history_after_its_pending_row_is_removed() {
        use super::super::tests::app_with;
        let (mut app, _) = app_with(vec![conversation(&["saved"])]);
        app.remember_input("failed attempt");
        let id = app
            .selected_mut()
            .queue_pending_submission("failed attempt".into());
        app.selected_mut().restore_pending_submission(id);
        app.selected_mut().composer.clear();
        app.navigate_input(true);
        assert_eq!(app.selected().composer.text, "failed attempt");
        app.navigate_input(true);
        assert_eq!(app.selected().composer.text, "saved");
        assert_eq!(app.selected().transcript.len(), 1);
    }

    #[test]
    fn input_log_appends_across_clients_and_preserves_multiline_text() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut first = InputLog::open(path.clone()).unwrap();
        let mut second = InputLog::open(path.clone()).unwrap();
        first
            .record("one\n界", ["saved".into()].into_iter())
            .unwrap();
        second
            .record("/title", ["saved".into()].into_iter())
            .unwrap();
        let reopened = InputLog::open(path.clone()).unwrap();
        assert_eq!(reopened.entries.unwrap(), ["saved", "one\n界", "/title"]);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn input_log_recovers_an_incomplete_last_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        fs::write(&path, b"\"saved\"\n\"truncated").unwrap();
        let mut log = InputLog::open(path.clone()).unwrap();
        assert_eq!(log.entries.as_ref().unwrap(), &["saved"]);
        log.record("/title", std::iter::empty()).unwrap();
        assert_eq!(
            InputLog::open(path).unwrap().entries.unwrap(),
            ["saved", "/title"]
        );
    }

    #[test]
    fn invalid_disk_history_does_not_lose_new_attempts() {
        use super::super::tests::app_with;
        let dir = tempfile::tempdir().unwrap();
        let (mut app, _) = app_with(vec![conversation(&["saved"])]);
        app.input_log_dir = Some(dir.path().to_path_buf());
        let key = super::super::super::launcher::hash_key(
            &app.repo_dir,
            &serde_json::json!(app.selected().id),
        )
        .unwrap();
        fs::write(dir.path().join(format!("{key}.jsonl")), b"invalid\n").unwrap();
        app.selected_mut().composer.insert_str("/title");
        app.start_turn();
        app.selected_mut().composer.clear();
        app.navigate_input(true);
        assert_eq!(app.selected().composer.text, "/title");
    }

    #[test]
    fn input_log_keeps_attempt_in_memory_when_persistence_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = InputLog {
            path: Some(dir.path().join("missing/history")),
            entries: None,
        };
        assert!(log.record("failed attempt", std::iter::empty()).is_err());
        assert_eq!(log.entries.unwrap(), ["failed attempt"]);
    }

    #[test]
    fn arrows_without_history_preserve_the_draft() {
        let mut state = conversation(&[]);
        state.composer.insert_str("draft");
        let draft = state.composer.clone();
        state.navigate_input(true, None);
        state.navigate_input(false, None);
        assert_eq!(state.composer, draft);
    }
}
