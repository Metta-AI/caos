use super::apply::{apply, client_signature, mint, Transition};
use super::events::Event;
use super::view::Conversation;
use super::*;
use std::collections::{BTreeMap, HashSet};

#[test]
fn execution_roundtrip_leaves_no_lifecycle_files() {
    let mut store = MemoryStore::new();
    let fork = fixtures::golden(&mut store);
    let source = store.read_commit(&fork).unwrap().parents[0].clone();
    validate_spine(&store, &fork, &mut HashSet::new()).unwrap();
    let view = Conversation::open(&store, &source).unwrap();
    assert!(view.active_turn().unwrap().is_none());
    assert!(!view.turn_ids().unwrap().is_empty());
    for path in [
        ".caos/source_trees",
        ".caos/requests",
        ".caos/tools",
        ".caos/async",
        ".caos/subagents",
        ".caos/publications",
    ] {
        assert!(!view.snapshot().exists(path).unwrap(), "{path}");
    }
    assert_eq!(
        view.snapshot().entry("main").unwrap().unwrap().mode,
        Mode::Commit
    );
    let calls = view
        .tools(&Oid::parse(&"1".repeat(40), "request").unwrap(), 0)
        .unwrap();
    assert_eq!(calls.len(), 2);
    for call in calls {
        if let Some(ToolResult::Complete { observation, .. }) = call.result {
            assert!(!view.payload(&observation).unwrap().is_empty());
        }
    }
    let fork_view = Conversation::open(&store, &fork).unwrap();
    assert!(fork_view.turn_ids().unwrap().is_empty());
    assert_eq!(
        fork_view.reference_start("main").unwrap(),
        fork_view.source_tree("main").unwrap().unwrap().commit
    );
    assert_ne!(
        view.reference_start("main").unwrap(),
        view.source_tree("main").unwrap().unwrap().commit
    );
    assert!(fork_view.active_turn().unwrap().is_none());
    assert!(fork_view.latest_turn().unwrap().is_none());
    assert!(view.latest_turn().unwrap().is_some());
    assert!(fork_view.publications_by_creation().unwrap().is_empty());
    let ordered = view.publications_by_creation().unwrap();
    assert_eq!(ordered.len(), view.publications().unwrap().len());
    for publication in ordered {
        assert_eq!(
            view.publication(&publication.id).unwrap(),
            Some(publication)
        );
    }

    let request = Oid::parse(&"1".repeat(40), "request").unwrap();
    assert_eq!(
        fork_view.tools(&request, 0).unwrap(),
        view.tools(&request, 0).unwrap()
    );
    for call in fork_view.tools(&request, 0).unwrap() {
        if let Some(ToolResult::Complete { observation, .. }) = call.result {
            assert_eq!(
                fork_view.payload(&observation).unwrap(),
                view.payload(&observation).unwrap()
            );
        }
    }
    assert!(
        matches!(fork_view.identity().unwrap().kind, IdentityKind::Fork { source: origin } if origin == source)
    );
    let identity =
        String::from_utf8(fork_view.snapshot().read(paths::IDENTITY).unwrap().unwrap()).unwrap();
    assert!(!identity.contains("source"));
}

#[test]
fn event_only_commits_validate_and_tampered_events_do_not() {
    let mut store = MemoryStore::new();
    let mut cursor = fixtures::golden(&mut store);
    loop {
        let mut info = store.read_commit(&cursor).unwrap();
        let (kind, mut events) = events::decode(&info.message).unwrap();
        if kind == Kind::TurnClaim {
            assert_eq!(info.tree, store.read_commit(&info.parents[0]).unwrap().tree);
            validate_commit(&store, &cursor).unwrap();
            let Event::Request(request) = &mut events[0] else {
                panic!("request event")
            };
            request.round += 1;
            info.message = events::encode(kind, &events);
            let corrupted = store.write_commit(&info).unwrap();
            assert!(validate_commit(&store, &corrupted).is_err());
            break;
        }
        cursor = info.parents[0].clone();
    }
}

