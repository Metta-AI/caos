use super::*;
use conversation_protocol::v3::{MemoryStore, ObjectStore};
use std::collections::HashMap;

fn signature(time: i64) -> Signature {
    Signature {
        name: "Replay".into(),
        email: "test@example.com".into(),
        time,
        offset: "+0000".into(),
    }
}
fn oid(byte: char) -> Oid {
    Oid::parse(&byte.to_string().repeat(40), "test").unwrap()
}
fn plan(onto: &Oid, body: &str) -> String {
    format!("onto={onto}\ncommitter=Replay <test@example.com> 99 +0000\n{body}")
}

#[derive(Default)]
struct Fixture {
    store: MemoryStore,
    messages: HashMap<String, Vec<u8>>,
    reads: Vec<Oid>,
    merges: Vec<(Oid, Oid, Oid)>,
    conflict: Option<(Oid, Oid, Oid)>,
}
impl Fixture {
    fn commit(&mut self, tree: char, parents: Vec<Oid>, time: i64) -> Oid {
        self.store
            .write_commit(&CommitInfo {
                tree: oid(tree),
                parents,
                author: signature(time),
                committer: signature(time),
                extra_headers: Vec::new(),
                message: format!("source {time}\n").into_bytes(),
            })
            .unwrap()
    }
    fn info(&self, oid: &Oid) -> CommitInfo {
        self.store.read_commit(oid).unwrap()
    }
}
impl Backend for Fixture {
    fn read_commit(&mut self, oid: &Oid) -> Result<CommitInfo, String> {
        self.reads.push(oid.clone());
        self.store.read_commit(oid).map_err(String::from)
    }
    fn read_message(&mut self, path: &str) -> Result<Vec<u8>, String> {
        self.messages
            .get(path)
            .cloned()
            .ok_or_else(|| format!("missing message {path}"))
    }
    fn merge_trees(&mut self, base: &Oid, ours: &Oid, theirs: &Oid) -> Result<Merge, String> {
        let triple = (base.clone(), ours.clone(), theirs.clone());
        self.merges.push(triple.clone());
        if self.conflict.as_ref() == Some(&triple) {
            return Ok(Merge {
                tree: oid('f'),
                conflicts: Some(b"native conflict\0report\n".to_vec()),
            });
        }
        // Tests assert each triple separately. The endpoint tree is sufficient
        // to exercise commit metadata and deterministic replay here; the worker
        // fixture exercises actual three-way Git merging.
        Ok(Merge {
            tree: if base == theirs {
                ours.clone()
            } else {
                theirs.clone()
            },
            conflicts: None,
        })
    }
    fn commit_tree(&mut self, commit: &CommitInfo) -> Result<Oid, String> {
        self.store.write_commit(commit).map_err(String::from)
    }
}

#[test]
fn range_reads_only_endpoints_and_builds_one_commit() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let b = f.commit('b', vec![oid('9')], 900); // Intermediate history is deliberately unavailable.
    let h = f.commit('c', vec![a.clone()], 2);
    let source = plan(&h, &format!("pick={a}..{b}\nbranch=feature\nbranch=work\n"));
    let out = run(&mut f, &source).unwrap();
    let commit = f.info(&out.layers[0].tip);
    assert_eq!(commit.parents, vec![h.clone()]);
    assert_eq!(commit.tree, oid('b'));
    assert_eq!(commit.author, signature(900));
    assert_eq!(commit.committer, signature(99));
    assert_eq!(commit.message, b"source 900\n");
    assert_eq!(f.merges, vec![(oid('a'), oid('c'), oid('b'))]);
    assert!(f
        .reads
        .iter()
        .all(|read| [a.clone(), b.clone(), h.clone()].contains(read)));
    assert_eq!(out.layers[0].name, "00-feature");
    assert_eq!(out.layers[0].base, h);
    assert_eq!(out.layers[1].name, "01-work");
    assert_eq!(out.layers[1].base, out.layers[0].tip);
    assert_eq!(out.layers[1].tip, out.layers[0].tip);
    assert_eq!(run(&mut f, &source).unwrap().layers, out.layers);
}

