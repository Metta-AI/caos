//! Preview and publish a selected set of named workspaces.
use super::*;
use caos_cli::workspaces::{
    branch_snapshot, publication_order, publication_plan, publish_prepared_target,
    PublicationTarget,
};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PLAN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub(super) struct PlanRow {
    pub target: PublicationTarget,
    pub included: bool,
    pub pull_request: Option<String>,
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

impl PublishPlanPrompt {
    fn refresh_bases(&mut self) {
        let branches = self
            .rows
            .iter()
            .map(|row| (row.target.workspace.clone(), row.target.branch.clone()))
            .collect::<HashMap<_, _>>();
        for row in &mut self.rows {
            if let Some(parent) = &row.target.parent {
                if let Some(branch) = branches.get(parent) {
                    row.target.base = branch.clone();
                }
            }
        }
    }
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
                        let pull_request = super::super::workspace::lookup_workspace_pr(
                            &target.repository,
                            &target.branch,
                            transport.work_dir(),
                        )?;
                        Ok(PlanRow {
                            included: target.workspace == selected,
                            target,
                            pull_request,
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
                                        row.target.base = branch;
                                        row.target.parent = parent;
                                    } else {
                                        row.target.branch = branch;
                                        row.pull_request = None;
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
                        target
                            .parent
                            .as_ref()
                            .map(|parent| format!("@{parent}"))
                            .unwrap_or_else(|| target.base.clone()),
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
        prompt.refresh_bases();
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
        // Changing a parent's branch changes its dependents' PR bases in this preview.
        prompt.refresh_bases();
        let all = prompt
            .rows
            .iter()
            .map(|row| row.target.clone())
            .collect::<Vec<_>>();
        let ordered = match publication_order(&all) {
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
                let mut completed = Vec::new();
                for target in &targets {
                    let result = publish_target(
                        transport,
                        &conversation,
                        &title,
                        &options,
                        target,
                        &tx,
                        &cancel,
                    );
                    match result {
                        Ok(url) => completed.push(format!("{}: {url}", target.workspace)),
                        Err(error) => {
                            return Err(if completed.is_empty() {
                                error
                            } else {
                                format!(
                                    "{}\n\nPublication stopped at {}: {error}",
                                    completed.join("\n"),
                                    target.workspace
                                )
                            })
                        }
                    }
                }
                Ok(completed.join("\n"))
            },
            move |result| UiMessage::Published {
                conversation: finished_conversation,
                result,
            },
        );
    }
}

fn publish_target(
    transport: &GitTransport,
    conversation: &str,
    title: &str,
    options: &TurnOptions,
    target: &PublicationTarget,
    tx: &Sender<UiMessage>,
    cancel: &AtomicBool,
) -> Result<String, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("publication cancelled".into());
    }
    let load = conversation_load(transport, conversation)?.ok_or("conversation disappeared")?;
    let workspace = load
        .workspaces
        .iter()
        .find(|workspace| workspace.name == target.workspace)
        .ok_or("workspace disappeared")?;
    if workspace.head != target.head || workspace.config != target.previous_config {
        return Err(format!(
            "workspace {:?} changed since the publication preview; review it again",
            target.workspace
        ));
    }
    let base =
        branch_snapshot(transport, &target.repository, &target.base).map_err(
            |error| match &target.parent {
                Some(parent) => format!(
                    "publish base workspace {parent:?} first or include it in the plan: {error}"
                ),
                None => error,
            },
        )?;
    if let Some(parent) = &target.parent {
        if load
            .workspaces
            .iter()
            .find(|workspace| &workspace.name == parent)
            .is_none_or(|workspace| workspace.head != base)
        {
            return Err(format!("base workspace {parent:?} has unpublished changes; include it in the publication plan"));
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("publication cancelled".into());
    }
    let ancestor = remote_base_is_ancestor(&base, &target.head, transport.work_dir())?;
    if !ancestor {
        transport.ensure_pushed(&base)?;
    }
    let mut options = options.clone();
    options.workspace = Some(target.workspace.clone());
    let outcome = run_chat_turn(
        transport,
        &options,
        conversation,
        &publish_turn_message(&target.workspace, &base, ancestor),
        None,
        None,
        |_| {
            if cancel.load(Ordering::Relaxed) {
                let _ = interrupt_request(transport, conversation);
            }
        },
        |event| {
            let _ = tx.send(UiMessage::Turn {
                conversation: conversation.to_string(),
                event,
            });
        },
    )?;
    if outcome.interrupted {
        return Err("publication preparation was interrupted".into());
    }
    let prepared = conversation_load_at(transport, conversation, &outcome.commit)?;
    let prepared = prepared
        .workspaces
        .iter()
        .find(|workspace| workspace.name == target.workspace)
        .ok_or("workspace disappeared during preparation")?;
    validate_prepared_workspace(&base, &prepared.head, transport.work_dir())?;
    if cancel.load(Ordering::Relaxed) {
        return Err("publication cancelled".into());
    }
    let published =
        publish_prepared_target(transport, conversation, target, &prepared.head, &base)?;
    if published.status != conversation_protocol::v3::PublicationStatus::Complete {
        return Err(format!(
            "branch publication is {:?}: {}",
            published.status,
            publication_diagnostic(transport, conversation, &published.publication)?
                .unwrap_or_default()
        ));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(format!(
            "branch {} was published; PR creation was cancelled",
            published.branch
        ));
    }
    super::super::workspace::find_or_open_workspace_pr_in(
        &target.repository,
        conversation,
        &format!("{}: {title}", target.workspace),
        &published,
        &target.base,
        transport.work_dir(),
    )
}
