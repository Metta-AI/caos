use super::*;
use crate::v3::{CommitInfo, GitStore, Mode, Signature, Snapshot, TreeBuilder};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Repo {
    dir: std::path::PathBuf,
    store: GitStore,
}
impl Repo {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "caos-stack-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(std::process::Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&dir)
            .status()
            .unwrap()
            .success());
        let store = GitStore::open(&dir, None).unwrap();
        Self { dir, store }
    }
    fn commit(
        &mut self,
        parent: Option<&Oid>,
        files: &[(&str, Option<&str>)],
        message: &str,
    ) -> Oid {
        let tree = parent.map(|p| self.store.read_commit(p).unwrap().tree);
        let mut builder = TreeBuilder::from(tree);
        for (name, content) in files {
            if let Some(content) = content {
                builder.put(name, Mode::Blob, content.as_bytes().to_vec());
            } else {
                builder.delete(name);
            }
        }
        let tree = builder.build(&mut self.store).unwrap();
        let sig = Signature {
            name: "Test".into(),
            email: "test@example.com".into(),
            time: 1234,
            offset: "+0000".into(),
        };
        self.store
            .write_commit(&CommitInfo {
                tree,
                parents: parent.cloned().into_iter().collect(),
                author: sig.clone(),
                committer: sig,
                extra_headers: Vec::new(),
                message: message.as_bytes().to_vec(),
            })
            .unwrap()
    }
    fn text(&self, commit: &Oid, path: &str) -> String {
        let tree = self.store.read_commit(commit).unwrap().tree;
        let snap = Snapshot::new(&self.store, tree);
        String::from_utf8(snap.read(path).unwrap().unwrap()).unwrap()
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn layer(path: &str, base: &Oid, head: &Oid) -> Layer {
    Layer {
        path: path.into(),
        base: base.clone(),
        head: head.clone(),
    }
}

#[test]
fn rebase_replays_each_layer_and_preserves_commit_messages_without_checkout() {
    let mut r = Repo::new();
    let base = r.commit(None, &[("base", Some("base"))], "base");
    let a = r.commit(Some(&base), &[("a", Some("a"))], "first");
    let b = r.commit(Some(&a), &[("b", Some("b"))], "second");
    let main = r.commit(Some(&base), &[("upstream", Some("new"))], "main moved");
    let mut op = Operation::start(
        &r.store,
        vec![layer("first", &base, &a), layer("second", &a, &b)],
        main.clone(),
        Method::Rebase,
    )
    .unwrap();
    op.advance(&mut r.store).unwrap();
    assert!(op.is_complete());
    assert_eq!(
        r.store.read_commit(&op.completed[0]).unwrap().parents,
        vec![main]
    );
    assert_eq!(
        r.store.read_commit(&op.completed[1]).unwrap().parents,
        vec![op.completed[0].clone()]
    );
    assert_eq!(
        r.store.read_commit(&op.completed[1]).unwrap().message,
        b"second"
    );
    assert_eq!(r.text(&op.completed[1], "upstream"), "new");
    assert_eq!(r.text(&op.completed[1], "a"), "a");
    assert_eq!(r.text(&op.completed[1], "b"), "b");
    assert!(!r.dir.join("index").exists());
    assert!(!r.dir.join("a").exists());
}

#[test]
fn conflict_survives_serialization_and_resolution_drafts_stay_out_of_history() {
    let mut r = Repo::new();
    let base = r.commit(None, &[("file", Some("base\n"))], "base");
    let a = r.commit(Some(&base), &[("file", Some("ours\n"))], "first");
    let b = r.commit(Some(&a), &[("extra", Some("second"))], "second");
    let main = r.commit(Some(&base), &[("file", Some("theirs\n"))], "main");
    let mut op = Operation::start(
        &r.store,
        vec![layer("first", &base, &a), layer("second", &a, &b)],
        main.clone(),
        Method::Rebase,
    )
    .unwrap();
    op.advance(&mut r.store).unwrap();
    let pending = op.pending.as_ref().unwrap();
    assert_eq!(pending.result.stages.len(), 3);
    assert!(!pending.result.messages.is_empty());
    let draft = pending.draft.clone();
    assert!(r.text(&draft, "file").contains("<<<<<<<"));
    let tree = r.store.read_commit(&draft).unwrap().tree;
    assert!(Snapshot::new(&r.store, tree)
        .entry(".caos")
        .unwrap()
        .is_none());
    let bytes = serde_json::to_vec(&op).unwrap();
    let mut op: Operation = serde_json::from_slice(&bytes).unwrap();
    let resolved = r.commit(Some(&draft), &[("file", Some("both\n"))], "agent resolves");
    op.resume(&mut r.store, &resolved).unwrap();
    assert!(op.is_complete());
    assert_eq!(
        r.store.read_commit(&op.completed[0]).unwrap().parents,
        vec![main]
    );
    assert_ne!(op.completed[0], resolved);
    assert_eq!(r.text(&op.completed[1], "file"), "both\n");
    assert_eq!(r.text(&op.completed[1], "extra"), "second");
}

#[test]
fn structural_modify_delete_conflict_can_be_resolved_by_deletion() {
    let mut r = Repo::new();
    let base = r.commit(None, &[("file", Some("base"))], "base");
    let a = r.commit(Some(&base), &[("file", Some("edited"))], "edit");
    let main = r.commit(Some(&base), &[("file", None)], "delete");
    let mut op = Operation::start(
        &r.store,
        vec![layer("first", &base, &a)],
        main,
        Method::Rebase,
    )
    .unwrap();
    op.advance(&mut r.store).unwrap();
    let p = op.pending.as_ref().unwrap();
    assert!(p
        .result
        .messages
        .iter()
        .any(|m| m.kind.contains("modify/delete")));
    let draft = p.draft.clone();
    let resolved = r.commit(Some(&draft), &[("file", None)], "accept deletion");
    op.resume(&mut r.store, &resolved).unwrap();
    assert!(op.is_complete());
    assert!(r
        .store
        .read_tree(&r.store.read_commit(&op.completed[0]).unwrap().tree)
        .unwrap()
        .is_empty());
}

#[test]
fn lower_layer_edits_propagate_using_its_old_boundary() {
    let mut r = Repo::new();
    let base = r.commit(None, &[("base", Some("base"))], "base");
    let a = r.commit(Some(&base), &[("a", Some("one"))], "a");
    let b = r.commit(Some(&a), &[("b", Some("two"))], "b");
    let edited_a = r.commit(Some(&a), &[("a", Some("changed"))], "revise a");
    let mut op = Operation::start(
        &r.store,
        vec![layer("a", &base, &edited_a), layer("b", &a, &b)],
        base,
        Method::Rebase,
    )
    .unwrap();
    op.advance(&mut r.store).unwrap();
    assert_eq!(op.completed[0], edited_a);
    assert_eq!(r.text(&op.completed[1], "a"), "changed");
    assert_eq!(r.text(&op.completed[1], "b"), "two");
}

#[test]
fn merge_updates_keep_both_parents_and_linear_rebase_rejects_merge_history() {
    let mut r = Repo::new();
    let base = r.commit(None, &[("base", Some("base"))], "base");
    let a = r.commit(Some(&base), &[("a", Some("a"))], "a");
    let main = r.commit(Some(&base), &[("new", Some("new"))], "new main");
    let mut op = Operation::start(
        &r.store,
        vec![layer("a", &base, &a)],
        main.clone(),
        Method::Merge,
    )
    .unwrap();
    op.advance(&mut r.store).unwrap();
    assert!(op.is_complete());
    assert_eq!(
        r.store.read_commit(&op.completed[0]).unwrap().parents,
        vec![a, main]
    );
    assert!(Operation::start(
        &r.store,
        vec![layer("a", &base, &op.completed[0])],
        base,
        Method::Rebase
    )
    .unwrap_err()
    .contains("not a linear history"));
}

#[test]
fn empty_layers_and_unchanged_stack_preserve_identity() {
    let mut r = Repo::new();
    let base = r.commit(None, &[("file", Some("x"))], "base");
    let mut op = Operation::start(
        &r.store,
        vec![layer("empty", &base, &base)],
        base.clone(),
        Method::Rebase,
    )
    .unwrap();
    op.advance(&mut r.store).unwrap();
    assert_eq!(op.completed, vec![base]);
    assert!(op.is_complete());
}

#[test]
fn malformed_saved_cursor_is_an_error_not_a_panic() {
    let mut r = Repo::new();
    let base = r.commit(None, &[], "base");
    let mut op = Operation::start(
        &r.store,
        vec![layer("empty", &base, &base)],
        base,
        Method::Rebase,
    )
    .unwrap();
    op.cursor = 50;
    assert!(op.advance(&mut r.store).is_err());
}

#[test]
fn rewritten_commits_drop_signatures_but_keep_encoding() {
    assert_eq!(unsigned_headers(b"encoding UTF-8\ngpgsig signature\n continuation\nmergetag tag\n continuation\ncustom okay\n"),
        b"encoding UTF-8\ncustom okay\n");
}

#[test]
fn partial_object_store_does_not_fetch_an_unchanged_large_blob() {
    let mut r = Repo::new();
    for key in ["uploadpack.allowFilter", "uploadpack.allowAnySHA1InWant"] {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&r.dir)
            .args(["config", key, "true"])
            .status()
            .unwrap()
            .success());
    }
    let big = "unchanged content ".repeat(100_000);
    let base = r.commit(None, &[("large", Some(&big))], "base");
    let a = r.commit(Some(&base), &[("a", Some("a"))], "a");
    let main = r.commit(Some(&base), &[("main", Some("new"))], "main");
    let base_tree = r.store.read_commit(&base).unwrap().tree;
    let big_oid = Snapshot::new(&r.store, base_tree)
        .entry("large")
        .unwrap()
        .unwrap()
        .oid;
    let name = format!(
        "caos-stack-partial-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut store =
        GitStore::scratch_partial(&name, &format!("file://{}", r.dir.display())).unwrap();
    let mut op = Operation::start(
        &store,
        vec![layer("first", &base, &a)],
        main,
        Method::Rebase,
    )
    .unwrap();
    assert!(!store.has_local(&big_oid).unwrap());
    op.advance(&mut store).unwrap();
    assert!(op.is_complete());
    assert!(
        !store.has_local(&big_oid).unwrap(),
        "restacking hydrated unchanged file contents"
    );
    assert!(!std::env::temp_dir().join(&name).join("index").exists());
    let mut conversation = TreeBuilder::from(None);
    conversation.put_oid("source", Mode::Commit, a.clone());
    conversation.put("note", Mode::Blob, b"before".to_vec());
    let tree = conversation.build(&mut r.store).unwrap();
    let mut info = r.store.read_commit(&base).unwrap();
    info.tree = tree;
    info.parents.clear();
    let head = r.store.write_commit(&info).unwrap();
    let refname = "refs/conversations/lazy";
    assert!(std::process::Command::new("git")
        .arg("-C")
        .arg(&r.dir)
        .args(["update-ref", refname, head.as_str()])
        .status()
        .unwrap()
        .success());
    assert_eq!(store.fetch_ref(refname).unwrap(), Some(head.clone()));
    let mut info = store.read_commit(&head).unwrap();
    let mut changed = TreeBuilder::from(Some(info.tree));
    changed.put("note", Mode::Blob, b"after".to_vec());
    info.tree = changed.build(&mut store).unwrap();
    info.parents = vec![head.clone()];
    let next = store.write_commit(&info).unwrap();
    store
        .push(&[crate::v3::RefUpdate {
            refname: refname.into(),
            expected: Some(head),
            new: Some(next),
        }])
        .unwrap();
    assert!(
        !store.has_local(&big_oid).unwrap(),
        "conversation push hydrated source gitlinks"
    );
    drop(store);
    std::fs::remove_dir_all(std::env::temp_dir().join(name)).unwrap();
}

