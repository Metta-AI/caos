//! The `root` contract (design/tool-contract.md, "`root`"): which object a
//! tool is given, and how a writer's new object is put back.
//!
//! A tool that declares `@root` takes one required argument, `root`: a path
//! relative to the session root. It is given the object there (a blob, a tree,
//! or a gitlink's commit) and nothing else. A writer answers with a new object
//! for that path, or with `deleted`; the harness then rewrites everything
//! above it, making a child commit at every gitlink the path crosses, and hands
//! the result to the same reconciliation every proposal goes through.
use session_protocol::v3::apply::inherited_signature;
use session_protocol::v3::paths;
use session_protocol::v3::tree::{canonical_tree_order, CommitInfo, Mode, ObjectStore, TreeEntry};
use session_protocol::v3::Oid;

/// What the model is told the `root` parameter is.
pub const ROOT_DOC: &str = "The path this tool works on, relative to the session \
                            root: a file, a directory, or a source tree such as \
                            sources/<repo>/<layer>. `.` is the whole session. \
                            Required: nothing is implied by where the tool sits.";

/// A `root` argument as path components, relative to the session root. `.`
/// (or an empty value) is the session root itself, as no components.
pub fn components(root: &str) -> Result<Vec<String>, String> {
    let trimmed = root.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return Ok(Vec::new());
    }
    paths::validate_tree_path(trimmed)
        .map_err(|error| format!("root {root:?} is not a path in the session: {error}"))?;
    if trimmed == ".caos" || trimmed.starts_with(".caos/") {
        return Err(".caos is protocol metadata and cannot be a root".to_string());
    }
    Ok(trimmed.split('/').map(str::to_string).collect())
}

/// The object at `components` under `tree`, descending into a gitlink's commit
/// as into a directory. `None` when nothing is there; an error when something
/// on the way is a file.
pub fn resolve(
    store: &dyn ObjectStore,
    tree: &Oid,
    components: &[String],
) -> Result<Option<(Mode, Oid)>, String> {
    let mut at = (Mode::Tree, tree.clone());
    for (index, name) in components.iter().enumerate() {
        let dir = match at.0 {
            Mode::Tree => at.1.clone(),
            Mode::Commit => store.read_commit(&at.1).map_err(String::from)?.tree,
            _ => return Err(crosses_a_file(&components[..index])),
        };
        let entries = store.read_tree(&dir).map_err(String::from)?;
        let Some(entry) = entries.into_iter().find(|entry| entry.name == *name) else {
            return Ok(None);
        };
        at = (entry.mode, entry.oid);
    }
    Ok(Some(at))
}

fn crosses_a_file(prefix: &[String]) -> String {
    format!("root crosses {}, which is a file", prefix.join("/"))
}

/// The session tree with `new` at `components`, and the commits made on the
/// way: one per gitlink the path crosses, a child of the commit it replaces,
/// deepest first so each can be published before anything names it.
pub struct Rebuilt {
    pub tree: Oid,
    pub commits: Vec<Oid>,
}

/// Put `new` (or nothing, to delete) at `components` under `tree`. A directory
/// left empty by a deletion is dropped; a source tree is kept, with an empty
/// tree. Writing back what was there changes nothing and makes no commit.
pub fn replace(
    store: &mut dyn ObjectStore,
    tree: &Oid,
    components: &[String],
    new: Option<(Mode, Oid)>,
    message: &str,
) -> Result<Rebuilt, String> {
    let mut commits = Vec::new();
    let mut walked = Vec::new();
    let top = replace_in(
        store,
        Some((Mode::Tree, tree.clone())),
        components,
        new,
        message,
        &mut walked,
        &mut commits,
    )?;
    let tree = match top {
        Some((Mode::Tree, oid)) => oid,
        None => store.write_tree(&[]).map_err(String::from)?,
        Some((mode, _)) => return Err(format!("the session root cannot become a {mode:?}")),
    };
    Ok(Rebuilt { tree, commits })
}

