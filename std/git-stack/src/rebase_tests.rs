use super::*;
use conversation_protocol::v3::{MemoryStore, Signature};

fn signature() -> Signature {
    Signature {
        name: "Replay".into(),
        email: "test@example.com".into(),
        time: 1,
        offset: "+0000".into(),
    }
}
fn source(store: &mut MemoryStore, text: &str, parents: Vec<Oid>) -> Oid {
    let mut tree = TreeBuilder::from(None);
    tree.put("file", Mode::Blob, text.as_bytes().to_vec());
    let tree = tree.build(store).unwrap();
    store
        .write_commit(&CommitInfo {
            tree,
            parents,
            author: signature(),
            committer: signature(),
            extra_headers: Vec::new(),
            message: text.as_bytes().to_vec(),
        })
        .unwrap()
}
fn fixture() -> (MemoryStore, Oid, Oid, String) {
    let mut store = MemoryStore::new();
    let a = source(&mut store, "base", vec![]);
    let b = source(&mut store, "work", vec![a.clone()]);
    let plan = format!("onto={a}\ncommitter=Replay <test@example.com> 99 +0000\npick={b}\nbranch=feature\nbranch=work\n");
    let mut scope = TreeBuilder::from(None);
    scope.put_oid("00-work", Mode::Commit, b.clone());
    scope.put("00.base", Mode::Blob, a.encode_line());
    scope.put("notes", Mode::Blob, b"ordinary notes\n".to_vec());
    scope.put("2026.md", Mode::Blob, b"dated notes\n".to_vec());
    scope.put("2026-notes.md", Mode::Blob, b"prefixed notes\n".to_vec());
    scope.put(
        "2026-archive/file",
        Mode::Blob,
        b"archived notes\n".to_vec(),
    );
    scope.put(
        "2026.base/file",
        Mode::Blob,
        b"ordinary directory\n".to_vec(),
    );
    scope.put("rebase/plan", Mode::Blob, plan.as_bytes().to_vec());
    let scope = scope.build(&mut store).unwrap();
    (store, scope, b, plan)
}

#[test]
fn completes_from_scoped_tree_preserves_ordinary_files_and_removes_rebase() {
    let (mut store, input, b, plan) = fixture();
    let proposal = propose(&mut store, &input).unwrap();
    let output = Snapshot::new(&store, proposal.tree);
    assert_eq!(output.entry("00-feature").unwrap().unwrap().oid, b);
    assert_eq!(output.entry("01-work").unwrap().unwrap().oid, b);
    assert_eq!(output.read("01.base").unwrap().unwrap(), b.encode_line());
    assert_eq!(output.read("notes").unwrap().unwrap(), b"ordinary notes\n");
    assert_eq!(output.read("2026.md").unwrap().unwrap(), b"dated notes\n");
    assert_eq!(
        output.read("2026-notes.md").unwrap().unwrap(),
        b"prefixed notes\n"
    );
    assert_eq!(
        output.read("2026-archive/file").unwrap().unwrap(),
        b"archived notes\n"
    );
    assert_eq!(
        output.read("2026.base/file").unwrap().unwrap(),
        b"ordinary directory\n"
    );
    assert!(!output.exists("00-work").unwrap());
    assert!(!output.exists("rebase").unwrap());
    assert!(proposal.report.contains(&plan));
    assert!(Snapshot::new(&store, input).exists("rebase/plan").unwrap());
}

#[test]
fn conflict_installs_only_draft_and_report_without_progress_or_output_layers() {
    let (mut store, input, b, plan) = fixture();
    let parent = store.read_commit(&b).unwrap().parents[0].clone();
    let draft = source(
        &mut store,
        "<<<<<<< ours\n=======\n>>>>>>> theirs\n",
        vec![parent.clone()],
    );
    let outcome = rebase_plan::Outcome {
        layers: vec![rebase_plan::Layer {
            name: "00-new".into(),
            tip: parent.clone(),
            base: parent.clone(),
        }],
        conflict: Some(rebase_plan::Conflict {
            line: 4,
            parent: parent.clone(),
            draft: draft.clone(),
            report: b"native report\0stages\n".to_vec(),
        }),
    };
    let proposal = apply(&mut store, &input, &plan, outcome).unwrap();
    let output = Snapshot::new(&store, proposal.tree);
    assert_eq!(output.entry("00-work").unwrap().unwrap().oid, b);
    assert!(!output.exists("00-new").unwrap());
    assert!(!output.exists("rebase/stack").unwrap());
    assert_eq!(
        output.read("rebase/plan").unwrap().unwrap(),
        plan.as_bytes()
    );
    assert_eq!(output.entry("rebase/work").unwrap().unwrap().oid, draft);
    assert_eq!(
        output.read("rebase/conflicts").unwrap().unwrap(),
        b"native report\0stages\n"
    );
    assert!(proposal.report.contains(&format!("pick={parent}..R")));
}

