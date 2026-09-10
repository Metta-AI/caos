//! Browsing commit entries changes inspection only.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct SourceTreePicker {
    pub selected: usize,
}

impl App {
    pub(super) fn open_source_tree_picker(&mut self) {
        let state = self.selected();
        let selected = state
            .source_trees
            .iter()
            .position(|entry| Some(&entry.name) == state.selected_source_tree.as_ref())
            .unwrap_or(0);
        self.source_tree_picker = Some(SourceTreePicker { selected });
        self.palette = None;
    }
    pub(super) fn handle_source_tree_picker_key(&mut self, key: KeyEvent) {
        let Some(mut picker) = self.source_tree_picker.take() else {
            return;
        };
        let count = self.selected().source_trees.len();
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return,
            KeyCode::Up if count > 0 => picker.selected = (picker.selected + count - 1) % count,
            KeyCode::Down if count > 0 => picker.selected = (picker.selected + 1) % count,
            KeyCode::Enter => {
                if let Some(entry) = self.selected().source_trees.get(picker.selected) {
                    let name = entry.name.clone();
                    if let Err(error) = self.selected_mut().select_source_tree(&name) {
                        self.selected_mut().show_command_error(error);
                    }
                    if self.view == View::Tools {
                        self.load_selected_tool_set();
                    }
                }
                return;
            }
            _ => {}
        }
        self.source_tree_picker = Some(picker);
    }
}
