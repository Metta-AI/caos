//! Select snapshots and explicitly confirm their publication destination.
use super::*;
use caos_cli::source_trees::{
    publication_order, publication_plan, publication_provenance, resolve_publication_target,
    stack_directory, PublicationDestination, PublicationTarget,
};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PLAN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub(super) struct PlanRow {
    pub target: PublicationTarget,
    pub destination: PublicationDestination,
    pub included: bool,
}

#[derive(Clone, Debug)]
pub(super) struct PublishPlanPrompt {
    pub id: u64,
    pub loading: bool,
    pub rows: Vec<PlanRow>,
    pub selected: usize,
    pub error: Option<String>,
    pub branch_only: bool,
    pub previewed: bool,
    pub editing: bool,
    pub field: usize,
}

impl PublishPlanPrompt {
    pub(super) fn sync_destination(&mut self) {
        let row = &self.rows[self.selected];
        let directory = stack_directory(&row.target.source_tree).to_owned();
        let destination = row.destination.clone();
        for row in &mut self.rows {
            if stack_directory(&row.target.source_tree) == directory {
                row.destination = destination.clone();
            }
        }
        self.previewed = false;
        self.error = None;
    }
}

impl App {
    pub(super) fn publish_selected(&mut self) {
        if self.selected().publish_plan.is_some() {
            self.confirm_publication();
        } else {
            self.open_publication(false);
        }
    }