#[test]
fn retries_ignore_obsolete_draft_and_preserve_current_scope_files() {
    let (mut store, input, b, _) = fixture();
    let mut edited = TreeBuilder::from(Some(input));
    edited.put_oid("rebase/work", Mode::Commit, b);
    edited.put("rebase/conflicts", Mode::Blob, b"old report".to_vec());
    edited.put("notes", Mode::Blob, b"current notes".to_vec());
    let edited = edited.build(&mut store).unwrap();
    let first = propose(&mut store, &edited).unwrap();
    let second = propose(&mut store, &edited).unwrap();
    assert_eq!(first.tree, second.tree);
    assert_eq!(first.report, second.report);
    let output = Snapshot::new(&store, first.tree);
    assert_eq!(output.read("notes").unwrap().unwrap(), b"current notes");
    assert!(!output.exists("rebase").unwrap());
}

#[test]
fn messages_are_scoped_regular_files_and_read_as_literal_bytes() {
    let (mut store, input, b, plan) = fixture();
    let mut changed = TreeBuilder::from(Some(input));
    changed.put(
        "rebase/plan",
        Mode::Blob,
        plan.replace("branch=feature", "message=messages/feature\nbranch=feature")
            .into_bytes(),
    );
    changed.put("messages/feature", Mode::Blob, b"  literal\n\n".to_vec());
    let changed = changed.build(&mut store).unwrap();
    let proposal = propose(&mut store, &changed).unwrap();
    let tip = Snapshot::new(&store, proposal.tree)
        .entry("00-feature")
        .unwrap()
        .unwrap()
        .oid;
    let commit = store.read_commit(&tip).unwrap();
    assert_eq!(commit.message, b"  literal\n\n");
    assert_eq!(commit.parents, store.read_commit(&b).unwrap().parents);
    assert_eq!(commit.committer.time, 99);
    assert!(blob(&store, &changed, "messages")
        .unwrap_err()
        .contains("regular file"));
}

#[test]
fn plan_is_required_but_previous_stack_entries_are_not() {
    let (mut store, input, b, _) = fixture();
    let mut changed = TreeBuilder::from(Some(input));
    changed.delete("00-work");
    changed.delete("00.base");
    let changed = changed.build(&mut store).unwrap();
    let proposal = propose(&mut store, &changed).unwrap();
    assert_eq!(
        Snapshot::new(&store, proposal.tree)
            .entry("00-feature")
            .unwrap()
            .unwrap()
            .oid,
        b
    );
    let mut missing = TreeBuilder::from(Some(changed));
    missing.delete("rebase");
    let missing = missing.build(&mut store).unwrap();
    assert!(propose(&mut store, &missing)
        .unwrap_err()
        .contains("missing file rebase/plan"));
}

#[test]
fn completion_rejects_output_names_that_collide_with_ordinary_entries() {
    for path in ["00-feature", "00-feature/file", "01.base/file"] {
        let (mut store, input, _, _) = fixture();
        let mut changed = TreeBuilder::from(Some(input));
        changed.put(path, Mode::Blob, b"keep me".to_vec());
        let changed = changed.build(&mut store).unwrap();
        let error = propose(&mut store, &changed).unwrap_err();
        assert!(
            error.contains("would replace an ordinary stack file"),
            "{error}"
        );
        assert_eq!(
            Snapshot::new(&store, changed).read(path).unwrap().unwrap(),
            b"keep me"
        );
    }
}
