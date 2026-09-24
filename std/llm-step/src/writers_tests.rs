use super::*;
use crate::tests::{append_memory, golden_with_first, ImportStore, ASSISTANT_ID};
use conversation_protocol::v3::{CommitInfo, TreeBuilder};

fn oid(c: char) -> Oid {
    Oid::parse(&c.to_string().repeat(40), "test oid").unwrap()
}

fn file(name: &str, text: &str) -> (String, Option<(Mode, Vec<u8>)>) {
    (name.into(), Some((Mode::Blob, text.as_bytes().to_vec())))
}

fn reference(name: &str, value: Oid) -> (String, Option<(Mode, Vec<u8>)>) {
    (name.into(), Some((Mode::Commit, value.encode_line())))
}

fn call(id: &str, stack: &str) -> Call {
    Call {
        id: id.into(),
        name: "run_tool".into(),
        input: json!({"path":"caos-std/git-rebase-i", "arguments":{
            "stack":stack, "plan":format!("{stack}/rebase/plan")
        }}),
    }
}

fn setup() -> (progress::State<ImportStore>, Oid) {
    let first = call("first", "feature");
    let second = call("second", "other");
    let mut golden = golden_with_first("run_tool", first.input.clone()).unwrap();
    // Reuse the normal conversation fixture, declaring its second call as a
    // repository writer too so both can run against their captured inputs.
    let declared = golden.store.read_commit(&golden.head).unwrap();
    let mut entry = Conversation::open(&golden.store, &golden.head)
        .unwrap()
        .transcript_entry(1)
        .unwrap()
        .unwrap()
        .1;
    for block in &mut entry.blocks {
        if let Block::ToolUse { id, name, .. } = block {
            if id == "second" {
                *name = "run_tool".into();
            }
        }
    }
    golden.head =
        append_memory(
            &mut golden.store,
            &declared.parents[0],
            Transition::ModelComplete {
                request: golden.request.clone(),
                entry,
                payloads: vec![
            ("response.json".into(), canonical_payload_bytes(&json!([
                {"type":"text","text":"I will inspect it."}, first.value(), second.value()
            ])).unwrap()),
            ("args-first.json".into(), canonical_payload_bytes(&first.input).unwrap()),
            ("args-second.json".into(), canonical_payload_bytes(&second.input).unwrap()),
        ],
                calls: vec![
                    DeclaredCall {
                        id: "first".into(),
                        name: "run_tool".into(),
                    },
                    DeclaredCall {
                        id: "second".into(),
                        name: "run_tool".into(),
                    },
                ],
            },
        )
        .unwrap();
    golden.head = append_memory(
        &mut golden.store,
        &golden.head,
        Transition::FilesApply {
            files: vec![
                reference("feature/00-work", oid('b')),
                file("feature/00.base", oid('a').as_str()),
                reference("feature/01-work", oid('c')),
                file("feature/01.base", oid('b').as_str()),
                file("feature/notes", "preserve my notes"),
                file("feature/rebase/plan", "original plan"),
                reference("other/00-work", oid('d')),
                file("other/00.base", oid('a').as_str()),
                file("other/rebase/plan", "other plan"),
                file("notes", "outside notes"),
            ],
        },
    )
    .unwrap();
    let request = golden.request;
    let state = progress::State::from_store(
        ImportStore {
            objects: golden.store,
            head: golden.head.clone(),
            race: None,
            lost_ack: false,
        },
        "refs/conversations/conversation/head".into(),
        golden.head,
    )
    .unwrap();
    (state, request)
}

