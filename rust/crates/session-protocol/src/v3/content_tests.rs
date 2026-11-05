use super::apply::{apply, client_signature, mint, Transition};
use super::events::Event;
use super::view::Session;
use super::*;
use std::collections::{BTreeMap, HashSet};

#[test]
fn execution_roundtrip_leaves_no_lifecycle_files() {
    let mut store = MemoryStore::new();
    let fork = fixtures::golden(&mut store);
    let source = store.read_commit(&fork).unwrap().parents[0].clone();
    validate_spine(&store, &fork, &mut HashSet::new()).unwrap();
    let view = Session::open(&store, &source).unwrap();
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
    // Calls are keyed by id alone: the read, the bash and the spawn.
    let calls = view.tools().unwrap();
    assert_eq!(calls.len(), 3);
    for call in &calls {
        assert_eq!(view.tool(&call.id).unwrap().as_ref(), Some(call));
        if let Some(ToolResult::Complete { observation, .. }) = &call.result {
            assert!(!view.payload(observation).unwrap().is_empty());
        }
    }
    // The async call was declared and never recorded: unfinished, and nothing
    // more than that.
    assert!(view.tool("async-call").unwrap().is_none());
    let last = view.transcript_len().unwrap() - 1;
    assert!(view
        .transcript_entry(last)
        .unwrap()
        .unwrap()
        .1
        .is_interrupt());
    let fork_view = Session::open(&store, &fork).unwrap();
    assert_eq!(
        fork_view.reference_start("main").unwrap(),
        fork_view.source_tree("main").unwrap().unwrap().commit
    );
    assert_ne!(
        view.reference_start("main").unwrap(),
        view.source_tree("main").unwrap().unwrap().commit
    );
    assert!(fork_view.publications_by_creation().unwrap().is_empty());
    let ordered = view.publications_by_creation().unwrap();
    assert_eq!(ordered.len(), view.publications().unwrap().len());
    for publication in ordered {
        assert_eq!(
            view.publication(&publication.id).unwrap(),
            Some(publication)
        );
    }

    assert_eq!(fork_view.tools().unwrap(), view.tools().unwrap());
    for call in fork_view.tools().unwrap() {
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
        if kind == Kind::ToolStart {
            assert_eq!(info.tree, store.read_commit(&info.parents[0]).unwrap().tree);
            validate_commit(&store, &cursor).unwrap();
            let Some(Event::Tool(record)) = events.last_mut() else {
                panic!("tool event")
            };
            record.status = CallStatus::Cancelled;
            record.result = Some(ToolResult::Cancelled {
                reason: "tampered".to_string(),
            });
            record.files_outcome = None;
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
    let root = Transition::SessionRoot {
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
    let view = Session::open(&store, &next).unwrap();
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
    assert!(Session::open(&store, &head)
        .unwrap()
        .source_tree("feature/01-draft")
        .unwrap()
        .is_some());
}

/// A plain commit standing for a client repo's: its files, no session kind.
fn content_commit(store: &mut MemoryStore, parents: &[Oid], files: &[(&str, &str)]) -> (Oid, Oid) {
    let mut builder = crate::v3::tree::TreeBuilder::from(None);
    for (name, text) in files {
        builder.put(name, Mode::Blob, text.as_bytes().to_vec());
    }
    let tree = builder.build(store).unwrap();
    let signature = client_signature("Repo", "repo@example.com", 1);
    let commit = store
        .write_commit(&crate::v3::tree::CommitInfo {
            tree: tree.clone(),
            parents: parents.to_vec(),
            author: signature.clone(),
            committer: signature,
            extra_headers: Vec::new(),
            message: b"client repo\n".to_vec(),
        })
        .unwrap();
    (commit, tree)
}

#[test]
fn a_root_parents_its_content_commit_and_a_fork_merges_a_newer_one_in() {
    use crate::v3::apply::mint_with_parents;

    let mut store = MemoryStore::new();
    let sig = client_signature("Test", "test@example.com", 1);
    // A directory among the files: the fork replaces one the source already has.
    let (old, old_tree) = content_commit(
        &mut store,
        &[],
        &[
            (".caos-expr", "old"),
            ("AGENTS.md", "v1"),
            ("tools/run/.caos-expr", "old tool"),
        ],
    );
    let (new, new_tree) = content_commit(
        &mut store,
        std::slice::from_ref(&old),
        &[
            (".caos-expr", "new"),
            ("AGENTS.md", "v1"),
            ("tools/run/.caos-expr", "new tool"),
            ("tools/test/.caos-expr", "added tool"),
        ],
    );

    // The root: its parent is the commit its content came from.
    let root = Transition::SessionRoot {
        identity: Identity {
            id: "pinned".into(),
            kind: IdentityKind::Root,
            owner: None,
        },
        title: "Pinned".into(),
        content: Some(old_tree.clone()),
    };
    let applied = apply(&mut store, None, &root).unwrap();
    let head = mint(&mut store, &old, &applied, root.kind(), &sig).unwrap();
    validate_spine(&store, &head, &mut HashSet::new()).unwrap();
    assert_eq!(
        Session::open(&store, &head)
            .unwrap()
            .snapshot()
            .read(".caos-expr")
            .unwrap()
            .unwrap(),
        b"old"
    );

    // A root whose content is not its parent's tree is refused, and so is one
    // parenting a session commit.
    let other = mint(&mut store, &new, &applied, root.kind(), &sig).unwrap();
    assert!(validate_commit(&store, &other)
        .unwrap_err()
        .reason
        .contains("differs from its parent"));
    assert!(mint(&mut store, &head, &applied, root.kind(), &sig)
        .unwrap_err()
        .contains("not a session commit"));

    // The fork merges the newer commit in: content replaced, .caos/ kept, the
    // merged commit as second parent.
    let fork = Transition::SessionFork {
        identity: Identity {
            id: "pinned-again".into(),
            kind: IdentityKind::Fork {
                source: head.clone(),
            },
            owner: None,
        },
        title: "Pinned again".into(),
        content: Some(new_tree.clone()),
    };
    let applied = apply(&mut store, Some(&head), &fork).unwrap();
    let forked = mint_with_parents(
        &mut store,
        &head,
        std::slice::from_ref(&new),
        &applied,
        fork.kind(),
        &sig,
    )
    .unwrap();
    validate_spine(&store, &forked, &mut HashSet::new()).unwrap();
    let view = Session::open(&store, &forked).unwrap();
    assert_eq!(view.snapshot().read(".caos-expr").unwrap().unwrap(), b"new");
    assert_eq!(
        view.snapshot()
            .read("tools/run/.caos-expr")
            .unwrap()
            .unwrap(),
        b"new tool"
    );
    assert!(view.snapshot().exists("tools/test/.caos-expr").unwrap());
    assert_eq!(view.title().unwrap(), "Pinned again");
    assert!(
        matches!(view.identity().unwrap().kind, IdentityKind::Fork { source } if source == head)
    );
    assert_eq!(
        store.read_commit(&forked).unwrap().parents,
        vec![head.clone(), new.clone()]
    );

    // Content without the commit it came from, or a second parent on anything
    // but a fork, is refused.
    let bare = mint(&mut store, &head, &applied, fork.kind(), &sig).unwrap();
    assert!(validate_commit(&store, &bare).is_err());
    let plain = Transition::SessionFork {
        identity: Identity {
            id: "plain".into(),
            kind: IdentityKind::Fork {
                source: head.clone(),
            },
            owner: None,
        },
        title: "Plain".into(),
        content: None,
    };
    let plain_applied = apply(&mut store, Some(&head), &plain).unwrap();
    let plain_fork = mint(&mut store, &head, &plain_applied, plain.kind(), &sig).unwrap();
    validate_spine(&store, &plain_fork, &mut HashSet::new()).unwrap();
    assert!(mint_with_parents(
        &mut store,
        &head,
        std::slice::from_ref(&head),
        &applied,
        fork.kind(),
        &sig
    )
    .unwrap_err()
    .contains("not a session commit"));
    let title = Transition::TitleSet {
        title: "Renamed".into(),
    };
    let title_applied = apply(&mut store, Some(&forked), &title).unwrap();
    assert!(mint_with_parents(
        &mut store,
        &forked,
        &[new],
        &title_applied,
        title.kind(),
        &sig
    )
    .unwrap_err()
    .contains("only a session fork"));
}

/// A session recorded before the rename -- format v6, a `conversation.root`
/// commit, transcript entries keyed `conversation` -- still validates, is
/// appended to as v6, and forks into a v7 session that carries its history.
#[test]
fn a_v6_session_appends_as_v6_and_forks_into_v7() {
    let mut store = MemoryStore::new();
    let genesis = super::oid::ensure_genesis(&mut store).unwrap();
    let signature = client_signature("Old", "old@example.com", 1_700_000_000);
    let root = Transition::SessionRoot {
        identity: Identity {
            id: "old-talk".to_string(),
            kind: IdentityKind::Root,
            owner: None,
        },
        title: "Old talk".to_string(),
        content: None,
    };
    let applied = apply(&mut store, None, &root).unwrap();
    // What a pre-rename build wrote: the v6 marker, under `conversation.root`.
    let mut v6 = tree::TreeBuilder::from(Some(applied.tree.clone()));
    v6.put(
        paths::FORMAT,
        Mode::Blob,
        paths::V6_FORMAT_BYTES.as_bytes().to_vec(),
    );
    let v6 = v6.build(&mut store).unwrap();
    let message = events::encode(Kind::SessionRoot, &[]);
    let message = [
        b"conversation.root".as_slice(),
        &message[b"session.root".len()..],
    ]
    .concat();
    let root_commit = store
        .write_commit(&tree::CommitInfo {
            tree: v6,
            parents: vec![genesis],
            author: signature.clone(),
            committer: signature.clone(),
            extra_headers: Vec::new(),
            message,
        })
        .unwrap();

    let append = Transition::MessageAppend {
        entry: TranscriptEntry {
            message_id: "user-0".to_string(),
            session: "old-talk".to_string(),
            role: Role::User,
            actor: "user".to_string(),
            model: None,
            blocks: vec![Block::Text {
                text: "hello".to_string(),
            }],
            proposal: None,
            source_tree_resolution: None,
        },
        payloads: Vec::new(),
    };
    let applied = apply(&mut store, Some(&root_commit), &append).unwrap();
    let head = mint(
        &mut store,
        &root_commit,
        &applied,
        append.kind(),
        &signature,
    )
    .unwrap();
    validate_spine(&store, &head, &mut HashSet::new()).unwrap();
    let view = Session::open(&store, &head).unwrap();
    assert_eq!(view.format().unwrap(), paths::Format::V6);
    let entry_path = paths::transcript_entry_path(0, "user-0");
    let stored = String::from_utf8(view.snapshot().read(&entry_path).unwrap().unwrap()).unwrap();
    assert!(stored.contains("\"conversation\":\"old-talk\""), "{stored}");
    assert_eq!(
        view.transcript_entry(0).unwrap().unwrap().1.session,
        "old-talk"
    );

    let fork = Transition::SessionFork {
        identity: Identity {
            id: "new-talk".to_string(),
            kind: IdentityKind::Fork {
                source: head.clone(),
            },
            owner: None,
        },
        title: "Old talk".to_string(),
        content: None,
    };
    let applied = apply(&mut store, Some(&head), &fork).unwrap();
    let forked = mint(&mut store, &head, &applied, fork.kind(), &signature).unwrap();
    validate_spine(&store, &forked, &mut HashSet::new()).unwrap();
    let view = Session::open(&store, &forked).unwrap();
    assert_eq!(view.format().unwrap(), paths::Format::V7);
    let stored = String::from_utf8(view.snapshot().read(&entry_path).unwrap().unwrap()).unwrap();
    assert!(stored.contains("\"session\":\"old-talk\""), "{stored}");
    assert!(!stored.contains("conversation"), "{stored}");
    assert!(store
        .read_commit(&forked)
        .unwrap()
        .message
        .starts_with(b"session.fork\n"));
}