#[test]
fn conflict_parser_keeps_unusual_filenames_and_messages_without_stage_rows() {
    let h = "a".repeat(40);
    let bytes = format!("{h}\0\0").replace("\\0", "\0").into_bytes();
    // A clean merge requested with --messages has an empty stage section.
    assert!(!super::git::parse_merge(&bytes, false).unwrap().conflicted);
    let mut record = format!("{h}\0\0").into_bytes();
    record.extend_from_slice(
        b"1\0tab\tand\nnewline\0CONFLICT (directory rename suggested)\0explanation\0",
    );
    let result = super::git::parse_merge(&record, true).unwrap();
    assert!(result.conflicted);
    assert!(result.stages.is_empty());
    assert_eq!(result.messages[0].paths, vec!["tab\tand\nnewline"]);
}

#[test]
fn continuation_can_pause_again_and_merge_conflicts_keep_intended_parents() {
    for method in [Method::Rebase, Method::Merge] {
        let mut r = Repo::new();
        let base = r.commit(None, &[("file", Some("base\n"))], "base");
        let a = r.commit(Some(&base), &[("file", Some("first\n"))], "first");
        let b = r.commit(Some(&a), &[("file", Some("second\n"))], "second");
        let main = r.commit(Some(&base), &[("file", Some("main\n"))], "main");
        let mut op = Operation::start(
            &r.store,
            vec![layer("a", &base, &a), layer("b", &a, &b)],
            main.clone(),
            method,
        )
        .unwrap();
        op.advance(&mut r.store).unwrap();
        let first_draft = op.pending.as_ref().unwrap().draft.clone();
        let resolved = r.commit(
            Some(&first_draft),
            &[("file", Some("resolved first\n"))],
            "resolve",
        );
        op.resume(&mut r.store, &resolved).unwrap();
        assert_eq!(op.completed.len(), 1);
        let second_draft = op.pending.as_ref().unwrap().draft.clone();
        let encoded = serde_json::to_vec(&op).unwrap();
        let mut op: Operation = serde_json::from_slice(&encoded).unwrap();
        let resolved = r.commit(
            Some(&second_draft),
            &[("file", Some("resolved second\n"))],
            "resolve",
        );
        op.resume(&mut r.store, &resolved).unwrap();
        assert!(op.is_complete());
        let first_parents = r.store.read_commit(&op.completed[0]).unwrap().parents;
        let second_parents = r.store.read_commit(&op.completed[1]).unwrap().parents;
        match method {
            Method::Rebase => {
                assert_eq!(first_parents, vec![main]);
                assert_eq!(second_parents, vec![op.completed[0].clone()]);
            }
            Method::Merge => {
                assert_eq!(first_parents, vec![a, main]);
                assert_eq!(second_parents, vec![b, op.completed[0].clone()]);
            }
        }
        assert!(!r.store.is_ancestor(&first_draft, &op.completed[1]).unwrap());
        assert!(!r
            .store
            .is_ancestor(&second_draft, &op.completed[1])
            .unwrap());
        assert_eq!(r.text(&op.completed[1], "file"), "resolved second\n");
    }
}