fn begin(
    state: &mut progress::State<ImportStore>,
    request: &Oid,
    id: &str,
    scope: &str,
) -> (CallRecord, Context) {
    let call = call(id, scope);
    let context = Context {
        head: state.head().clone(),
        scope: scope.into(),
        tool_path: "caos-std/git-rebase-i".into(),
        committer: "Author <a@example.com> 1 +0000".into(),
    };
    let mut record = CallSite::at(request, 0, &call, ASSISTANT_ID).stub(None);
    record.status = CallStatus::Started;
    record.input_commit = Some(context.head.clone());
    state
        .append(Transition::ToolStart {
            record: record.clone(),
            payloads: vec![(
                "writer-input.json".into(),
                canonical_payload_bytes(&json!(context)).unwrap(),
            )],
        })
        .unwrap();
    (record, context)
}

fn proposal(
    state: &mut progress::State<ImportStore>,
    context: &Context,
    changes: impl FnOnce(&mut TreeBuilder),
) -> Oid {
    let original = state.store().read_commit(&context.head).unwrap();
    let mut builder = TreeBuilder::from(Some(original.tree));
    changes(&mut builder);
    let tree = builder.build(state.store_mut()).unwrap();
    state
        .store_mut()
        .write_commit(&CommitInfo {
            tree,
            parents: vec![context.head.clone()],
            author: original.author,
            committer: original.committer,
            extra_headers: Vec::new(),
            message: b"writer proposal\n".to_vec(),
        })
        .unwrap()
}

fn finished_stack(state: &mut progress::State<ImportStore>, context: &Context, tip: Oid) -> Oid {
    let scope = context.scope.clone();
    proposal(state, context, |builder| {
        builder.delete(&format!("{scope}/00-work"));
        builder.delete(&format!("{scope}/01-work"));
        builder.delete(&format!("{scope}/01.base"));
        builder.delete(&format!("{scope}/rebase"));
        builder.put_oid(&format!("{scope}/00-clean"), Mode::Commit, tip);
    })
}

fn result(state: &progress::State<ImportStore>, started: &CallRecord) -> (CallRecord, Value) {
    let view = state.conversation().unwrap();
    let record = view
        .tool(&started.request, started.round, &started.id)
        .unwrap()
        .unwrap();
    let bytes = view
        .payload(&payload_path(&record, "writer-result.json"))
        .unwrap();
    (record, serde_json::from_slice(&bytes).unwrap())
}

fn assert_reference(state: &progress::State<ImportStore>, path: &str, expected: Oid) {
    let entry = state
        .conversation()
        .unwrap()
        .snapshot()
        .entry(path)
        .unwrap()
        .unwrap();
    assert_eq!((entry.mode, entry.oid), (Mode::Commit, expected));
}

#[test]
fn scoped_proposal_installs_exact_gitlinks_and_preserves_other_files() {
    let (mut state, request) = setup();
    let (started, context) = begin(&mut state, &request, "first", "feature");
    let proposed = finished_stack(&mut state, &context, oid('e'));
    apply(&mut state, &started, &context, &proposed, "rewrote feature").unwrap();
    assert_reference(&state, "feature/00-clean", oid('e'));
    assert_reference(&state, "other/00-work", oid('d'));
    let view = state.conversation().unwrap();
    assert_eq!(
        view.file("feature/notes").unwrap().unwrap(),
        b"preserve my notes"
    );
    assert_eq!(view.file("notes").unwrap().unwrap(), b"outside notes");
    assert_eq!(
        view.file("other/rebase/plan").unwrap().unwrap(),
        b"other plan"
    );
    assert!(!view.snapshot().exists("feature/00-work").unwrap());
    assert!(!view.snapshot().exists("feature/01-work").unwrap());
    assert!(!view.snapshot().exists("feature/01.base").unwrap());
    assert!(!view.snapshot().exists("feature/rebase").unwrap());
    let (record, result) = result(&state, &started);
    assert_eq!(result["applied"], true);
    assert_eq!(result["report"], "rewrote feature");
    assert!(
        record.source_tree_resolution.is_none(),
        "rewritten gitlinks must not be reconciled"
    );
    assert!(
        matches!(record.result, Some(ToolResult::Complete { proposal: Some(ref oid), .. }) if oid == &proposed)
    );
    assert!(record.files_outcome.unwrap().conflicted.is_empty());
}

