//! Preview and confirm publication of an explicitly named gitlink.
use super::*;
use caos_cli::source_trees::PublicationTarget;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PREVIEW: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub(super) struct PublishPrompt {
    pub id: u64,
    pub loading: bool,
    pub target: Option<PublicationTarget>,
    pub error: Option<String>,
    pub branch_only: bool,
}

impl App {
    pub(super) fn run_publication(&mut self, arguments: &str, branch_only: bool) {
        let usage = if branch_only {
            "usage: /publish-branch <conversation/gitlink> [remote-URL]"
        } else {
            "usage: /pr <conversation/gitlink> <base-remote-branch> [remote-URL]"
        };
        let required = if branch_only { 1 } else { 2 };
        let parts = match shell_words::split(arguments) {
            Ok(parts) if (required..=required + 1).contains(&parts.len()) => parts,
            _ => {
                self.selected_mut().show_command_error(usage);
                return;
            }
        };
        if self.selected().is_busy() {
            self.selected_mut()
                .show_command_error("finish this conversation's operation before publishing it");
            return;
        }
        let source = parts[0].clone();
        let base = (!branch_only).then(|| parts[1].clone());
        let repository = parts.get(if branch_only { 1 } else { 2 }).cloned();
        let conversation = self.selected().id.clone();
        let id = NEXT_PREVIEW.fetch_add(1, Ordering::Relaxed);
        self.selected_mut().publish_plan = Some(PublishPrompt {
            id,
            loading: true,
            target: None,
            error: None,
            branch_only,
        });
        let finished = conversation.clone();
        spawn(
            self.repo_dir.clone(),
            self.tx.clone(),
            move |transport| {
                caos_cli::source_trees::prepare_publication(
                    transport,
                    &conversation,
                    &source,
                    base.as_deref(),
                    repository.as_deref(),
                )
            },
            move |result| UiMessage::PublicationPlanned {
                conversation: finished,
                id,
                result,
            },
        );
    }

    pub(super) fn handle_publication_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.selected_mut().publish_plan = None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_mut().publish_plan = None;
            }
            KeyCode::Enter => self.confirm_publication(),
            _ => {}
        }
    }

    fn confirm_publication(&mut self) {
        let Some(mut prompt) = self.selected_mut().publish_plan.take() else {
            return;
        };
        if prompt.loading || prompt.error.is_some() || prompt.target.is_none() {
            self.selected_mut().publish_plan = Some(prompt);
            return;
        }
        if self.selected().is_busy() {
            prompt.error = Some(
                "finish the running operation before publishing; run the command again".into(),
            );
            self.selected_mut().publish_plan = Some(prompt);
            return;
        }
        let target = prompt.target.take().expect("preview completed");
        let conversation = self.selected().id.clone();
        let title = self.selected().title.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.selected_mut().publication_cancel = Some(cancel.clone());
        self.selected_mut().publishing = true;
        self.selected_mut().status = format!("publishing {}", target.source_tree);
        let finished = conversation.clone();
        spawn(
            self.repo_dir.clone(),
            self.tx.clone(),
            move |transport| {
                if cancel.load(Ordering::Relaxed) {
                    return Err("publication cancelled".into());
                }
                if prompt.branch_only {
                    let published = caos_cli::source_trees::publish_branch_target(
                        transport,
                        &conversation,
                        &target,
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
                        published.branch, target.repository
                    ))
                } else {
                    caos_cli::publication::publish_target(
                        transport,
                        &conversation,
                        &title,
                        &target,
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
