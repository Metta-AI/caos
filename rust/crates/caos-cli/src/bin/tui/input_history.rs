use super::{ui, App, Composer, ConversationState, EntryRole};
use ratatui_core::buffer::CellWidth;

/// A browsing session freezes the transcript order while remote updates arrive.
/// Each entry retains edits, and the last entry is the user's original draft.
pub(super) struct InputHistory {
    entries: Vec<Composer>,
    position: usize,
}

impl App {
    pub(super) fn navigate_input(&mut self, up: bool) {
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
            // The durable transcript supplies history on every TUI restart.
            // Peer, agent, and system messages are never offered as user input.
            let mut entries: Vec<_> = self
                .transcript
                .iter()
                .filter(|entry| entry.role == EntryRole::Human && !entry.text.trim().is_empty())
                .map(|entry| {
                    let mut composer = Composer::default();
                    composer.insert_str(&entry.text);
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
    fn arrows_without_history_preserve_the_draft() {
        let mut state = conversation(&[]);
        state.composer.insert_str("draft");
        let draft = state.composer.clone();
        state.navigate_input(true, None);
        state.navigate_input(false, None);
        assert_eq!(state.composer, draft);
    }
}