#[test]
fn proposal_outside_scope_rejects_all_changes_including_similar_prefix() {
    for outside in ["other/00-work", "feature-sibling/00-work"] {
        let (mut state, request) = setup();
        let (started, context) = begin(&mut state, &request, "first", "feature");
        let before = state.head().clone();
        let proposed = proposal(&mut state, &context, |builder| {
            builder.put_oid("feature/00-work", Mode::Commit, oid('e'));
            builder.put_oid(outside, Mode::Commit, oid('f'));
        });
        let error = apply(&mut state, &started, &context, &proposed, "invalid").unwrap_err();
        assert!(error.contains("outside its declared scope"));
        assert_eq!(state.head(), &before);
        assert_reference(&state, "feature/00-work", oid('b'));
        assert_reference(&state, "other/00-work", oid('d'));
    }
}

#[test]
fn changed_guarded_stack_rejects_without_reconciling_or_overwriting() {
    for change in [
        reference("feature/00-work", oid('f')),
        file("feature/notes", "concurrent note"),
    ] {
        let (mut state, request) = setup();
        let (started, context) = begin(&mut state, &request, "first", "feature");
        let proposed = finished_stack(&mut state, &context, oid('e'));
        state
            .append(Transition::FilesApply {
                files: vec![change.clone()],
            })
            .unwrap();
        let contents = state.conversation().unwrap().tree().clone();
        apply(&mut state, &started, &context, &proposed, "stale result").unwrap();
        assert_eq!(state.conversation().unwrap().tree(), &contents);
        let (record, result) = result(&state, &started);
        assert_eq!(result["applied"], false);
        assert!(record.files.is_empty());
        assert!(record.source_tree_resolution.is_none());
        assert_eq!(record.files_outcome.unwrap().conflicted, vec!["feature"]);
        assert!(!state
            .conversation()
            .unwrap()
            .snapshot()
            .exists("feature/00-clean")
            .unwrap());
    }
}

#[test]
fn concurrent_unrelated_edits_are_preserved_and_do_not_stale_writer() {
    let (mut state, request) = setup();
    let (started, context) = begin(&mut state, &request, "first", "feature");
    let proposed = finished_stack(&mut state, &context, oid('e'));
    state
        .append(Transition::FilesApply {
            files: vec![
                file("notes", "new notes"),
                reference("other/00-work", oid('f')),
            ],
        })
        .unwrap();
    apply(&mut state, &started, &context, &proposed, "done").unwrap();
    assert_eq!(result(&state, &started).1["applied"], true);
    assert_reference(&state, "feature/00-clean", oid('e'));
    assert_reference(&state, "other/00-work", oid('f'));
    assert_eq!(
        state
            .conversation()
            .unwrap()
            .file("notes")
            .unwrap()
            .unwrap(),
        b"new notes"
    );
}

#[test]
fn independent_writers_apply_their_own_scopes_from_old_inputs() {
    let (mut state, request) = setup();
    let (first, first_context) = begin(&mut state, &request, "first", "feature");
    let (second, second_context) = begin(&mut state, &request, "second", "other");
    let first_proposal = finished_stack(&mut state, &first_context, oid('e'));
    let second_proposal = finished_stack(&mut state, &second_context, oid('f'));
    apply(
        &mut state,
        &first,
        &first_context,
        &first_proposal,
        "first done",
    )
    .unwrap();
    apply(
        &mut state,
        &second,
        &second_context,
        &second_proposal,
        "second done",
    )
    .unwrap();
    assert_reference(&state, "feature/00-clean", oid('e'));
    assert_reference(&state, "other/00-clean", oid('f'));
    assert_eq!(result(&state, &first).1["applied"], true);
    assert_eq!(result(&state, &second).1["applied"], true);
}

