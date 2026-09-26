//! A scoped writer replaces one ordinary conversation directory. Its task
//! contains only that tree; invocation metadata stays in the conversation.
use super::*;
use conversation_protocol::v3::Snapshot;

pub(super) fn input(store: &dyn ObjectStore, root: &Oid, scope: &str) -> Result<Oid, String> {
    paths::validate_source_tree_name(scope)?;
    match Snapshot::new(store, root.clone()).entry(scope)? {
        Some(entry) if entry.mode == Mode::Tree => Ok(entry.oid),
        _ => Err(format!(
            "writer scope {scope:?} must be a conversation directory"
        )),
    }
}

pub(super) fn saved<S: progress::RefStore>(
    state: &progress::State<S>,
    record: &CallRecord,
) -> Result<Option<String>, String> {
    let path = format!(
        "{}/scope.txt",
        paths::call_payload_dir(record.request.as_str(), record.round, &record.id)
    );
    state
        .conversation()?
        .optional_payload(&path)?
        .map(|bytes| String::from_utf8(bytes).map_err(|_| "invalid writer scope".into()))
        .transpose()
}

pub(super) fn graft(
    store: &mut dyn ObjectStore,
    root: &Oid,
    scope: &str,
    replacement: Oid,
) -> Result<Oid, String> {
    input(store, root, scope)?;
    let mut proposal = conversation_protocol::v3::TreeBuilder::from(Some(root.clone()));
    proposal.delete(scope);
    proposal.put_oid(scope, Mode::Tree, replacement);
    proposal.build(store)
}

