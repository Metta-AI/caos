//! Replay computation only. The shared writer handler owns conversation updates.
use crate::{add_layer::Proposal, objects, rebase_plan, stack};
use conversation_protocol::v3::events::{self, Event};
use conversation_protocol::v3::{CommitInfo, Mode, ObjectStore, Oid, Snapshot, TreeBuilder};
use serde_json::json;
use std::process::Command;

const MAX_HISTORY: usize = 4096;

#[derive(serde::Deserialize)]
pub struct Context {
    pub head: Oid,
    pub scope: String,
    pub committer: String,
}

pub struct Parameters {
    pub stack: String,
    pub plan: Option<String>,
    pub action: Option<String>,
}

struct Checkpoint {
    plan: String,
    layers: Oid,
}

fn text(store: &dyn ObjectStore, root: &Oid, path: &str) -> Result<String, String> {
    let bytes = Snapshot::new(store, root.clone())
        .read(path)?
        .ok_or_else(|| format!("missing plan {path}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{path} must be UTF-8"))
}

/// The input commit supplies immutable accepted results. The editable plan is
/// compared with its last accepted version rather than trusted as its own proof.
fn checkpoint(
    store: &dyn ObjectStore,
    head: &Oid,
    path: &str,
) -> Result<Option<Checkpoint>, String> {
    let mut cursor = head.clone();
    for _ in 0..MAX_HISTORY {
        let commit = store.read_commit(&cursor).map_err(String::from)?;
        if Snapshot::new(store, commit.tree)
            .entry(&format!("{path}/rebase"))?
            .is_none()
        {
            return Ok(None);
        }
        let (_, events) = events::decode(&commit.message)?;
        for event in events {
            let Event::Payload { path: name, bytes } = event else {
                continue;
            };
            if !name.ends_with("/writer-result.json") {
                continue;
            }
            let accepted: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid writer result: {e}"))?;
            if accepted["scope"] != path || accepted["applied"] != true {
                continue;
            }
            let report: serde_json::Value = serde_json::from_str(
                accepted["report"]
                    .as_str()
                    .ok_or("writer result lacks report")?,
            )
            .unwrap_or(serde_json::Value::Null);
            if report["kind"] != "git-rebase-i" || report["stack"] != path {
                continue;
            }
            return Ok(Some(Checkpoint {
                plan: report["plan"]
                    .as_str()
                    .ok_or("replay result lacks executed plan")?
                    .into(),
                layers: Oid::parse(
                    report["output_tree"]
                        .as_str()
                        .ok_or("replay result lacks output layer tree")?,
                    "output layers",
                )?,
            }));
        }
        let Some(parent) = commit.parents.first() else {
            return Ok(None);
        };
        cursor = parent.clone();
    }
    Err("previous replay is beyond conversation history limit".into())
}

