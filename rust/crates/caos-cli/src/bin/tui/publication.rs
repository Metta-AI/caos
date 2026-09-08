//! Preview and publish a selected set of named workspaces.
use super::*;
use caos_cli::workspaces::{
    publication_order, publication_plan, resolve_publication_plan, PublicationBase,
    PublicationTarget,
};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PLAN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub(super) struct PlanRow {
    pub target: PublicationTarget,
    pub included: bool,
}

#[derive(Clone, Debug)]
pub(super) struct PublishPlanPrompt {
    pub id: u64,
    pub loading: bool,
    pub rows: Vec<PlanRow>,
    pub selected: usize,
    pub edit: Option<(bool, String)>, // true edits the PR base; false edits the branch.
    pub error: Option<String>,
}

impl App {
    pub(super) fn publish_selected(&mut self) {
        if self.selected().publish_plan.is_some() {
            self.confirm_publication();
            return;
        }
        if self.selected().is_busy() {
            self.selected_mut()
                .show_command_error("finish this conversation's operation before publishing it");
            return;
        }
        let selected = match self.selected().require_selected_workspace() {
            Ok(workspace) => workspace.name.clone(),
            Err(error) => {
                self.selected_mut().show_command_error(error);
                return;
            }
        };
        let conversation = self.selected().id.clone();
        let id = NEXT_PLAN.fetch_add(1, Ordering::Relaxed);
        self.selected_mut().publish_plan = Some(PublishPlanPrompt {
            id,
            loading: true,
            rows: Vec::new(),
            selected: 0,
            edit: None,
            error: None,
        });
        let finished_conversation = conversation.clone();
        spawn(
            self.repo_dir.clone(),
            self.tx.clone(),
            move |transport| {
                publication_plan(transport, &conversation)?
                    .into_iter()
                    .map(|target| {
                        Ok(PlanRow {
                            included: target.workspace == selected,
                            target,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()
            },
            move |result| UiMessage::PublicationPlanned {
                conversation: finished_conversation,
                id,
                result,
            },
        );
    }

    pub(super) fn handle_publication_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.selected_mut().publish_plan = None;
            return;
        }
        let Some(mut prompt) = self.selected_mut().publish_plan.take() else {
            return;
        };
        if prompt.loading {
            self.selected_mut().publish_plan = Some(prompt);
            return;
        }
        if let Some((base, input)) = prompt.edit.as_mut() {
            match key.code {
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.clear()
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
                {
                    input.push(ch)
                }
                KeyCode::Enter => {
                    let value = input.trim().to_string();
                    let parent = if *base {
                        value.strip_prefix('@').map(str::to_string)
                    } else {
                        None
                    };
                    let branch = if let Some(parent) = &parent {
                        prompt
                            .rows
                            .iter()
                            .find(|row| &row.target.workspace == parent)
                            .map(|row| row.target.branch.clone())
                    } else {
                        Some(pr_base_branch(&value).to_string())
                    };
                    match branch {
                        Some(branch) => {
                            match conversation_protocol::v3::workspaces::validate_branch(&branch) {
                                Ok(()) => {
                                    let row = &mut prompt.rows[prompt.selected];
                                    if *base {
                                        row.target.base = match parent {
                                            Some(parent) => PublicationBase::Workspace(parent),
                                            None => PublicationBase::Branch(branch),
                                        };
                                    } else {
                                        row.target.branch = branch;
                                        if !row.target.repository.is_empty() {
                                            row.target.diagnostic = None;
                                        }
                                    }
                                    row.included = true;
                                    prompt.edit = None;
                                    prompt.error = None;
                                }
                                Err(error) => prompt.error = Some(error),
                            }
                        }
                        None => prompt.error = Some("unknown base workspace".into()),
                    }
                }
                _ => {}
            }
        } else if !prompt.rows.is_empty() {
            let count = prompt.rows.len();
            match key.code {
                KeyCode::Up => prompt.selected = (prompt.selected + count - 1) % count,
                KeyCode::Down => prompt.selected = (prompt.selected + 1) % count,
                KeyCode::Char(' ') => {
                    let row = &mut prompt.rows[prompt.selected];
                    row.included = !row.included;
                }
                KeyCode::Char('a') => {
                    let include = prompt.rows.iter().any(|row| !row.included);
                    for row in &mut prompt.rows {
                        row.included = include;
                    }
                }
                KeyCode::Char('b') => {
                    let target = &prompt.rows[prompt.selected].target;
                    prompt.edit = Some((
                        true,
                        match &target.base {
                            PublicationBase::Default => String::new(),
                            other => other.to_string(),
                        },
                    ));
                }
                KeyCode::Char('h') => {
                    prompt.edit = Some((false, prompt.rows[prompt.selected].target.branch.clone()))
                }
                KeyCode::Enter => {
                    self.selected_mut().publish_plan = Some(prompt);
                    self.confirm_publication();
                    return;
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.selected_mut().publish_plan = Some(prompt);
                    self.confirm_publication();
                    return;
                }
                _ => {}
            }
        }
        self.selected_mut().publish_plan = Some(prompt);
    }

    fn confirm_publication(&mut self) {
        let Some(mut prompt) = self.selected_mut().publish_plan.take() else {
            return;
        };
        if prompt.loading || prompt.edit.is_some() {
            self.selected_mut().publish_plan = Some(prompt);
            return;
        }
        let all = prompt
            .rows
            .iter()
            .map(|row| row.target.clone())
            .collect::<Vec<_>>();
        let selected = prompt
            .rows
            .iter()
            .filter(|row| row.included)
            .map(|row| row.target.clone())
            .collect::<Vec<_>>();
        let ordered = match publication_order(&selected) {
            Ok(ordered) => ordered,
            Err(error) => {
                prompt.error = Some(error);
                self.selected_mut().publish_plan = Some(prompt);
                return;
            }
        };
        let targets = ordered
            .into_iter()
            .filter_map(|name| {
                prompt
                    .rows
                    .iter()
                    .find(|row| row.target.workspace == name && row.included)
                    .map(|row| row.target.clone())
            })
            .collect::<Vec<_>>();
        if targets.is_empty() {
            prompt.error = Some("select at least one workspace".into());
            self.selected_mut().publish_plan = Some(prompt);
            return;
        }
        if self.selected().is_busy() {
            prompt.error = Some("finish the running operation before publishing".into());
            self.selected_mut().publish_plan = Some(prompt);
            return;
        }
        let conversation = self.selected().id.clone();
        let title = self.selected().title.clone();
        let options = self.selected().turn_options.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.selected_mut().publication_cancel = Some(cancel.clone());
        self.selected_mut().publishing = true;
        self.selected_mut().running = true;
        self.selected_mut().local_turn = true;
        self.selected_mut().status = "preparing publication".into();
        let tx = self.tx.clone();
        let finished_conversation = conversation.clone();
        spawn(
            self.repo_dir.clone(),
            self.tx.clone(),
            move |transport| {
                let targets = resolve_publication_plan(transport, &all, &targets)?;
                caos_cli::publication::publish_plan(
                    transport,
                    &conversation,
                    &title,
                    &options,
                    &targets,
                    &cancel,
                    |event| {
                        let _ = tx.send(UiMessage::Turn {
                            conversation: conversation.clone(),
                            event,
                        });
                    },
                )
            },
            move |result| UiMessage::Published {
                conversation: finished_conversation,
                result,
            },
        );
    }
}