fn replace_in(
    store: &mut dyn ObjectStore,
    at: Option<(Mode, Oid)>,
    components: &[String],
    new: Option<(Mode, Oid)>,
    message: &str,
    walked: &mut Vec<String>,
    commits: &mut Vec<Oid>,
) -> Result<Option<(Mode, Oid)>, String> {
    let Some((name, rest)) = components.split_first() else {
        return Ok(new);
    };
    match at {
        // A gitlink on the way: change its code tree, and commit that as a
        // child of the commit it replaces.
        Some((Mode::Commit, commit)) => {
            let tree = store.read_commit(&commit).map_err(String::from)?.tree;
            let inner = replace_in(
                store,
                Some((Mode::Tree, tree.clone())),
                components,
                new,
                message,
                walked,
                commits,
            )?;
            let changed = match inner {
                Some((Mode::Tree, oid)) => oid,
                None => store.write_tree(&[]).map_err(String::from)?,
                Some(_) => unreachable!("a directory walk yields a tree"),
            };
            if changed == tree {
                return Ok(Some((Mode::Commit, commit)));
            }
            let signature = inherited_signature(store, &commit)?;
            let info = CommitInfo {
                tree: changed,
                parents: vec![commit],
                author: signature.clone(),
                committer: signature,
                extra_headers: Vec::new(),
                message: format!("{}\n", message.trim()).into_bytes(),
            };
            let minted = store.write_commit(&info).map_err(String::from)?;
            commits.push(minted.clone());
            Ok(Some((Mode::Commit, minted)))
        }
        Some((Mode::Tree, _)) | None => {
            let mut entries = match &at {
                Some((_, tree)) => store.read_tree(tree).map_err(String::from)?,
                None => Vec::new(),
            };
            let child = entries
                .iter()
                .find(|entry| entry.name == *name)
                .map(|entry| (entry.mode, entry.oid.clone()));
            walked.push(name.clone());
            let replaced = replace_in(store, child, rest, new, message, walked, commits)?;
            walked.pop();
            entries.retain(|entry| entry.name != *name);
            if let Some((mode, oid)) = replaced {
                entries.push(TreeEntry {
                    name: name.clone(),
                    mode,
                    oid,
                });
            }
            if entries.is_empty() && !walked.is_empty() {
                return Ok(None);
            }
            canonical_tree_order(&mut entries);
            let written = store.write_tree(&entries).map_err(String::from)?;
            Ok(Some((Mode::Tree, written)))
        }
        Some(_) => Err(crosses_a_file(walked)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use session_protocol::v3::tree::{MemoryStore, Signature};

    fn blob(store: &mut MemoryStore, text: &str) -> Oid {
        store.write_blob(text.as_bytes()).unwrap()
    }

    fn tree(store: &mut MemoryStore, entries: Vec<(&str, Mode, Oid)>) -> Oid {
        let mut entries: Vec<TreeEntry> = entries
            .into_iter()
            .map(|(name, mode, oid)| TreeEntry {
                name: name.to_string(),
                mode,
                oid,
            })
            .collect();
        canonical_tree_order(&mut entries);
        store.write_tree(&entries).unwrap()
    }

    fn commit(store: &mut MemoryStore, tree: Oid) -> Oid {
        let signature = Signature {
            name: "dev".to_string(),
            email: "dev@caos".to_string(),
            time: 1_700_000_000,
            offset: "+0000".to_string(),
        };
        store
            .write_commit(&CommitInfo {
                tree,
                parents: Vec::new(),
                author: signature.clone(),
                committer: signature,
                extra_headers: Vec::new(),
                message: b"base\n".to_vec(),
            })
            .unwrap()
    }

    fn path(text: &str) -> Vec<String> {
        components(text).unwrap()
    }

    /// A session holding `notes.md` and a source tree at `sources/repo/01-a`
    /// whose code holds `README.md`.
    fn session(store: &mut MemoryStore) -> (Oid, Oid) {
        let readme = blob(store, "hello\n");
        let code = tree(store, vec![("README.md", Mode::Blob, readme)]);
        let layer = commit(store, code);
        let repo = tree(store, vec![("01-a", Mode::Commit, layer.clone())]);
        let sources = tree(store, vec![("repo", Mode::Tree, repo)]);
        let notes = blob(store, "notes\n");
        let root = tree(
            store,
            vec![
                ("notes.md", Mode::Blob, notes),
                ("sources", Mode::Tree, sources),
            ],
        );
        (root, layer)
    }

    #[test]
    fn components_refuse_metadata_and_bad_paths() {
        assert!(path(".").is_empty());
        assert!(path("").is_empty());
        assert_eq!(path("/sources/repo/"), vec!["sources", "repo"]);
        assert!(components(".caos/tools").is_err());
        assert!(components("a/../b").is_err());
    }

    #[test]
    fn resolve_descends_into_a_gitlink() {
        let mut store = MemoryStore::new();
        let (root, layer) = session(&mut store);
        let at = |p: &str| resolve(&store, &root, &path(p)).unwrap();
        assert_eq!(at("sources/repo/01-a"), Some((Mode::Commit, layer)));
        assert_eq!(at("sources/repo/01-a/README.md").unwrap().0, Mode::Blob);
        assert_eq!(at("sources/repo/01-a/missing"), None);
        assert_eq!(at("nowhere/at/all"), None);
        let error = resolve(&store, &root, &path("notes.md/x")).unwrap_err();
        assert!(error.contains("notes.md"), "{error}");
    }

    #[test]
    fn a_file_inside_a_source_tree_makes_a_child_commit() {
        let mut store = MemoryStore::new();
        let (root, layer) = session(&mut store);
        let new = blob(&mut store, "changed\n");
        let rebuilt = replace(
            &mut store,
            &root,
            &path("sources/repo/01-a/README.md"),
            Some((Mode::Blob, new.clone())),
            "edit README.md",
        )
        .unwrap();
        assert_eq!(rebuilt.commits.len(), 1);
        let minted = &rebuilt.commits[0];
        let info = store.read_commit(minted).unwrap();
        assert_eq!(info.parents, vec![layer]);
        assert_eq!(info.message, b"edit README.md\n");
        assert_eq!(
            resolve(&store, &rebuilt.tree, &path("sources/repo/01-a")).unwrap(),
            Some((Mode::Commit, minted.clone()))
        );
        assert_eq!(
            resolve(&store, &rebuilt.tree, &path("sources/repo/01-a/README.md")).unwrap(),
            Some((Mode::Blob, new))
        );
        // Everything off the path is untouched.
        assert_eq!(
            resolve(&store, &rebuilt.tree, &path("notes.md")).unwrap(),
            resolve(&store, &root, &path("notes.md")).unwrap()
        );
    }

    #[test]
    fn writing_back_what_was_there_changes_nothing() {
        let mut store = MemoryStore::new();
        let (root, _) = session(&mut store);
        let same = resolve(&store, &root, &path("sources/repo/01-a/README.md")).unwrap();
        let rebuilt = replace(
            &mut store,
            &root,
            &path("sources/repo/01-a/README.md"),
            same,
            "nothing",
        )
        .unwrap();
        assert_eq!(rebuilt.tree, root);
        assert!(rebuilt.commits.is_empty());
    }

    #[test]
    fn creating_and_deleting() {
        let mut store = MemoryStore::new();
        let (root, _) = session(&mut store);
        let new = blob(&mut store, "new\n");
        let created = replace(
            &mut store,
            &root,
            &path("docs/deep/new.md"),
            Some((Mode::Blob, new.clone())),
            "create",
        )
        .unwrap();
        assert_eq!(
            resolve(&store, &created.tree, &path("docs/deep/new.md")).unwrap(),
            Some((Mode::Blob, new))
        );
        // Deleting it again drops the directories it alone kept alive.
        let deleted = replace(
            &mut store,
            &created.tree,
            &path("docs/deep/new.md"),
            None,
            "rm",
        )
        .unwrap();
        assert_eq!(deleted.tree, root);
        // Deleting a whole source tree is one gitlink gone, no commit made.
        let gone = replace(&mut store, &root, &path("sources/repo/01-a"), None, "rm").unwrap();
        assert!(gone.commits.is_empty());
        assert_eq!(resolve(&store, &gone.tree, &path("sources")).unwrap(), None);
    }

    #[test]
    fn a_root_through_a_file_is_refused() {
        let mut store = MemoryStore::new();
        let (root, _) = session(&mut store);
        let new = blob(&mut store, "x\n");
        let error = replace(
            &mut store,
            &root,
            &path("notes.md/inner"),
            Some((Mode::Blob, new)),
            "x",
        )
        .err()
        .unwrap();
        assert!(error.contains("notes.md"), "{error}");
    }
}
