//! Prepare, validate, publish and open a PR for an explicit source tree plan.
//! Client focus cannot retarget this operation; progress and cancellation are inputs.
use super::*;
use crate::host_git::{
    find_or_open_source_tree_pr_in, remote_base_is_ancestor, validate_prepared_source_tree,
};
use crate::source_trees::{branch_snapshot, PublicationTarget};
use std::sync::atomic::{AtomicBool, Ordering};

pub fn publish_plan(
    transport: &GitTransport,
    conversation: &str,
    title: &str,
    targets: &[PublicationTarget],
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut completed = Vec::new();
    for target in targets {
        match publish_target(transport, conversation, title, target, cancel) {
            Ok(url) => completed.push(format!("{}: {url}", target.source_tree)),
            Err(error) => {
                return Err(if completed.is_empty() {
                    error
                } else {
                    format!(
                        "{}\n\nPublication stopped at {}: {error}",
                        completed.join("\n"),
                        target.source_tree
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
    resolved: &PublicationTarget,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let target = resolved;
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
        || Conversation::open(
            &open_store(transport)?,
            &oid(&load.snapshot.head, "conversation")?,
        )?
        .base_url(&target.source_tree)?
            != target.base_url
    {
        return Err(format!(
            "source tree {:?} changed since the publication preview; review it again",
            target.source_tree
        ));
    }
    let base =
        branch_snapshot(transport, &target.repository, &resolved.base_branch).map_err(|error| {
            match target.parent.as_deref() {
                Some(parent) => format!(
                    "publish base source tree {parent:?} first or include it in the plan: {error}"
                ),
                None => error,
            }
        })?;
    if let Some(parent) = target.parent.as_deref() {
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
    validate_prepared_source_tree(&base, &target.head, transport.work_dir())?;
    if !remote_base_is_ancestor(&base, &target.head, transport.work_dir())? {
        return Err("PR base is not an ancestor; ask the agent to integrate it before previewing publication".into());
    }
    let published = crate::source_trees::publish_target(transport, conversation, resolved, &base)?;
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
