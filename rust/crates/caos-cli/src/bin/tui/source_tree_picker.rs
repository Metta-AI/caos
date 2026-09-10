//! SourceTree navigation stays local; creation goes through the conversation lease.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct SourceTreePicker {
    pub selected: usize,
    pub creating: Option<String>,
    pub source: Option<String>,
    pub attaching: Option<AttachmentForm>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct AttachmentForm {
    pub fields: [String; 3],
    pub selected: usize,
}

impl App {
    pub(super) fn open_source_tree_picker(&mut self) {
        let state = self.selected();
        let selected = state
            .source_trees
            .iter()
            .position(|ws| Some(&ws.name) == state.selected_source_tree.as_ref())
            .unwrap_or(0);
        self.source_tree_picker = Some(SourceTreePicker {
            selected,
            creating: None,
            source: None,
            attaching: None,
        });
        self.palette = None;
    }

    pub(super) fn handle_source_tree_picker_key(&mut self, key: KeyEvent) {
        let Some(mut picker) = self.source_tree_picker.take() else {
            return;
        };
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return;
        }
        if let Some(form) = picker.attaching.as_mut() {
            match key.code {
                KeyCode::Tab => form.selected = (form.selected + 1) % 3,
                KeyCode::BackTab => form.selected = (form.selected + 2) % 3,
                KeyCode::Enter if form.selected < 2 => form.selected += 1,
                KeyCode::Enter => {
                    let [name, repository, reference] =
                        form.fields.clone().map(|value| value.trim().to_string());
                    self.start_source_tree_mutation(
                        "attaching repository",
                        move |transport, conversation| {
                            caos_cli::source_trees::attach(
                                transport,
                                conversation,
                                &name,
                                &repository,
                                (!reference.is_empty()).then_some(reference.as_str()),
                            )?;
                            Ok(format!("Attached {repository} as source tree {name:?}."))
                        },
                    );
                    return;
                }
                KeyCode::Backspace => {
                    form.fields[form.selected].pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
                {
                    form.fields[form.selected].push(ch)
                }
                _ => {}
            }
        } else if let Some(input) = picker.creating.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let name = input.trim().to_string();
                    if let Err(error) =
                        conversation_protocol::v3::paths::validate_source_tree_name(&name)
                    {
                        self.selected_mut().show_command_error(error);
                    } else if let Some(source) = picker.source.clone() {
                        self.start_source_tree_mutation(
                            "creating source tree",
                            move |transport, conversation| {
                                caos_cli::source_trees::create_from_source_tree(
                                    transport,
                                    conversation,
                                    &name,
                                    &source,
                                )?;
                                Ok(format!("Created source tree {name:?} from {source:?}."))
                            },
                        );
                        return;
                    }
                }
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
            let count = self.selected().source_trees.len();
            match key.code {
                KeyCode::Up if count > 0 => picker.selected = (picker.selected + count - 1) % count,
                KeyCode::Down if count > 0 => picker.selected = (picker.selected + 1) % count,
                KeyCode::Enter => {
                    if let Some(ws) = self.selected().source_trees.get(picker.selected) {
                        let name = ws.name.clone();
                        if let Err(error) = self.selected_mut().select_source_tree(&name) {
                            self.selected_mut().show_command_error(error);
                        }
                        if self.view == View::Tools {
                            self.load_selected_tool_set();
                        }
                    }
                    return;
                }
                KeyCode::Char('u') => {
                    let name = self
                        .selected()
                        .source_trees
                        .get(picker.selected)
                        .map(|ws| ws.name.clone());
                    self.update_selected_stack(name);
                    return;
                }
                KeyCode::Char('a') => picker.attaching = Some(AttachmentForm::default()),
                KeyCode::Char('n') => {
                    if let Some(ws) = self.selected().source_trees.get(picker.selected) {
                        picker.source = Some(ws.name.clone());
                        picker.creating = Some(String::new());
                    }
                }
                _ => {}
            }
        }
        self.source_tree_picker = Some(picker);
    }
}

impl ConversationState {
    pub(super) fn source_tree_is_stale(&self, name: &str) -> bool {
        let mut name = name;
        // The protocol rejects cycles; keep the UI bounded even for partial loads.
        for _ in 0..self.source_trees.len() {
            let Some(ws) = self.source_trees.iter().find(|ws| ws.name == name) else {
                return false;
            };
            let Some(conversation_protocol::v3::SourceTreeBase::SourceTree {
                name: parent,
                commit,
            }) = &ws.config.upstream
            else {
                return false;
            };
            let Some(base) = self.source_trees.iter().find(|ws| ws.name == *parent) else {
                return false;
            };
            if base.head != commit.as_str() {
                return true;
            }
            name = parent;
        }
        false
    }
}