pub(super) fn change(
    store: &dyn ObjectStore,
    before: &Oid,
    after: &Oid,
    scope: &str,
) -> Result<conversation_protocol::v3::tree::Change, String> {
    Ok(conversation_protocol::v3::tree::Change {
        path: scope.to_owned(),
        before: Some((Mode::Tree, input(store, before, scope)?)),
        after: match Snapshot::new(store, after.clone()).entry(scope)? {
            Some(entry) if entry.mode == Mode::Tree => Some((Mode::Tree, entry.oid)),
            None => None, // Git omits empty directories.
            _ => return Err("scoped proposal must be a directory".into()),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversation_protocol::v3::{GitStore, TreeBuilder};

    struct Temp(std::path::PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Temp, GitStore, Oid, Oid) {
        let path = std::env::temp_dir().join(fresh_name("scoped-writer-test"));
        assert!(std::process::Command::new("git")
            .args(["init", "--quiet", "--bare"])
            .arg(&path)
            .status()
            .unwrap()
            .success());
        let mut store = GitStore::open(&path, None).unwrap();
        let mut feature = TreeBuilder::from(None);
        feature.put("00.base", Mode::Blob, b"base".to_vec());
        // Rewritten gitlinks must be replaced, never reconciled as code edits.
        feature.put_oid(
            "00-work",
            Mode::Commit,
            Oid::parse(&"a".repeat(40), "test").unwrap(),
        );
        let feature = feature.build(&mut store).unwrap();
        let mut root = TreeBuilder::from(None);
        root.put_oid("feature", Mode::Tree, feature.clone());
        root.put("notes", Mode::Blob, b"before".to_vec());
        let root = root.build(&mut store).unwrap();
        (Temp(path), store, root, feature)
    }

    #[test]
    fn unrelated_conversation_edits_do_not_change_writer_input_or_block_replacement() {
        let (_temp, mut store, root, feature) = fixture();
        let mut output = TreeBuilder::from(Some(feature.clone()));
        output.put_oid(
            "00-work",
            Mode::Commit,
            Oid::parse(&"b".repeat(40), "test").unwrap(),
        );
        let output = output.build(&mut store).unwrap();
        let proposed = graft(&mut store, &root, "feature", output.clone()).unwrap();
        let change = change(&store, &root, &proposed, "feature").unwrap();
        let mut current = TreeBuilder::from(Some(root));
        current.put("notes", Mode::Blob, b"after".to_vec());
        let current = current.build(&mut store).unwrap();
        assert_eq!(input(&store, &current, "feature").unwrap(), feature);
        let plan = plan_file_changes(&mut store, &[change], &current, None).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(
            plan.files,
            vec![("feature".into(), Some((Mode::Tree, output.encode_line())))]
        );
    }

    #[test]
    fn concurrent_scope_edit_rejects_the_whole_proposal() {
        let (_temp, mut store, root, feature) = fixture();
        let mut output = TreeBuilder::from(Some(feature.clone()));
        output.put("00.base", Mode::Blob, b"proposed".to_vec());
        let output = output.build(&mut store).unwrap();
        let proposed = graft(&mut store, &root, "feature", output).unwrap();
        let change = change(&store, &root, &proposed, "feature").unwrap();
        let mut current = TreeBuilder::from(Some(root));
        current.put("feature/notes", Mode::Blob, b"concurrent edit".to_vec());
        let current = current.build(&mut store).unwrap();
        let plan = plan_file_changes(&mut store, &[change], &current, None).unwrap();
        assert_eq!(plan.conflicts, ["feature"]);
        assert!(plan.files.is_empty());
    }

    #[test]
    fn scope_cannot_be_protocol_state_a_gitlink_or_a_path_through_one() {
        let (_temp, store, root, _) = fixture();
        for scope in [
            ".caos",
            "../feature",
            "feature/00-work",
            "feature/00-work/src",
            "notes",
            "missing",
        ] {
            assert!(input(&store, &root, scope).is_err(), "{scope}");
        }
    }

    fn files_transition(store: &mut GitStore, head: &Oid, files: FileEdits) -> Oid {
        let transition = Transition::FilesApply { files };
        let applied = apply(store, Some(head), &transition).unwrap();
        let signature = inherited_signature(store, head).unwrap();
        mint(store, head, &applied, transition.kind(), &signature).unwrap()
    }

    #[test]
    fn directory_replacement_applies_through_the_conversation_transition() {
        let (_temp, mut store, _, feature) = fixture();
        let root = conversation_protocol::v3::fixtures::golden(&mut store);
        let before = files_transition(
            &mut store,
            &root,
            vec![("feature".into(), Some((Mode::Tree, feature.encode_line())))],
        );
        let before_tree = store.tree_of(&before).unwrap();
        let mut replacement = TreeBuilder::from(None);
        replacement.put("replacement", Mode::Blob, b"new contents".to_vec());
        let replacement = replacement.build(&mut store).unwrap();
        let proposed = graft(&mut store, &before_tree, "feature", replacement.clone()).unwrap();
        let change = change(&store, &before_tree, &proposed, "feature").unwrap();
        let plan = plan_file_changes(&mut store, &[change], &before_tree, None).unwrap();
        let after = files_transition(&mut store, &before, plan.files);
        let after_tree = store.tree_of(&after).unwrap();
        let snapshot = Snapshot::new(&store, after_tree);
        assert_eq!(snapshot.entry("feature").unwrap().unwrap().oid, replacement);
        assert!(!snapshot.exists("feature/00-work").unwrap());
        assert_eq!(
            snapshot.read("feature/replacement").unwrap().unwrap(),
            b"new contents"
        );
        assert_eq!(
            snapshot.read("notes.md").unwrap().unwrap(),
            b"seeded notes\n"
        );
    }

    #[test]
    fn empty_output_deletes_only_the_scope_and_is_retry_safe() {
        let (_temp, mut store, root, _) = fixture();
        let proposed = graft(
            &mut store,
            &root,
            "feature",
            conversation_protocol::v3::oid::empty_tree(),
        )
        .unwrap();
        let change = change(&store, &root, &proposed, "feature").unwrap();
        assert_eq!(change.after, None);
        let plan =
            plan_file_changes(&mut store, std::slice::from_ref(&change), &root, None).unwrap();
        assert_eq!(plan.files, [("feature".into(), None)]);
        assert!(plan.conflicts.is_empty());
        let retry = plan_file_changes(&mut store, &[change], &proposed, None).unwrap();
        assert!(retry.files.is_empty());
        assert!(retry.conflicts.is_empty());
        assert_eq!(
            Snapshot::new(&store, proposed)
                .read("notes")
                .unwrap()
                .unwrap(),
            b"before"
        );
    }
}