#[test]
fn abort_and_identical_recreation_cannot_revive_old_writer() {
    let (mut state, request) = setup();
    let (started, context) = begin(&mut state, &request, "first", "feature");
    let proposed = finished_stack(&mut state, &context, oid('e'));
    let original = state
        .conversation()
        .unwrap()
        .snapshot()
        .entry("feature")
        .unwrap()
        .unwrap()
        .oid;
    state
        .append(Transition::FilesApply {
            files: vec![("feature/rebase".into(), None)],
        })
        .unwrap();
    state
        .append(Transition::FilesApply {
            files: vec![file("feature/rebase/plan", "original plan")],
        })
        .unwrap();
    assert_eq!(
        state
            .conversation()
            .unwrap()
            .snapshot()
            .entry("feature")
            .unwrap()
            .unwrap()
            .oid,
        original
    );
    apply(&mut state, &started, &context, &proposed, "old result").unwrap();
    assert_eq!(result(&state, &started).1["applied"], false);
    assert_reference(&state, "feature/00-work", oid('b'));
    assert!(state
        .conversation()
        .unwrap()
        .snapshot()
        .exists("feature/rebase/plan")
        .unwrap());
    assert!(!state
        .conversation()
        .unwrap()
        .snapshot()
        .exists("feature/00-clean")
        .unwrap());
}

#[test]
fn cas_retry_rechecks_scope_and_preserves_unrelated_races() {
    for guarded in [false, true] {
        let (mut state, request) = setup();
        let (started, context) = begin(&mut state, &request, "first", "feature");
        let proposed = finished_stack(&mut state, &context, oid('e'));
        let path = if guarded { "feature/notes" } else { "notes" };
        state.store_mut().race = Some(Transition::FilesApply {
            files: vec![file(path, "racing change")],
        });
        apply(&mut state, &started, &context, &proposed, "done").unwrap();
        assert_eq!(result(&state, &started).1["applied"], !guarded);
        assert_eq!(
            state.conversation().unwrap().file(path).unwrap().unwrap(),
            b"racing change"
        );
        if guarded {
            assert_reference(&state, "feature/00-work", oid('b'));
            assert!(!state
                .conversation()
                .unwrap()
                .snapshot()
                .exists("feature/00-clean")
                .unwrap());
        } else {
            assert_reference(&state, "feature/00-clean", oid('e'));
        }
    }
}

#[test]
fn lost_ack_and_repeated_callback_do_not_apply_twice() {
    let (mut state, request) = setup();
    let (started, context) = begin(&mut state, &request, "first", "feature");
    let proposed = finished_stack(&mut state, &context, oid('e'));
    state.store_mut().lost_ack = true;
    apply(&mut state, &started, &context, &proposed, "done once").unwrap();
    let completed = state.head().clone();
    apply(&mut state, &started, &context, &proposed, "done once").unwrap();
    assert_eq!(state.head(), &completed);
    assert_reference(&state, "feature/00-clean", oid('e'));
    assert_eq!(result(&state, &started).1["applied"], true);
}

#[test]
fn writer_input_is_optional_and_recovers_the_pinned_context() {
    let (mut state, request) = setup();
    let plain = CallSite::at(&request, 0, &call("first", "feature"), ASSISTANT_ID).stub(None);
    assert!(input(&state, &plain).unwrap().is_none());
    let (started, context) = begin(&mut state, &request, "first", "feature");
    let recovered = input(&state, &started).unwrap().unwrap();
    assert_eq!(recovered.head, context.head);
    assert_eq!(recovered.scope, "feature");
    assert_eq!(recovered.tool_path, "caos-std/git-rebase-i");
    assert_eq!(recovered.committer, context.committer);
}

