//! Promote the final work snapshot into a named commit and start fresh work.
use crate::stack::read_stack;
use conversation_protocol::v3::paths;
use conversation_protocol::v3::{CommitInfo, Mode, ObjectStore, Oid, Signature, TreeBuilder};
use serde_json::{json, Value};

pub struct Parameters {
    pub stack: String,
    pub name: String,
    pub author: Signature,
    pub committer: Signature,
    pub message: Vec<u8>,
}

#[derive(Debug)]
pub struct Proposal {
    pub tree: Oid,
    pub report: Value,
}

pub fn add_layer(
    store: &mut dyn ObjectStore,
    root: &Oid,
    p: &Parameters,
) -> Result<Proposal, String> {
    paths::validate_component(&p.name)?;
    if p.name == "work" || p.name.chars().any(char::is_whitespace) {
        return Err("name must be a layer name other than work, without whitespace".into());
    }
    let stack = read_stack(store, root, &p.stack)?;
    if stack.entries.iter().any(|entry| entry.name == "rebase") {
        return Err("finish or abort the stack's replay before adding a layer".into());
    }
    let work = stack.layers.last().expect("read_stack requires a layer");
    if work.name != format!("{:02}-work", work.number) {
        return Err("the final stack layer must be named <number>-work".into());
    }
    if work.number > 0 && work.base != stack.layers[work.number - 1].commit {
        return Err(
            "work base differs from its predecessor; replay the work onto it before adding a layer"
                .into(),
        );
    }
    let name = format!("{:02}-{}", work.number, p.name);
    paths::validate_component(&name)?;
    let next_name = format!("{:02}-work", work.number + 1);
    let current = store.read_commit(&work.commit).map_err(String::from)?;
    let commit = store
        .write_commit(&CommitInfo {
            tree: current.tree,
            parents: vec![work.base.clone()],
            author: p.author.clone(),
            committer: p.committer.clone(),
            extra_headers: Vec::new(),
            message: p.message.clone(),
        })
        .map_err(String::from)?;
    let mut proposal = TreeBuilder::from(Some(root.clone()));
    proposal.delete(&format!("{}/{}", p.stack, work.name));
    proposal.put_oid(&format!("{}/{name}", p.stack), Mode::Commit, commit.clone());
    proposal.put_oid(
        &format!("{}/{next_name}", p.stack),
        Mode::Commit,
        commit.clone(),
    );
    proposal.put(
        &format!("{}/{:02}.base", p.stack, work.number + 1),
        Mode::Blob,
        commit.encode_line(),
    );
    let tree = proposal.build(store)?;
    Ok(Proposal {
        tree,
        report: json!({
            "kind": "git-add-layer",
            "stack": p.stack,
            "layers": [ {"name": name, "commit": commit}, {"name": next_name, "commit": commit} ],
            "previous_work": work.commit,
            "base": work.base,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversation_protocol::v3::{MemoryStore, Snapshot};

    fn signature(time: i64) -> Signature {
        Signature {
            name: "Author".into(),
            email: "author@example.com".into(),
            time,
            offset: "+0000".into(),
        }
    }
    fn commit(store: &mut MemoryStore, text: &str, parents: Vec<Oid>) -> Oid {
        let mut b = TreeBuilder::from(None);
        b.put("code", Mode::Blob, text.as_bytes().to_vec());
        let tree = b.build(store).unwrap();
        store
            .write_commit(&CommitInfo {
                tree,
                parents,
                author: signature(1),
                committer: signature(1),
                extra_headers: Vec::new(),
                message: b"tool call".to_vec(),
            })
            .unwrap()
    }
    fn params() -> Parameters {
        Parameters {
            stack: "feature".into(),
            name: "first".into(),
            author: signature(2),
            committer: signature(3),
            message: b"Intentional subject\n\nBody".to_vec(),
        }
    }
    fn setup(store: &mut MemoryStore) -> (Oid, Oid, Oid) {
        let base = commit(store, "initial", vec![]);
        let first = commit(store, "intermediate", vec![base.clone()]);
        let work = commit(store, "final", vec![first]);
        let mut b = TreeBuilder::from(None);
        b.put_oid("feature/00-work", Mode::Commit, work.clone());
        b.put("feature/00.base", Mode::Blob, base.encode_line());
        b.put("feature/notes", Mode::Blob, b"retain notes".to_vec());
        b.put(
            "elsewhere/file",
            Mode::Blob,
            b"retain unrelated files".to_vec(),
        );
        (b.build(store).unwrap(), base, work)
    }

    #[test]
    fn promotion_squashes_history_and_starts_next_work_at_the_new_commit() {
        let mut store = MemoryStore::new();
        let (root, base, work) = setup(&mut store);
        let p = params();
        let proposal = add_layer(&mut store, &root, &p).unwrap();
        let stack = read_stack(&store, &proposal.tree, "feature").unwrap();
        assert_eq!(stack.layers.len(), 2);
        assert_eq!(stack.layers[0].name, "00-first");
        assert_eq!(stack.layers[0].base, base);
        let named = &stack.layers[0].commit;
        let info = store.read_commit(named).unwrap();
        assert_eq!(info.tree, store.read_commit(&work).unwrap().tree);
        assert_eq!(info.parents, vec![base]);
        assert_eq!(info.author, p.author);
        assert_eq!(info.committer, p.committer);
        assert_eq!(info.message, p.message);
        assert!(info.extra_headers.is_empty());
        assert_eq!(stack.layers[1].name, "01-work");
        assert_eq!(&stack.layers[1].commit, named);
        assert_eq!(&stack.layers[1].base, named);
        let result = Snapshot::new(&store, proposal.tree);
        assert!(!result.exists("feature/00-work").unwrap());
        assert_eq!(
            result.read("feature/notes").unwrap().unwrap(),
            b"retain notes"
        );
        assert_eq!(
            result.read("elsewhere/file").unwrap().unwrap(),
            b"retain unrelated files"
        );
        assert!(Snapshot::new(&store, root)
            .exists("feature/00-work")
            .unwrap());
    }

    #[test]
    fn promotion_does_not_read_the_source_tree_or_its_files() {
        let mut store = MemoryStore::new();
        let (root, _, work) = setup(&mut store);
        let opaque_tree = Oid::parse(&"a".repeat(40), "opaque source tree").unwrap();
        let mut info = store.read_commit(&work).unwrap();
        info.tree = opaque_tree.clone();
        let work = store.write_commit(&info).unwrap();
        let mut b = TreeBuilder::from(Some(root));
        b.put_oid("feature/00-work", Mode::Commit, work);
        let root = b.build(&mut store).unwrap();
        // The worker can copy this pointer without loading even the source tree.
        assert!(!store.contains(&opaque_tree));
        let proposal = add_layer(&mut store, &root, &params()).unwrap();
        let stack = read_stack(&store, &proposal.tree, "feature").unwrap();
        assert_eq!(
            store.read_commit(&stack.layers[0].commit).unwrap().tree,
            opaque_tree
        );
    }

    #[test]
    fn next_promotion_uses_the_preceding_named_layer_as_parent() {
        let mut store = MemoryStore::new();
        let (root, _, _) = setup(&mut store);
        let first = add_layer(&mut store, &root, &params()).unwrap();
        let original = read_stack(&store, &first.tree, "feature").unwrap();
        let second = add_layer(
            &mut store,
            &first.tree,
            &Parameters {
                name: "second".into(),
                ..params()
            },
        )
        .unwrap();
        let stack = read_stack(&store, &second.tree, "feature").unwrap();
        assert_eq!(stack.layers[1].name, "01-second");
        let info = store.read_commit(&stack.layers[1].commit).unwrap();
        assert_eq!(info.parents, vec![original.layers[0].commit.clone()]);
        assert_eq!(stack.layers[2].name, "02-work");
        assert_eq!(stack.layers[2].base, stack.layers[1].commit);
    }

    #[test]
    fn rejects_a_stale_predecessor_without_reparenting_work() {
        let mut store = MemoryStore::new();
        let (root, base, _) = setup(&mut store);
        let first = add_layer(&mut store, &root, &params()).unwrap();
        let replacement = commit(&mut store, "changed predecessor", vec![base]);
        let mut b = TreeBuilder::from(Some(first.tree));
        b.put_oid("feature/00-first", Mode::Commit, replacement);
        let root = b.build(&mut store).unwrap();
        assert!(add_layer(&mut store, &root, &params())
            .unwrap_err()
            .contains("predecessor"));
    }

    #[test]
    fn rejects_replay_and_missing_work_or_reserved_name() {
        let mut store = MemoryStore::new();
        let (root, _, work) = setup(&mut store);
        let mut b = TreeBuilder::from(Some(root.clone()));
        b.put("feature/rebase/plan", Mode::Blob, b"onto=...".to_vec());
        let replay = b.build(&mut store).unwrap();
        assert!(add_layer(&mut store, &replay, &params())
            .unwrap_err()
            .contains("replay"));
        assert!(add_layer(
            &mut store,
            &root,
            &Parameters {
                name: "work".into(),
                ..params()
            }
        )
        .is_err());
        let mut b = TreeBuilder::from(Some(root));
        b.delete("feature/00-work");
        b.put_oid("feature/00-named", Mode::Commit, work);
        let named = b.build(&mut store).unwrap();
        assert!(add_layer(&mut store, &named, &params())
            .unwrap_err()
            .contains("final stack layer"));
    }
}