pub fn propose(
    store: &mut dyn ObjectStore,
    root: &Oid,
    context: &Context,
    parameters: &Parameters,
) -> Result<Proposal, String> {
    let path = &parameters.stack;
    if context.scope != *path {
        return Err("writer context guards a different stack".into());
    }
    if store.read_commit(&context.head).map_err(String::from)?.tree != *root {
        return Err("writer input differs from the recorded conversation snapshot".into());
    }
    conversation_protocol::v3::paths::validate_source_tree_name(path)?;
    let replay_path = format!("{path}/rebase");
    let plan_path = format!("{replay_path}/plan");
    if let Some(plan) = &parameters.plan {
        if plan != &plan_path {
            return Err(format!(
                "copy the plan to {plan_path} first, then use plan={plan_path}"
            ));
        }
    }
    let current = stack::read_stack(store, root, path)?;
    let replay = stack::directory(store, root, &replay_path)?;
    let previous = checkpoint(store, &context.head, path)?;
    let source = match (parameters.plan.as_deref(), parameters.action.as_deref()) {
        (Some(plan), None) => {
            if previous.is_some() {
                return Err("this stack already has an active replay; use continue".into());
            }
            if store
                .read_tree(&replay)
                .map_err(String::from)?
                .iter()
                .any(|entry| entry.name != "plan")
            {
                return Err("new rebase directory must contain only its plan".into());
            }
            text(store, root, plan)?
        }
        (None, Some("continue")) => {
            let previous = previous
                .as_ref()
                .ok_or("this stack has no started replay")?;
            let layers =
                match Snapshot::new(store, root.clone()).entry(&format!("{replay_path}/stack"))? {
                    None => conversation_protocol::v3::oid::empty_tree(),
                    Some(entry) if entry.mode == Mode::Tree => entry.oid,
                    Some(_) => return Err("saved output layers must be a directory".into()),
                };
            if layers != previous.layers {
                return Err(
                    "recorded output layers are not editable; edit the unfinished plan".into(),
                );
            }
            text(store, root, &format!("{replay_path}/plan"))?
        }
        _ => return Err("provide plan to start, or action=continue to resume".into()),
    };
    let original = current.numbered_tree(store)?;
    let draft = if previous.is_some() && rebase_plan::Plan::parse(&source)?.uses_draft() {
        let work = Snapshot::new(store, root.clone())
            .entry(&format!("{replay_path}/work"))?
            .ok_or("paused replay has no work draft")?;
        if work.mode != Mode::Commit {
            return Err("replay work must be a source gitlink".into());
        }
        Some(store.read_commit(&work.oid).map_err(String::from)?.tree)
    } else {
        None
    };
    let mut backend = Backend { store };
    let outcome = if let Some(previous) = &previous {
        rebase_plan::resume(&mut backend, &source, &previous.plan, draft.as_ref())?
    } else {
        rebase_plan::start(
            &mut backend,
            &source,
            original.clone(),
            objects::parse_signature(&context.committer)?,
        )?
    };
    if outcome.pause.is_none()
        && rebase_plan::Plan::parse(&outcome.plan)?.original.as_ref() != Some(&original)
    {
        return Err(
            "original numbered stack entries changed during replay; result was not applied".into(),
        );
    }
    let mut layers = TreeBuilder::from(None);
    for (number, layer) in outcome.layers.iter().enumerate() {
        layers.put_oid(&layer.name, Mode::Commit, layer.tip.clone());
        layers.put(
            &format!("{number:02}.base"),
            Mode::Blob,
            layer.base.encode_line(),
        );
    }
    let layer_tree = layers.build(store)?;
    let mut replacement = TreeBuilder::from(Some(current.directory));
    if let Some(pause) = &outcome.pause {
        replacement.put("rebase/plan", Mode::Blob, outcome.plan.as_bytes().to_vec());
        replacement.delete("rebase/stack");
        replacement.put_oid("rebase/stack", Mode::Tree, layer_tree.clone());
        replacement.put_oid("rebase/work", Mode::Commit, pause.draft_commit.clone());
        if let Some(conflicts) = &pause.conflicts {
            replacement.put("rebase/conflicts", Mode::Blob, conflicts.clone());
        }
    } else {
        for entry in stack::numbered_entries(&current.entries) {
            replacement.delete(&entry.name);
        }
        for entry in store.read_tree(&layer_tree).map_err(String::from)? {
            replacement.put_oid(&entry.name, entry.mode, entry.oid);
        }
        replacement.delete("rebase");
    }
    let replacement = replacement.build(store)?;
    let mut proposal = TreeBuilder::from(Some(root.clone()));
    proposal.delete(path);
    proposal.put_oid(path, Mode::Tree, replacement);
    let tree = proposal.build(store)?;
    let report = json!({
        "kind":"git-rebase-i", "stack":path,
        "status":outcome.pause.as_ref().map(|pause| pause.reason.as_str()).unwrap_or("complete"),
        "plan":outcome.plan, "output_tree":layer_tree,
        "layers":outcome.layers.iter().map(|layer| json!({"name":layer.name,"commit":layer.tip})).collect::<Vec<_>>(),
        "instruction":outcome.pause.as_ref().map(|pause| &pause.instruction),
        "draft":outcome.pause.as_ref().map(|_| format!("{replay_path}/work")),
        "conflicts":outcome.pause.as_ref().map(|_| format!("{replay_path}/conflicts")),
    });
    Ok(Proposal { tree, report })
}

struct Backend<'s> {
    store: &'s mut dyn ObjectStore,
}

impl rebase_plan::Backend for Backend<'_> {
    fn read_commit(&mut self, oid: &Oid) -> Result<CommitInfo, String> {
        self.store.read_commit(oid).map_err(String::from)
    }

    fn merge_trees(
        &mut self,
        base: &Oid,
        ours: &Oid,
        theirs: &Oid,
    ) -> Result<rebase_plan::Merge, String> {
        let output = Command::new("caos")
            .args([
                "git-merge-tree",
                base.as_str(),
                ours.as_str(),
                theirs.as_str(),
            ])
            .output()
            .map_err(|e| format!("running merge-tree: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "merge-tree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("invalid merge-tree result: {e}"))?;
        let tree = Oid::parse(
            value["tree"].as_str().ok_or("merge-tree lacks tree")?,
            "merged tree",
        )?;
        let report = value["conflicts"]
            .as_str()
            .ok_or("merge-tree lacks report")?;
        Ok(rebase_plan::Merge {
            tree,
            conflicts: (!report.is_empty()).then(|| report.as_bytes().to_vec()),
        })
    }

    fn commit_tree(&mut self, commit: &CommitInfo) -> Result<Oid, String> {
        self.store.write_commit(commit).map_err(String::from)
    }
}

#[cfg(test)]
#[path = "rebase_tests.rs"]
mod tests;
