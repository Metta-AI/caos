//! Preview and publish a selected set of named source_trees.
use super::*;
use caos_cli::source_trees::{publication_order, publication_plan, PublicationTarget};
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
        let selected = match self.selected().require_selected_source_tree() {
            Ok(source_tree) => source_tree.name.clone(),
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
                            included: target.source_tree.rsplit_once('/').map(|(dir, _)| dir)
                                == selected.rsplit_once('/').map(|(dir, _)| dir),
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
        if !prompt.rows.is_empty() {
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
        if prompt.loading {
            self.selected_mut().publish_plan = Some(prompt);
            return;
        }
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
                    .find(|row| row.target.source_tree == name && row.included)
                    .map(|row| row.target.clone())
            })
            .collect::<Vec<_>>();
        if targets.is_empty() {
            prompt.error = Some("select at least one source tree".into());
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
        let cancel = Arc::new(AtomicBool::new(false));
        self.selected_mut().publication_cancel = Some(cancel.clone());
        self.selected_mut().publishing = true;
        self.selected_mut().status = "publishing".into();
        let finished_conversation = conversation.clone();
        spawn(
            self.repo_dir.clone(),
            self.tx.clone(),
            move |transport| {
                caos_cli::publication::publish_plan(
                    transport,
                    &conversation,
                    &title,
                    &targets,
                    &cancel,
                )
            },
            move |result| UiMessage::Published {
                conversation: finished_conversation,
                result,
            },
        );
    }
}
