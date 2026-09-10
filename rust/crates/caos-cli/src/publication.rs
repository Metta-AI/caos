//! Prepare, validate, publish and open a PR for an explicit source tree plan.
//! Client focus cannot retarget this operation; progress and cancellation are inputs.
use super::*;
use crate::host_git::{
    find_or_open_source_tree_pr_in, remote_base_is_ancestor, validate_prepared_source_tree,
};
use crate::source_trees::{branch_snapshot, publish_prepared_target, ResolvedPublicationTarget};
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
            Ok(url) => completed.push(format!("{}: {url}", target.target.source_tree)),
            Err(error) => {
                return Err(if completed.is_empty() {
                    error
                } else {
                    format!(
                        "{}\n\nPublication stopped at {}: {error}",
                        completed.join("\n"),
                        target.target.source_tree
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
    let source_tree = load
        .source_trees
        .iter()
        .find(|source_tree| source_tree.name == target.source_tree)
        .ok_or("source tree disappeared")?;
    if source_tree.head != target.head
        || source_tree.config.publication != target.previous_config.publication
    {
        return Err(format!(
            "source tree {:?} changed since the publication preview; review it again",
            target.source_tree
        ));
    }
    let base =
        branch_snapshot(transport, &target.repository, &resolved.base_branch).map_err(|error| {
            match target.base.parent() {
                Some(parent) => format!(
                    "publish base source tree {parent:?} first or include it in the plan: {error}"
                ),
                None => error,
            }
        })?;
    if let Some(parent) = target.base.parent() {
        if load
            .source_trees
            .iter()
            .find(|source_tree| source_tree.name == parent)
            .is_none_or(|source_tree| source_tree.head != base)
        {
            return Err(format!("base source tree {parent:?} has unpublished changes; include it in the publication plan"));
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
    options.source_tree = Some(target.source_tree.clone());
    let outcome = run_chat_turn(
        transport,
        &options,
        conversation,
        &publish_turn_message(&target.source_tree, &base, ancestor),
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
        .source_trees
        .iter()
        .find(|source_tree| source_tree.name == target.source_tree)
        .ok_or("source tree disappeared during preparation")?;
    validate_prepared_source_tree(&base, &prepared.head, transport.work_dir())?;
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
    find_or_open_source_tree_pr_in(
        &target.repository,
        conversation,
        &format!("{}: {title}", target.source_tree),
        &published,
        &resolved.base_branch,
        transport.work_dir(),
    )
}

fn publish_turn_message(source_tree: &str, target: &str, base_is_ancestor: bool) -> String {
    let preparation = if base_is_ancestor {
        format!("The selected PR base `{target}` is already an ancestor of this source tree; do not merge it again.")
    } else {
        let arguments = serde_json::json!({"source_tree": source_tree, "theirs": target});
        format!("First call the existing `merge` tool with these arguments: {arguments}. Resolve every entry in `.caos/conflicts`, then remove `.caos/conflicts`.")
    };
    format!("Prepare source tree {source_tree:?} for publication. {preparation} Build and test that source tree. Finish only when it is ready to publish.")
}
