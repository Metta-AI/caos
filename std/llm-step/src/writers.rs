//! Repository writers propose a conversation tree; the harness applies it once
//! under a guarded path, without reconciling rewritten source gitlinks.
use super::*;
use conversation_protocol::v3::Snapshot;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Context {
    pub head: Oid,
    pub scope: String,
    pub tool_path: String,
    pub committer: String,
}

pub(super) fn payload_path(record: &CallRecord, name: &str) -> String {
    format!(
        "{}/{name}",
        paths::call_payload_dir(record.request.as_str(), record.round, &record.id)
    )
}

pub(super) fn input<S: progress::RefStore>(
    state: &progress::State<S>,
    record: &CallRecord,
) -> Result<Option<Context>, String> {
    state
        .conversation()?
        .optional_payload(&payload_path(record, "writer-input.json"))?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid writer input: {e}"))
        })
        .transpose()
}

/// Writer declarations come from the evaluated image, which may not exist in
/// the unevaluated source. Its lookup source never becomes the writer's input.
pub(super) fn prepare<S: progress::RefStore>(
    state: &progress::State<S>,
    head: &Oid,
    tool: &tools::TreeTool,
    bound: &[(String, String)],
) -> Result<Option<Context>, String> {
    let Some(parameter) = &tool.writer else {
        return Ok(None);
    };
    let scope = bound
        .iter()
        .find(|(name, _)| name == parameter)
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| format!("writer scope parameter {parameter} is missing"))?;
    paths::validate_source_tree_name(scope)?;
    if !unchanged(state, head, scope)? {
        return Err(format!(
            "{scope} changed while the tool was evaluated; no writer was started"
        ));
    }
    context(state, state.head(), scope, &tool.name).map(Some)
}

fn context<S: progress::RefStore>(
    state: &progress::State<S>,
    head: &Oid,
    scope: &str,
    tool_path: &str,
) -> Result<Context, String> {
    paths::validate_source_tree_name(scope)?;
    let mut signature = inherited_signature(state.store(), head)?;
    signature.time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "clock precedes Unix epoch")?
        .as_secs() as i64;
    Ok(Context {
        head: head.clone(),
        scope: scope.into(),
        tool_path: tool_path.into(),
        committer: format!(
            "{} <{}> {} {}",
            signature.name, signature.email, signature.time, signature.offset
        ),
    })
}

pub(super) fn complete(
    state: &mut progress::State,
    record: &CallRecord,
    context: &Context,
    result: &str,
) -> Result<(), String> {
    let output = Oid::parse(&cas_hash(result)?, "writer result")?;
    state.fetch_object(&output)?;
    let entries = state.store().read_tree(&output).map_err(String::from)?;
    let proposal = entries
        .iter()
        .find(|e| e.name == "proposal" && e.mode == Mode::Tree)
        .ok_or("writer result must contain a proposal tree")?
        .oid
        .clone();
    let report = entries
        .iter()
        .find(|e| e.name == "report" && e.mode == Mode::Blob)
        .ok_or("writer result must contain a report blob")?;
    let report = String::from_utf8(state.store().read_blob(&report.oid).map_err(String::from)?)
        .map_err(|_| "writer report must be UTF-8")?;
    let proposal_commit =
        mint_source_tree_commit(state, &proposal, &context.head, "repository writer")?;
    state.publish_commit(&proposal_commit)?;
    apply(state, record, context, &proposal_commit, &report)
}

/// Comparing only the final hash misses delete-and-recreate. Every intervening
/// conversation snapshot must retain the guarded path from the captured input.
fn unchanged<S: progress::RefStore>(
    state: &progress::State<S>,
    input: &Oid,
    scope: &str,
) -> Result<bool, String> {
    let base = state.store().read_commit(input).map_err(String::from)?;
    let expected = Snapshot::new(state.store(), base.tree)
        .entry(scope)?
        .map(|e| (e.mode, e.oid));
    let mut head = state.head().clone();
    for _ in 0..MAX_SPINE_WALK {
        if head == *input {
            return Ok(true);
        }
        let commit = state.store().read_commit(&head).map_err(String::from)?;
        let actual = Snapshot::new(state.store(), commit.tree)
            .entry(scope)?
            .map(|e| (e.mode, e.oid));
        if actual != expected {
            return Ok(false);
        }
        let Some(parent) = commit.parents.first() else {
            return Ok(false);
        };
        head = parent.clone();
    }
    Err("writer input is beyond conversation history limit".into())
}

fn apply<S: progress::RefStore>(
    state: &mut progress::State<S>,
    started: &CallRecord,
    context: &Context,
    proposal: &Oid,
    report: &str,
) -> Result<(), String> {
    if started.input_commit.as_ref() != Some(&context.head) || started.source_tree_name.is_some() {
        return Err("writer input does not match its recorded conversation snapshot".into());
    }
    let before = state
        .store()
        .read_commit(&context.head)
        .map_err(String::from)?
        .tree;
    let after = state
        .store()
        .read_commit(proposal)
        .map_err(String::from)?
        .tree;
    let changes = conversation_protocol::v3::tree::diff(state.store(), Some(&before), &after)?;
    if changes.iter().any(|change| {
        !(change.path == context.scope || change.path.starts_with(&format!("{}/", context.scope)))
            || change.path == ".caos"
            || change.path.starts_with(".caos/")
    }) {
        return Err("writer proposal changes paths outside its declared scope".into());
    }
    let edits: FileEdits = changes
        .into_iter()
        .map(|change| {
            let value = change
                .after
                .map(|(mode, oid)| {
                    let bytes = if matches!(mode, Mode::Tree | Mode::Commit) {
                        oid.encode_line()
                    } else {
                        state.store().read_blob(&oid).map_err(String::from)?
                    };
                    Ok::<_, String>((mode, bytes))
                })
                .transpose()?;
            Ok((change.path, value))
        })
        .collect::<Result<_, String>>()?;
    for _ in 0..32 {
        state.reload()?;
        if state
            .conversation()?
            .tool(&started.request, started.round, &started.id)?
            .is_some_and(|record| record.is_terminal())
        {
            return Ok(());
        }
        let accepted = unchanged(state, &context.head, &context.scope)?;
        let files = if accepted { edits.clone() } else { Vec::new() };
        let block = if accepted {
            result_block(&started.id, report, false)
        } else {
            error_block(
                &started.id,
                &format!(
                    "{} changed while the writer ran; no changes were applied",
                    context.scope
                ),
            )
        };
        let mut record = completed_record(
            started,
            ToolResult::Complete {
                observation: observation_path(started),
                proposal: Some(proposal.clone()),
            },
            None,
        );
        record.files = files.iter().map(|(path, _)| path.clone()).collect();
        record.files.sort();
        record.files_outcome = Some(FilesOutcome {
            applied: record.files.clone(),
            conflicted: if accepted {
                Vec::new()
            } else {
                vec![context.scope.clone()]
            },
        });
        let expected = state.head().clone();
        let transition = Transition::ToolComplete {
            record,
            payloads: vec![
                ("observation.json".into(), canonical_payload_bytes(&block)?),
                (
                    "writer-result.json".into(),
                    canonical_payload_bytes(&json!({
                        "scope":context.scope, "tool_path":context.tool_path,
                        "applied":accepted, "report":report
                    }))?,
                ),
            ],
            files,
        };
        if matches!(
            state.try_append_at(&expected, transition)?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(());
        }
    }
    Err("conversation kept moving while applying writer result".into())
}

#[cfg(test)]
#[path = "writers_tests.rs"]
mod tests;