#[test]
fn code_paths_are_content_and_renaming_preserves_code_history() {
    let mut store = MemoryStore::new();
    let g3 = oid::ensure_genesis(&mut store).unwrap();
    let sig = client_signature("Test", "test@example.com", 1);
    let w = Oid::parse(&"a".repeat(40), "code").unwrap();
    let root = Transition::ConversationRoot {
        identity: Identity {
            id: "content-test".into(),
            kind: IdentityKind::Root,
            owner: None,
        },
        title: "Content".into(),
        content: Some({
            let mut content = crate::v3::tree::TreeBuilder::from(None);
            for (name, commit) in BTreeMap::<String, Oid>::from([
                ("feature/00-base".into(), w.clone()),
                ("feature/01-draft".into(), w.clone()),
            ]) {
                content.put_oid(&name, crate::v3::Mode::Commit, commit);
            }
            content.build(&mut store).unwrap()
        }),
    };
    let applied = apply(&mut store, None, &root).unwrap();
    let head = mint(&mut store, &g3, &applied, root.kind(), &sig).unwrap();
    let edits = Transition::FilesApply {
        files: vec![
            ("feature/01-draft".into(), None),
            (
                "feature/01-change".into(),
                Some((Mode::Commit, w.encode_line())),
            ),
            (
                "feature/notes".into(),
                Some((Mode::Blob, b"review notes\n".to_vec())),
            ),
        ],
    };
    let applied = apply(&mut store, Some(&head), &edits).unwrap();
    let next = mint(&mut store, &head, &applied, edits.kind(), &sig).unwrap();
    validate_spine(&store, &next, &mut HashSet::new()).unwrap();
    let view = Conversation::open(&store, &next).unwrap();
    assert_eq!(
        view.source_tree_names().unwrap(),
        ["feature/00-base", "feature/01-change"]
    );
    assert_eq!(
        view.source_tree("feature/01-change")
            .unwrap()
            .unwrap()
            .commit,
        w
    );
    assert_eq!(
        view.previous_reference("feature/01-change")
            .unwrap()
            .unwrap()
            .0,
        "feature/00-base"
    );
    assert!(Conversation::open(&store, &head)
        .unwrap()
        .source_tree("feature/01-draft")
        .unwrap()
        .is_some());
}

#[test]
fn tool_conflict_survives_a_removed_or_replaced_source() {
    let mut store = MemoryStore::new();
    let mut head = fixtures::golden(&mut store);
    let mut record = loop {
        let info = store.read_commit(&head).unwrap();
        let (kind, events) = events::decode(&info.message).unwrap();
        head = info.parents[0].clone();
        if let Some(Event::Tool(record)) = events.into_iter().find(|event| {
            kind == Kind::ToolComplete
                && matches!(event, Event::Tool(record) if record.id == "bash-call")
        }) {
            break record;
        }
    };
    let signature = client_signature("Test", "test@example.com", 1);
    record.status = CallStatus::Conflict;
    record.files.clear();
    record.files_outcome = None;
    record.source_tree_resolution = Some(SourceTreeResolution::Conflict {
        current: None,
        candidate: Oid::parse(&"b".repeat(40), "proposal").unwrap(),
        merge: None,
    });
    for replacement in [None, Some((Mode::Blob, b"replaced".to_vec()))] {
        let remove = Transition::FilesApply {
            files: vec![("main".into(), replacement)],
        };
        let applied = apply(&mut store, Some(&head), &remove).unwrap();
        let removed = mint(&mut store, &head, &applied, remove.kind(), &signature).unwrap();
        let complete = Transition::ToolComplete {
            record: record.clone(),
            payloads: vec![(
                "observation".into(),
                b"Source removed; proposal retained".to_vec(),
            )],
            files: Vec::new(),
        };
        let applied = apply(&mut store, Some(&removed), &complete).unwrap();
        let completed = mint(&mut store, &removed, &applied, complete.kind(), &signature).unwrap();
        validate_spine(&store, &completed, &mut HashSet::new()).unwrap();
        let view = Conversation::open(&store, &completed).unwrap();
        assert_eq!(
            view.tool(&record.request, record.round, &record.id)
                .unwrap(),
            Some(record.clone())
        );
        assert!(view.active_turn().unwrap().is_some());
        assert_eq!(
            view.snapshot().entry("main").unwrap(),
            Conversation::open(&store, &removed)
                .unwrap()
                .snapshot()
                .entry("main")
                .unwrap()
        );
    }
}
