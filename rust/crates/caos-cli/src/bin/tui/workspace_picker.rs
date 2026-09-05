//! Workspace navigation stays local; creation goes through the conversation lease.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct WorkspacePicker {
    pub selected: usize,
    pub creating: Option<String>,
    pub source: Option<String>,
    pub stacked: bool,
}

impl App {
    pub(super) fn open_workspace_picker(&mut self) {
        let state = self.selected();
        let selected = state
            .workspaces
            .iter()
            .position(|ws| Some(&ws.name) == state.selected_workspace.as_ref())
            .unwrap_or(0);
        self.workspace_picker = Some(WorkspacePicker {
            selected,
            creating: None,
            source: None,
            stacked: false,
        });
        self.palette = None;
    }

    pub(super) fn handle_workspace_picker_key(&mut self, key: KeyEvent) {
        let Some(mut picker) = self.workspace_picker.take() else {
            return;
        };
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return;
        }
        if let Some(input) = picker.creating.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let name = input.trim().to_string();
                    if let Err(error) =
                        conversation_protocol::v3::paths::validate_workspace_name(&name)
                    {
                        self.selected_mut().show_command_error(error);
                    } else if let Some(source) = picker.source.clone() {
                        let stacked = picker.stacked;
                        self.start_workspace_mutation(
                            "creating workspace",
                            move |transport, conversation| {
                                caos_cli::workspaces::create_from_workspace(
                                    transport,
                                    conversation,
                                    &name,
                                    &source,
                                    stacked,
                                )?;
                                Ok(format!("Created workspace {name:?} from {source:?}."))
                            },
                        );
                        return;
                    }
                }
                KeyCode::Tab => picker.stacked = !picker.stacked,
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
                {
                    input.push(ch)
                }
                _ => {}
            }
        } else {
            let count = self.selected().workspaces.len();
            match key.code {
                KeyCode::Up if count > 0 => picker.selected = (picker.selected + count - 1) % count,
                KeyCode::Down if count > 0 => picker.selected = (picker.selected + 1) % count,
                KeyCode::Enter => {
                    if let Some(ws) = self.selected().workspaces.get(picker.selected) {
                        let name = ws.name.clone();
                        if let Err(error) = self.selected_mut().select_workspace(&name) {
                            self.selected_mut().show_command_error(error);
                        }
                        if self.view == View::Tools {
                            self.load_selected_tool_set();
                        }
                    }
                    return;
                }
                KeyCode::Char('n') => {
                    if let Some(ws) = self.selected().workspaces.get(picker.selected) {
                        picker.source = Some(ws.name.clone());
                        picker.creating = Some(String::new());
                    }
                }
                _ => {}
            }
        }
        self.workspace_picker = Some(picker);
    }
}