#[test]
fn aligned_single_pick_preserves_object_and_all_metadata() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let b = f.commit('b', vec![a.clone()], 2);
    let mut source = f.info(&b);
    source.extra_headers = b"gpgsig original\n continuation\nx-custom data\n".to_vec();
    let b = f.store.write_commit(&source).unwrap();
    let out = run(&mut f, &plan(&a, &format!("pick={b}\nbranch=feature\n"))).unwrap();
    assert_eq!(out.layers[0].tip, b);
    assert_eq!(f.info(&b), source);
    assert!(f.merges.is_empty());
}

#[test]
fn one_commit_range_is_still_an_explicit_new_commit() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let b = f.commit('b', vec![a.clone()], 2);
    let out = run(
        &mut f,
        &plan(&a, &format!("pick={a}..{b}\nbranch=feature\n")),
    )
    .unwrap();
    assert_ne!(out.layers[0].tip, b);
    assert_eq!(f.info(&out.layers[0].tip).committer, signature(99));
}

#[test]
fn prefix_repeats_exactly_when_suffix_changes() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let b = f.commit('b', vec![a.clone()], 2);
    let c = f.commit('c', vec![b.clone()], 3);
    let h = f.commit('d', vec![a.clone()], 4);
    let prefix = format!("pick={b}\nbranch=first\n");
    let first = run(
        &mut f,
        &plan(&h, &format!("{prefix}pick={c}\nbranch=second\n")),
    )
    .unwrap();
    f.messages
        .insert("message".into(), b"replacement\n".to_vec());
    let changed = run(
        &mut f,
        &plan(
            &h,
            &format!("{prefix}pick={c}\nmessage=message\nbranch=renamed\n"),
        ),
    )
    .unwrap();
    assert_eq!(first.layers[0], changed.layers[0]);
    assert_ne!(first.layers[1].tip, changed.layers[1].tip);
    assert_eq!(changed.layers[1].base, first.layers[0].tip);
}

#[test]
fn conflict_repair_is_an_explicit_range_and_does_not_import_draft_history() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let b = f.commit('b', vec![a.clone()], 2);
    let c = f.commit('c', vec![a.clone()], 3);
    let h = f.commit('d', vec![a.clone()], 4);
    let prefix = format!("pick={b}\nbranch=lower\n");
    f.conflict = Some((oid('a'), oid('b'), oid('c')));
    let source = plan(&h, &format!("{prefix}pick={c}\nbranch=upper\n"));
    let first = run(&mut f, &source).unwrap();
    let conflict = first.conflict.unwrap();
    let again = run(&mut f, &source).unwrap().conflict.unwrap();
    assert_eq!(conflict.draft, again.draft);
    assert_eq!(conflict.parent, first.layers[0].tip);
    assert_eq!(conflict.line, 5);
    assert_eq!(
        f.info(&conflict.draft).parents,
        vec![conflict.parent.clone()]
    );
    assert_eq!(conflict.report, b"native conflict\0report\n");
    let draft_edit = f.commit('e', vec![conflict.draft], 20);
    let resolved = f.commit('e', vec![draft_edit], 21);
    let repaired = plan(
        &h,
        &format!(
            "{prefix}pick={}..{resolved}\nbranch=upper\n",
            conflict.parent
        ),
    );
    let out = run(&mut f, &repaired).unwrap();
    assert!(out.conflict.is_none());
    assert_eq!(out.layers[0], first.layers[0]);
    let final_commit = f.info(&out.layers[1].tip);
    assert_eq!(final_commit.parents, vec![conflict.parent]);
    assert_eq!(final_commit.tree, oid('e'));
    assert_ne!(out.layers[1].tip, resolved);
    assert_eq!(run(&mut f, &repaired).unwrap().layers, out.layers);
}

#[test]
fn missing_later_message_is_not_read_before_conflict() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let b = f.commit('b', vec![a.clone()], 2);
    let h = f.commit('c', vec![a.clone()], 3);
    f.conflict = Some((oid('a'), oid('c'), oid('b')));
    let source = plan(
        &h,
        &format!("pick={b}\nmessage=messages/not-yet-written\nbranch=feature\n"),
    );
    assert!(run(&mut f, &source).unwrap().conflict.is_some());
    f.conflict = None;
    assert!(run(&mut f, &source)
        .unwrap_err()
        .contains("missing message"));
}

