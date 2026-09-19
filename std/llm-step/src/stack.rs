//! Object-only stack updates. Inputs are pinned before computation; source
//! pointers move together only if those inputs still match the conversation.
use super::*;
use crate::object_upload::upload;
use conversation_protocol::v3::stacks::{Layer, Method, Operation};
use conversation_protocol::v3::GitStore;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Boundary {
    name: String,
    base: Oid,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    base: String,
    layers: Vec<Boundary>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Parameters {
    Create {
        path: String,
        base: String,
        layers: Vec<String>,
    },
    Status {
        path: String,
    },
    Rebase {
        path: String,
        onto: String,
        #[serde(default = "rebase")]
        method: Method,
    },
    Continue {
        path: String,
        resolved: bool,
    },
    Abort {
        path: String,
    },
}
fn rebase() -> Method {
    Method::Rebase
}
impl Parameters {
    fn path(&self) -> &str {
        match self {
            Self::Create { path, .. }
            | Self::Status { path }
            | Self::Rebase { path, .. }
            | Self::Continue { path, .. }
            | Self::Abort { path } => path,
        }
    }
}

pub(super) fn declaration() -> Value {
    json!({
        "name":"stack",
        "description":"Manage an ordered stack of source gitlinks without checking out the repository. create registers existing immediate-child gitlinks and pins each layer's predecessor; base and layers are names relative to path. rebase replays each layer onto an imported commit or gitlink; method=merge preserves merge histories instead. Conflicts leave source pointers unchanged and create path/restack/work plus path/restack/operation.json containing every conflict and original blob hash. Edit and test the work gitlink, then continue with resolved=true to explicitly acknowledge ALL conflicts, including structural ones. This can pause again at the next conflict. Completed updates replace all stack pointers together; concurrent edits cause a refusal. abort removes the pending attempt and preserves source pointers. status shows pointers and progress. No command here publishes to GitHub.",
        "input_schema":{"type":"object","additionalProperties":false,"properties":{
            "action":{"type":"string","enum":["create","status","rebase","continue","abort"]},
            "path":{"type":"string","description":"Conversation directory containing the stack, e.g. feature."},
            "base":{"type":"string","description":"create: existing base gitlink name, e.g. 00-base."},
            "layers":{"type":"array","items":{"type":"string"},"description":"create: existing layer gitlink names, bottom to top."},
            "onto":{"type":"string","description":"rebase: full imported commit hash or conversation-relative gitlink path."},
            "method":{"type":"string","enum":["rebase","merge"]},
            "resolved":{"type":"boolean","description":"continue: must be true, asserting every reported conflict has been resolved in restack/work."}
        },"required":["action","path"]}
    })
}

#[derive(Clone, Serialize, Deserialize)]
struct Expected {
    path: String,
    entry: Option<(String, Oid)>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Plan {
    path: String,
    manifest: Manifest,
    operation: Operation,
    expected: Vec<Expected>,
    resolved: Option<Oid>,
}

fn child(path: &str, name: &str) -> Result<String, String> {
    if name.is_empty()
        || name.contains('/')
        || matches!(name, "stack.json" | "restack" | "." | "..")
    {
        return Err(format!("invalid stack gitlink name {name:?}"));
    }
    let path = format!("{path}/{name}");
    paths::validate_source_tree_name(&path)?;
    Ok(path)
}
fn manifest_path(path: &str) -> String {
    format!("{path}/stack.json")
}
fn operation_path(path: &str) -> String {
    format!("{path}/restack/operation.json")
}
fn work_path(path: &str) -> String {
    format!("{path}/restack/work")
}
fn source(view: &Conversation<'_>, path: &str) -> Result<Oid, String> {
    view.source_tree(path)?
        .map(|s| s.commit)
        .ok_or_else(|| format!("{path} is not a source gitlink"))
}
fn read_json<T: serde::de::DeserializeOwned>(
    view: &Conversation<'_>,
    path: &str,
) -> Result<T, String> {
    let bytes = view
        .snapshot()
        .read(path)?
        .ok_or_else(|| format!("{path} does not exist"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid {path}: {e}"))
}
fn expected(
    view: &Conversation<'_>,
    paths: impl IntoIterator<Item = String>,
) -> Result<Vec<Expected>, String> {
    paths
        .into_iter()
        .map(|path| {
            Ok(Expected {
                entry: view
                    .snapshot()
                    .entry(&path)?
                    .map(|e| (e.mode.octal().into(), e.oid)),
                path,
            })
        })
        .collect()
}
fn check(view: &Conversation<'_>, expected: &[Expected]) -> Result<(), String> {
    for item in expected {
        let entry = view
            .snapshot()
            .entry(&item.path)?
            .map(|e| (e.mode.octal().into(), e.oid));
        if entry != item.entry {
            return Err(format!("{} changed while the stack operation was running; sources were not replaced. Inspect the current stack and start a new operation.",item.path));
        }
    }
    Ok(())
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(value).map_err(|e| e.to_string())
}

pub(super) fn execute(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    match execute_inner(state, site) {
        Ok(()) => Ok(()),
        Err(error) => site.fail(state, &error),
    }
}

fn execute_inner(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let p: Parameters = serde_json::from_value(site.call.input.clone())
        .map_err(|e| format!("invalid stack arguments: {e}"))?;
    paths::validate_source_tree_name(p.path())?;
    let server = std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set")?;
    let mut store = GitStore::scratch_partial(&fresh_name("stack-objects"), &server)?;
    let view = state.conversation()?;
    match &p {
        Parameters::Status { path } => {
            let manifest: Manifest = read_json(&view, &manifest_path(path))?;
            let layers=manifest.layers.iter().map(|l|Ok(json!({
                "path":child(path,&l.name)?,"base":l.base,"head":source(&view,&child(path,&l.name)?)?
            }))).collect::<Result<Vec<_>,String>>()?;
            let operation = if view.snapshot().exists(&operation_path(path))? {
                Some(read_json::<Plan>(&view, &operation_path(path))?.operation)
            } else {
                None
            };
            return site.complete(state,result_block(&site.call.id,&json!({"base":source(&view,&child(path,&manifest.base)?)?,"layers":layers,"operation":operation}).to_string(),false),None,None,None);
        }
        Parameters::Create { path, base, layers } => {
            if view.snapshot().exists(&manifest_path(path))?
                || view.snapshot().exists(&format!("{path}/restack"))?
            {
                return Err("stack metadata already exists".into());
            }
            if layers.is_empty() {
                return Err("a stack needs at least one layer".into());
            }
            let base_path = child(path, base)?;
            let mut previous = source(&view, &base_path)?;
            let mut names = HashSet::from([base.clone()]);
            let mut boundaries = Vec::new();
            let mut inputs = vec![manifest_path(path), format!("{path}/restack"), base_path];
            let mut selected = Vec::new();
            for name in layers {
                if !names.insert(name.clone()) {
                    return Err(format!("duplicate stack gitlink {name}"));
                }
                let target = child(path, name)?;
                let head = source(&view, &target)?;
                selected.push(Layer {
                    path: target.clone(),
                    base: previous.clone(),
                    head: head.clone(),
                });
                boundaries.push(Boundary {
                    name: name.clone(),
                    base: previous,
                });
                previous = head;
                inputs.push(target);
            }
            // Establish each boundary now; later lower-layer edits must not
            // change where the upper layer's own commits begin.
            for layer in &selected {
                if !store.is_ancestor(&layer.base, &layer.head)? {
                    return Err(format!(
                        "{} does not descend from its predecessor",
                        layer.path
                    ));
                }
            }
            let guard = expected(&view, inputs)?;
            let manifest = Manifest {
                base: base.clone(),
                layers: boundaries,
            };
            return complete(
                state,
                site,
                &guard,
                vec![(manifest_path(path), Some((Mode::Blob, encode(&manifest)?)))],
                json!({"status":"created","path":path}),
            );
        }
        Parameters::Abort { path } => {
            let _: Plan = read_json(&view, &operation_path(path))?;
            let guard = expected(&view, [format!("{path}/restack")])?;
            return complete(
                state,
                site,
                &guard,
                vec![(format!("{path}/restack"), None)],
                json!({"status":"aborted","path":path}),
            );
        }
        _ => {}
    }
    let saved = pin_path(site);
    let mut plan: Plan = if view
        .tool(site.request, site.round, &site.call.id)?
        .is_some()
    {
        serde_json::from_slice(&view.payload(&saved)?).map_err(|_| "invalid saved stack plan")?
    } else {
        let path = p.path();
        let manifest: Manifest = read_json(&view, &manifest_path(path))?;
        let mut inputs = vec![
            manifest_path(path),
            child(path, &manifest.base)?,
            format!("{path}/restack"),
        ];
        for layer in &manifest.layers {
            inputs.push(child(path, &layer.name)?);
        }
        let (operation, resolved) = match p {
            Parameters::Rebase {
                ref onto, method, ..
            } => {
                if view.snapshot().exists(&format!("{path}/restack"))? {
                    return Err("a stack update is already pending; continue or abort it".into());
                }
                let onto = match Oid::parse(onto, "new stack base") {
                    Ok(oid) => oid,
                    Err(_) => {
                        inputs.push(onto.clone());
                        source(&view, onto)?
                    }
                };
                let layers = manifest
                    .layers
                    .iter()
                    .map(|l| {
                        Ok(Layer {
                            path: child(path, &l.name)?,
                            base: l.base.clone(),
                            head: source(&view, &child(path, &l.name)?)?,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                (Operation::start(&store, layers, onto, method)?, None)
            }
            Parameters::Continue { resolved, .. } => {
                if !resolved {
                    return Err(
                        "continue requires resolved=true after resolving every conflict".into(),
                    );
                }
                let saved: Plan = read_json(&view, &operation_path(path))?;
                let guard: Vec<_> = saved
                    .expected
                    .into_iter()
                    .filter(|e| e.path != format!("{path}/restack"))
                    .collect();
                check(&view, &guard)?;
                let operation = saved.operation;
                let pending = operation
                    .pending
                    .as_ref()
                    .ok_or("stack has no pending conflict")?;
                let resolved = source(&view, &work_path(path))?;
                if !store.is_ancestor(&pending.draft, &resolved)? {
                    return Err("the resolved work must descend from the pending draft".into());
                }
                (operation, Some(resolved))
            }
            _ => unreachable!(),
        };
        let proposed = Plan {
            path: path.into(),
            manifest,
            operation,
            expected: expected(&view, inputs)?,
            resolved,
        };
        match pin(state, site, proposed)? {
            Some(plan) => plan,
            None => return Ok(()),
        }
    };
    if let Some(resolved) = &plan.resolved {
        plan.operation.resume(&mut store, resolved)?;
    } else {
        plan.operation.advance(&mut store)?;
    }
    let mut uploaded = HashSet::new();
    for commit in &plan.operation.completed {
        upload(&store, &server, commit, Mode::Commit, &mut uploaded)?;
    }
    if let Some(pending) = &plan.operation.pending {
        upload(&store, &server, &pending.draft, Mode::Commit, &mut uploaded)?;
        // Stage blobs can include virtual merge-base objects created by Git.
        for stage in &pending.result.stages {
            let mode = Mode::parse(&stage.mode)?;
            if mode != Mode::Commit {
                upload(&store, &server, &stage.oid, mode, &mut uploaded)?;
            }
        }
        complete(
            state,
            site,
            &plan.expected,
            vec![
                (
                    operation_path(&plan.path),
                    Some((Mode::Blob, encode(&plan)?)),
                ),
                (
                    work_path(&plan.path),
                    Some((Mode::Commit, pending.draft.encode_line())),
                ),
            ],
            json!({"status":"conflicts","work":work_path(&plan.path),"operation":operation_path(&plan.path),"conflicts":pending.result}),
        )
    } else {
        let mut files = vec![
            (
                child(&plan.path, &plan.manifest.base)?,
                Some((Mode::Commit, plan.operation.onto.encode_line())),
            ),
            (format!("{}/restack", plan.path), None),
        ];
        let mut lower = plan.operation.onto.clone();
        for (layer, head) in plan
            .manifest
            .layers
            .iter_mut()
            .zip(&plan.operation.completed)
        {
            layer.base = lower;
            files.push((
                child(&plan.path, &layer.name)?,
                Some((Mode::Commit, head.encode_line())),
            ));
            lower = head.clone();
        }
        files.push((
            manifest_path(&plan.path),
            Some((Mode::Blob, encode(&plan.manifest)?)),
        ));
        complete(
            state,
            site,
            &plan.expected,
            files,
            json!({"status":"complete","path":plan.path,"commits":plan.operation.completed}),
        )
    }
}

fn pin_path(site: &CallSite<'_>) -> String {
    format!(
        "{}/stack.json",
        paths::call_payload_dir(site.request.as_str(), site.round, &site.call.id)
    )
}
fn pin(
    state: &mut progress::State,
    site: &CallSite<'_>,
    plan: Plan,
) -> Result<Option<Plan>, String> {
    for _ in 0..32 {
        state.reload()?;
        let view = state.conversation()?;
        if let Some(record) = view.tool(site.request, site.round, &site.call.id)? {
            return if record.is_terminal() {
                Ok(None)
            } else {
                Ok(Some(
                    serde_json::from_slice(&view.payload(&pin_path(site))?)
                        .map_err(|_| "invalid saved stack plan")?,
                ))
            };
        }
        check(&view, &plan.expected)?;
        let mut record = site.stub(None);
        record.status = CallStatus::Started;
        let head = state.head().clone();
        if matches!(
            state.try_append_at(
                &head,
                Transition::ToolStart {
                    record,
                    payloads: vec![("stack.json".into(), encode(&plan)?)]
                }
            )?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(Some(plan));
        }
    }
    Err("conversation kept moving while starting stack update".into())
}
fn complete<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    guard: &[Expected],
    files: FileEdits,
    result: Value,
) -> Result<(), String> {
    for _ in 0..32 {
        state.reload()?;
        let view = state.conversation()?;
        if view
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(());
        }
        check(&view, guard)?;
        // Protocol validation reconstructs leaf changes from the snapshot diff.
        // Omit no-ops and expand directory removals to that same representation.
        let before = view.snapshot().root().clone();
        let mut builder = conversation_protocol::v3::tree::TreeBuilder::from(Some(before.clone()));
        for (path, value) in &files {
            match value {
                Some((Mode::Commit | Mode::Tree, bytes)) => {
                    let oid = Oid::parse(
                        std::str::from_utf8(bytes)
                            .map_err(|_| "invalid object reference")?
                            .trim(),
                        "stack file reference",
                    )?;
                    builder.put_oid(path, value.as_ref().unwrap().0, oid);
                }
                Some((mode, bytes)) => builder.put(path, *mode, bytes.clone()),
                None => builder.delete(path),
            }
        }
        let after = builder.build(state.store_mut())?;
        let files = conversation_protocol::v3::tree::diff(state.store(), Some(&before), &after)?
            .into_iter()
            .map(|change| {
                let value = change
                    .after
                    .map(|(mode, oid)| {
                        let bytes = match mode {
                            Mode::Commit | Mode::Tree => oid.encode_line(),
                            _ => state.store().read_blob(&oid)?,
                        };
                        Ok::<_, String>((mode, bytes))
                    })
                    .transpose()?;
                Ok::<_, String>((change.path, value))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let stub = site.stub(None);
        let mut record = completed_record(
            &stub,
            ToolResult::Complete {
                observation: observation_path(&stub),
                proposal: None,
            },
            None,
        );
        record.files = files.iter().map(|(name, _)| name.clone()).collect();
        record.files.sort();
        record.files_outcome = Some(FilesOutcome {
            applied: record.files.clone(),
            conflicted: Vec::new(),
        });
        let transition = tool_complete_transition(
            record,
            &result_block(&site.call.id, &result.to_string(), false),
            files.clone(),
        )?;
        let head = state.head().clone();
        if matches!(
            state.try_append_at(&head, transition)?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(());
        }
    }
    Err("conversation kept moving while saving stack result".into())
}

/// Source paths in stack order, checked before any publication is pinned.
pub(super) fn push_sources(
    view: &Conversation<'_>,
    path: &str,
    store: &GitStore,
) -> Result<Vec<String>, String> {
    paths::validate_source_tree_name(path)?;
    if view.snapshot().exists(&format!("{path}/restack"))? {
        return Err("finish or abort the pending stack update before pushing".into());
    }
    let manifest: Manifest = read_json(view, &manifest_path(path))?;
    if manifest.layers.is_empty() {
        return Err("a stack needs at least one layer".into());
    }
    let mut lower = source(view, &child(path, &manifest.base)?)?;
    let mut branches = Vec::new();
    for boundary in &manifest.layers {
        let branch = child(path, &boundary.name)?;
        conversation_protocol::v3::source_trees::validate_branch(&branch)?;
        if branch.starts_with("refs/") {
            return Err("branch names must omit refs/heads/".into());
        }
        let commit = source(view, &branch)?;
        if boundary.base != lower || !store.is_ancestor(&lower, &commit)? {
            return Err(format!(
                "{branch} does not contain the current lower layer; restack before pushing"
            ));
        }
        branches.push(branch);
        lower = commit;
    }
    Ok(branches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{golden_with_first, ImportStore};

    #[test]
    fn stack_update_atomically_replaces_multiple_gitlinks_and_survives_lost_ack() {
        let args = json!({"action":"rebase","path":"feature","onto":"imports/main"});
        let golden = golden_with_first("stack", args.clone()).unwrap();
        let request = golden.request.clone();
        let call = Call {
            id: "first".into(),
            name: "stack".into(),
            input: args,
        };
        let view = Conversation::open(&golden.store, &golden.head).unwrap();
        let declaration = view.transcript_entry(1).unwrap().unwrap().1.message_id;
        let site = CallSite::at(&request, 0, &call, &declaration);
        let store = ImportStore {
            objects: golden.store,
            head: golden.head.clone(),
            race: None,
            lost_ack: true,
        };
        let mut state = progress::State::from_store(
            store,
            "refs/conversations/conversation/head".into(),
            golden.head,
        )
        .unwrap();
        let original = Oid::parse(&"a".repeat(40), "original").unwrap();
        let new_a = Oid::parse(&"b".repeat(40), "new a").unwrap();
        let new_b = Oid::parse(&"c".repeat(40), "new b").unwrap();
        state
            .append(Transition::FilesApply {
                files: vec![
                    (
                        "feature/a".into(),
                        Some((Mode::Commit, original.encode_line())),
                    ),
                    (
                        "feature/b".into(),
                        Some((Mode::Commit, original.encode_line())),
                    ),
                    (
                        "feature/base".into(),
                        Some((Mode::Commit, original.encode_line())),
                    ),
                    (
                        "feature/restack/work".into(),
                        Some((Mode::Commit, original.encode_line())),
                    ),
                    (
                        "feature/restack/operation.json".into(),
                        Some((Mode::Blob, b"{}".to_vec())),
                    ),
                ],
            })
            .unwrap();
        let guard = expected(
            &state.conversation().unwrap(),
            ["feature/a".into(), "feature/b".into()],
        )
        .unwrap();
        let files = vec![
            (
                "feature/base".into(),
                Some((Mode::Commit, original.encode_line())),
            ),
            ("feature/restack".into(), None),
            (
                "feature/a".into(),
                Some((Mode::Commit, new_a.encode_line())),
            ),
            (
                "feature/b".into(),
                Some((Mode::Commit, new_b.encode_line())),
            ),
        ];
        complete(
            &mut state,
            &site,
            &guard,
            files.clone(),
            json!({"status":"complete"}),
        )
        .unwrap();
        assert_eq!(
            source(&state.conversation().unwrap(), "feature/a").unwrap(),
            new_a
        );
        assert_eq!(
            source(&state.conversation().unwrap(), "feature/b").unwrap(),
            new_b
        );
        let head = state.head().clone();
        complete(
            &mut state,
            &site,
            &guard,
            files,
            json!({"status":"complete"}),
        )
        .unwrap();
        assert_eq!(state.head(), &head);
        validate_spine(state.store(), state.head(), &mut HashSet::new()).unwrap();
    }

    #[test]
    fn concurrent_edit_refuses_the_whole_stack_update() {
        let args = json!({"action":"rebase","path":"feature","onto":"imports/main"});
        let golden = golden_with_first("stack", args.clone()).unwrap();
        let request = golden.request.clone();
        let call = Call {
            id: "first".into(),
            name: "stack".into(),
            input: args,
        };
        let view = Conversation::open(&golden.store, &golden.head).unwrap();
        let declaration = view.transcript_entry(1).unwrap().unwrap().1.message_id;
        let site = CallSite::at(&request, 0, &call, &declaration);
        let store = ImportStore {
            objects: golden.store,
            head: golden.head.clone(),
            race: None,
            lost_ack: false,
        };
        let mut state = progress::State::from_store(
            store,
            "refs/conversations/conversation/head".into(),
            golden.head,
        )
        .unwrap();
        let original = Oid::parse(&"a".repeat(40), "original").unwrap();
        let changed = Oid::parse(&"b".repeat(40), "changed").unwrap();
        state
            .append(Transition::reference(
                "feature/a".into(),
                Some(original.clone()),
            ))
            .unwrap();
        let guard = expected(&state.conversation().unwrap(), ["feature/a".into()]).unwrap();
        state
            .append(Transition::reference(
                "feature/a".into(),
                Some(changed.clone()),
            ))
            .unwrap();
        assert!(complete(
            &mut state,
            &site,
            &guard,
            vec![(
                "feature/a".into(),
                Some((Mode::Commit, original.encode_line()))
            )],
            json!({})
        )
        .unwrap_err()
        .contains("changed"));
        assert_eq!(
            source(&state.conversation().unwrap(), "feature/a").unwrap(),
            changed
        );
    }
}