    pub(super) fn open_publication(&mut self, branch_only: bool) {
        if self.selected().is_busy() {
            self.selected_mut()
                .show_command_error("finish this conversation's operation before publishing it");
            return;
        }
        let selected = match self.selected().require_selected_source_tree() {
            Ok(source) => source.name.clone(),
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
            branch_only,
            previewed: false,
            editing: false,
            field: 0,
        });
        let finished = conversation.clone();
        spawn(
            self.repo_dir.clone(),
            self.tx.clone(),
            move |transport| {
                let mut destinations =
                    std::collections::BTreeMap::<String, PublicationDestination>::new();
                publication_plan(transport, &conversation)?
                    .into_iter()
                    .filter(|target| {
                        if branch_only {
                            target.source_tree == selected
                        } else {
                            target.base_commit.is_some()
                        }
                    })
                    .map(|target| {
                        let directory = stack_directory(&target.source_tree).to_owned();
                        let destination = if let Some(saved) = destinations.get(&directory) {
                            saved.clone()
                        } else {
                            let destination = match crate::tui::launcher::publication_destination(
                                transport.work_dir(),
                                &conversation,
                                &target.source_tree,
                            )? {
                                Some(saved) => saved,
                                None => publication_provenance(
                                    transport,
                                    &conversation,
                                    &target.source_tree,
                                )?
                                .unwrap_or_default(),
                            };
                            destinations.insert(directory, destination.clone());
                            destination
                        };
                        Ok(PlanRow {
                            included: stack_directory(&target.source_tree)
                                == stack_directory(&selected),
                            target,
                            destination,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()
            },
            move |result| UiMessage::PublicationPlanned {
                conversation: finished,
                id,
                result,
            },
        );
    }

    pub(super) fn handle_publication_key(&mut self, key: KeyEvent) {
        let Some(mut prompt) = self.selected_mut().publish_plan.take() else {
            return;
        };
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            if prompt.editing {
                prompt.editing = false;
                self.selected_mut().publish_plan = Some(prompt);
            }
            return;
        }
        if !prompt.loading && !prompt.rows.is_empty() {
            if prompt.editing {
                let row = &mut prompt.rows[prompt.selected];
                let value = if prompt.field == 0 {
                    &mut row.destination.repository
                } else {
                    &mut row.destination.base_branch
                };
                match key.code {
                    KeyCode::Tab | KeyCode::BackTab => prompt.field = 1 - prompt.field,
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        value.clear()
                    }
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        value.push(ch)
                    }
                    KeyCode::Enter => prompt.editing = false,
                    _ => {}
                }
                prompt.sync_destination();
            } else {
                let count = prompt.rows.len();
                match key.code {
                    KeyCode::Up => prompt.selected = (prompt.selected + count - 1) % count,
                    KeyCode::Down => prompt.selected = (prompt.selected + 1) % count,
                    KeyCode::Char('e') => {
                        prompt.editing = true;
                        prompt.field = 0;
                        prompt.previewed = false;
                    }
                    KeyCode::Char(' ') => {
                        prompt.rows[prompt.selected].included ^= true;
                        prompt.previewed = false;
                    }
                    KeyCode::Char('a') => {
                        let include = prompt.rows.iter().any(|row| !row.included);
                        for row in &mut prompt.rows {
                            row.included = include;
                        }
                        prompt.previewed = false;
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
        }
        self.selected_mut().publish_plan = Some(prompt);
    }

    fn confirm_publication(&mut self) {
        let Some(mut prompt) = self.selected_mut().publish_plan.take() else {
            return;
        };
        if prompt.loading || prompt.editing {
            self.selected_mut().publish_plan = Some(prompt);
            return;
        }
        if !prompt.rows.iter().any(|row| row.included) {
            prompt.error = Some("select at least one source tree".into());
            self.selected_mut().publish_plan = Some(prompt);
            return;
        }
        if !prompt.previewed {
            prompt.id = NEXT_PLAN.fetch_add(1, Ordering::Relaxed);
            prompt.loading = true;
            prompt.error = None;
            let id = prompt.id;
            let mut rows = prompt.rows.clone();
            let branch_only = prompt.branch_only;
            let conversation = self.selected().id.clone();
            // Mark ready only when this asynchronous preview succeeds.
            self.selected_mut().publish_plan = Some(prompt);
            spawn(
                self.repo_dir.clone(),
                self.tx.clone(),
                move |transport| {
                    for row in rows.iter_mut().filter(|row| row.included) {
                        resolve_publication_target(
                            transport,
                            &mut row.target,
                            &row.destination,
                            branch_only,
                        )?;
                    }
                    Ok(rows)
                },
                move |result| UiMessage::PublicationPlanned {
                    conversation,
                    id,
                    result,
                },
            );
            return;
        }
        let selected = prompt
            .rows
            .iter()
            .filter(|row| row.included)
            .cloned()
            .collect::<Vec<_>>();
        let targets = selected
            .iter()
            .map(|row| row.target.clone())
            .collect::<Vec<_>>();
        if let Err(error) = publication_order(&targets) {
            prompt.error = Some(error);
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
        let finished = conversation.clone();
        spawn(
            self.repo_dir.clone(),
            self.tx.clone(),
            move |transport| {
                for row in &selected {
                    crate::tui::launcher::remember_publication(
                        transport.work_dir(),
                        &conversation,
                        &row.target.source_tree,
                        &row.destination,
                    )?;
                }
                if cancel.load(Ordering::Relaxed) {
                    return Err("publication cancelled".into());
                }
                if prompt.branch_only {
                    let published = caos_cli::source_trees::publish_branch_target(
                        transport,
                        &conversation,
                        &targets[0],
                    )?;
                    if published.status != conversation_protocol::v3::PublicationStatus::Complete {
                        return Err(format!(
                            "branch publication is {:?}: {}",
                            published.status,
                            caos_cli::publication_diagnostic(
                                transport,
                                &conversation,
                                &published.publication
                            )?
                            .unwrap_or_default()
                        ));
                    }
                    Ok(format!(
                        "Published {} to {}",
                        published.branch, targets[0].repository
                    ))
                } else {
                    caos_cli::publication::publish_plan(
                        transport,
                        &conversation,
                        &title,
                        &targets,
                        &cancel,
                    )
                }
            },
            move |result| UiMessage::Published {
                conversation: finished,
                result,
            },
        );
    }
}