#[test]
fn message_reads_exact_bytes_and_same_message_preserves_original_commit() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let b = f.commit('b', vec![a.clone()], 2);
    f.messages.insert("msg".into(), f.info(&b).message);
    let source = plan(&a, &format!("pick={b}\nmessage=msg\nbranch=feature\n"));
    assert_eq!(run(&mut f, &source).unwrap().layers[0].tip, b);
    let bytes = b"  new subject  \n\nbody=literal # text\n\n";
    f.messages.insert("msg".into(), bytes.to_vec());
    let out = run(&mut f, &source).unwrap();
    let info = f.info(&out.layers[0].tip);
    assert_eq!(info.message, bytes);
    assert_eq!(info.parents, vec![a]);
    assert_eq!(info.author, signature(2));
    assert_eq!(info.committer, signature(99));
    assert_eq!(info.tree, oid('b'));
    assert_eq!(run(&mut f, &source).unwrap().layers, out.layers);
}

#[test]
fn rewriting_preserves_encoded_source_and_removes_invalidated_signatures() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let b = f.commit('b', vec![a.clone()], 2);
    let h = f.commit('c', vec![a.clone()], 3);
    let mut info = f.info(&b);
    info.message = b"caf\xe9\n".to_vec();
    info.extra_headers = b"gpgsig signature\n continuation\nencoding ISO-8859-1\nx-custom value\n continued\ngpgsig-sha256 sig\n continuation\nmergetag object hash\n type commit\n".to_vec();
    let b = f.store.write_commit(&info).unwrap();
    let prefix = format!("pick={b}\n");
    let out = run(&mut f, &plan(&h, &format!("{prefix}branch=feature\n"))).unwrap();
    let info = f.info(&out.layers[0].tip);
    assert_eq!(info.message, b"caf\xe9\n");
    assert_eq!(
        info.extra_headers,
        b"encoding ISO-8859-1\nx-custom value\n continued\n"
    );
    f.messages.insert("msg".into(), b"fresh message\n".to_vec());
    let out = run(
        &mut f,
        &plan(&h, &format!("{prefix}message=msg\nbranch=feature\n")),
    )
    .unwrap();
    assert_eq!(
        f.info(&out.layers[0].tip).extra_headers,
        b"x-custom value\n continued\n"
    );
}

#[test]
fn empty_range_keeps_an_empty_commit_and_multiple_picks_keep_multiple_commits() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let b = f.commit('b', vec![a.clone()], 2);
    let out = run(
        &mut f,
        &plan(&a, &format!("pick={a}..{a}\npick={b}\nbranch=feature\n")),
    )
    .unwrap();
    let last = f.info(&out.layers[0].tip);
    let empty = f.info(&last.parents[0]);
    assert_eq!(empty.parents, vec![a.clone()]);
    assert_eq!(empty.tree, f.info(&a).tree);
    assert_eq!(last.tree, f.info(&b).tree);
}

#[test]
fn root_and_merge_single_picks_require_an_explicit_range() {
    let mut f = Fixture::default();
    let a = f.commit('a', vec![], 1);
    let merge = f.commit('b', vec![a.clone(), a.clone()], 2);
    for picked in [&a, &merge] {
        assert!(run(
            &mut f,
            &plan(&a, &format!("pick={picked}\nbranch=feature\n"))
        )
        .unwrap_err()
        .contains("exactly one parent"));
    }
}

#[test]
fn grammar_requires_signature_sealed_tip_and_scoped_message_paths() {
    let a = oid('a');
    for body in [
        format!("pick={a}\n"),
        "message=msg\nbranch=x\n".into(),
        format!("pick={a}\nbranch=x\nmessage=msg\n"),
        "branch=00-x\n".into(),
        "branch=../x\n".into(),
        "branch=x y\n".into(),
        format!("pick={a}\nmessage=../msg\nbranch=x\n"),
        format!("pick={a}\nmessage=/msg\nbranch=x\n"),
        format!("squash={a}\nbranch=x\n"),
        format!("drop={a}\nbranch=x\n"),
        "amend=x\nbranch=x\n".into(),
        "continue=true\nbranch=x\n".into(),
        format!("done {a} pick={a}\nbranch=x\n"),
    ] {
        assert!(Plan::parse(&plan(&a, &body)).is_err(), "{body}");
    }
    assert!(Plan::parse(&format!("onto={a}\nbranch=x\n")).is_err());
    assert!(Plan::parse(&format!(
        "committer=Replay <x> 1 +0000\nonto={a}\nbranch=x\n"
    ))
    .is_err());
    assert!(Plan::parse(&plan(&a, "# note\n\nbranch=x\nbranch=x\n")).is_ok());
}
