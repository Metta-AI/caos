//! Prepare, validate, publish and open a PR for an explicit workspace plan.
//! Client focus cannot retarget this operation; progress and cancellation are inputs.
use super::*;
use crate::host_git::{
    find_or_open_workspace_pr_in, remote_base_is_ancestor, validate_prepared_workspace,
};
use crate::workspaces::{branch_snapshot, publish_prepared_target, ResolvedPublicationTarget};
use std::sync::atomic::{AtomicBool, Ordering};

pub fn publish_plan(
    transport: &GitTransport,
    conversation: &str,
    title: &str,
    options: &TurnOptions,
    targets: &[ResolvedPublicationTarget],
    cancel: &AtomicBool,
    mut on_event: impl FnMut(TurnEvent),
) -> Result<String, String> {
    let mut completed = Vec::new();
    for target in targets {
        match publish_target(
            transport,
            conversation,
            title,
            options,
            target,
            cancel,
            &mut on_event,
        ) {
            Ok(url) => completed.push(format!("{}: {url}", target.target.workspace)),
            Err(error) => {
                return Err(if completed.is_empty() {
                    error
                } else {
                    format!(
                        "{}\n\nPublication stopped at {}: {error}",
                        completed.join("\n"),
                        target.target.workspace
                    )
                })
            }
        }
    }
    Ok(completed.join("\n"))
}

fn publish_target(
    transport: &GitTransport,
    conversation: &str,
    title: &str,
    options: &TurnOptions,
    resolved: &ResolvedPublicationTarget,
    cancel: &AtomicBool,
    on_event: impl FnMut(TurnEvent),
) -> Result<String, String> {
    let target = &resolved.target;
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
        branch_snapshot(transport, &target.repository, &resolved.base_branch).map_err(|error| {
            match target.base.parent() {
                Some(parent) => format!(
                    "publish base workspace {parent:?} first or include it in the plan: {error}"
                ),
                None => error,
            }
        })?;
    if let Some(parent) = target.base.parent() {
        if load
            .workspaces
            .iter()
            .find(|workspace| workspace.name == parent)
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
        on_event,
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
        publish_prepared_target(transport, conversation, resolved, &prepared.head, &base)?;
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
    find_or_open_workspace_pr_in(
        &target.repository,
        conversation,
        &format!("{}: {title}", target.workspace),
        &published,
        &resolved.base_branch,
        transport.work_dir(),
    )
}

fn publish_turn_message(workspace: &str, target: &str, base_is_ancestor: bool) -> String {
    let preparation = if base_is_ancestor {
        format!("The selected PR base `{target}` is already an ancestor of this workspace; do not merge it again.")
    } else {
        let arguments = serde_json::json!({"workspace": workspace, "theirs": target});
        format!("First call the existing `merge` tool with these arguments: {arguments}. Resolve every entry in `.caos/conflicts`, then remove `.caos/conflicts`.")
    };
    format!("Prepare workspace {workspace:?} for publication. {preparation} Build and test that workspace. Finish only when it is ready to publish.")
}