#[test]
fn evaluated_writer_uses_conversation_input_not_its_lookup_source() {
    let (mut state, request) = setup();
    let source = oid('e');
    state
        .append(Transition::FilesApply {
            files: vec![reference("library/revision", source.clone())],
        })
        .unwrap();
    let call = Call {
        id: "first".into(),
        name: "run_tool".into(),
        input: json!({
            "path": "library/revision/generated/tool",
            "arguments": {"stack": "feature"}
        }),
    };
    // Selection must work without looking up the generated definition. The
    // client handoff uses this source commit and the relative generated/tool.
    assert!(matches!(
        resolve_target(&state.conversation().unwrap(), &call).unwrap(),
        Target::SourceTree { name, commit } if name == "library/revision" && commit == source
    ));
    let lookup_head = state.head().clone();
    state
        .append(Transition::FilesApply {
            files: vec![file("notes", "changed while evaluating the tool")],
        })
        .unwrap();
    let tool = tools::builtin_tool(
        "library/revision/generated/tool",
        "Generated writer\n@writer stack\n@param stack Conversation stack",
    );
    let bound = tools::tree_tool_args(
        &json!({"id": call.id, "input": call.input["arguments"]}),
        &tool,
    )
    .unwrap();
    let context = prepare(&state, &lookup_head, &tool, &bound)
        .unwrap()
        .unwrap();
    assert_eq!(context.head, *state.head());
    assert_ne!(context.head, lookup_head);
    assert_ne!(context.head, source);
    assert_eq!(context.scope, "feature");
    assert_eq!(context.tool_path, "library/revision/generated/tool");
    let tree = state.store().read_commit(&context.head).unwrap().tree;
    let input = Snapshot::new(state.store(), tree);
    assert!(input.exists("feature/00-work").unwrap());
    assert_eq!(
        input.read("notes").unwrap().unwrap(),
        b"changed while evaluating the tool"
    );
    assert_eq!(
        input.entry("library/revision").unwrap().unwrap().oid,
        source
    );
    // The protocol requires a conversation writer's ToolStart input to be
    // exactly the current head, even though the scope guard began at lookup.
    let mut record = CallSite::at(&request, 0, &call, ASSISTANT_ID).stub(None);
    record.status = CallStatus::Started;
    record.input_commit = Some(context.head.clone());
    state
        .append(Transition::ToolStart {
            record,
            payloads: vec![(
                "writer-input.json".into(),
                canonical_payload_bytes(&json!(context)).unwrap(),
            )],
        })
        .unwrap();
}

#[test]
fn resolved_writer_validates_scope_and_leaves_ordinary_tools_unchanged() {
    let (state, _) = setup();
    let ordinary = tools::builtin_tool("library/tool", "Ordinary tool\n@param stack A value");
    assert!(prepare(
        &state,
        state.head(),
        &ordinary,
        &[("stack".into(), "feature".into())]
    )
    .unwrap()
    .is_none());
    let writer = tools::builtin_tool(
        "library/tool",
        "Writer\n@writer stack\n@param stack Conversation stack",
    );
    assert!(prepare(&state, state.head(), &writer, &[]).is_err());
    for invalid in ["../feature", ".caos", "/feature", "feature/../other"] {
        assert!(prepare(
            &state,
            state.head(),
            &writer,
            &[("stack".into(), invalid.into())]
        )
        .is_err());
    }
}

#[test]
fn abort_and_recreation_during_tool_lookup_rejects_the_writer() {
    let (mut state, _) = setup();
    let lookup_head = state.head().clone();
    let plan = state
        .conversation()
        .unwrap()
        .file("feature/rebase/plan")
        .unwrap()
        .unwrap();
    state
        .append(Transition::FilesApply {
            files: vec![("feature/rebase".into(), None)],
        })
        .unwrap();
    state
        .append(Transition::FilesApply {
            files: vec![("feature/rebase/plan".into(), Some((Mode::Blob, plan)))],
        })
        .unwrap();
    let tool = tools::builtin_tool(
        "library/revision/generated/tool",
        "Generated writer\n@writer stack\n@param stack Conversation stack",
    );
    let result = prepare(
        &state,
        &lookup_head,
        &tool,
        &[("stack".into(), "feature".into())],
    );
    assert!(matches!(result, Err(error) if error.contains("changed while the tool was evaluated")));
}
