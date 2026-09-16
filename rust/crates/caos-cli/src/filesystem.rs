//! Read-only browsing of conversation files and adjacent source commits.
use super::*;
use conversation_protocol::v3::tree::{Snapshot, TreeEntry};
use conversation_protocol::v3::Mode;

const MAX_BLOB: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct SnapshotInfo {
    pub head: String,
    pub tree: String,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub directory: bool,
    pub commit: Option<String>,
    pub change: char,
}

pub struct Preview {
    pub title: String,
    pub text: String,
}

pub fn snapshot(t: &GitTransport, id: &str) -> Result<SnapshotInfo, String> {
    let store = open_store(t)?;
    let (_, head) =
        fetch_validated_head(t, &store, id)?.ok_or("conversation has no snapshot yet")?;
    let commit = store.read_commit(&head)?;
    Ok(SnapshotInfo {
        head: head.to_string(),
        tree: commit.tree.to_string(),
    })
}

fn lookup(store: &dyn ObjectStore, root: &Oid, path: &str) -> Result<Option<TreeEntry>, String> {
    let mut entry = TreeEntry {
        name: String::new(),
        mode: Mode::Tree,
        oid: root.clone(),
    };
    if path.is_empty() {
        return Ok(Some(entry));
    }
    conversation_protocol::v3::paths::validate_tree_path(path)?;
    for part in path.split('/') {
        if entry.mode == Mode::Commit {
            entry.oid = store.read_commit(&entry.oid)?.tree;
            entry.mode = Mode::Tree;
        }
        if entry.mode != Mode::Tree {
            return Ok(None);
        }
        let Some(next) = Snapshot::new(store, entry.oid)
            .list("")?
            .into_iter()
            .find(|e| e.name == part)
        else {
            return Ok(None);
        };
        entry = next;
    }
    Ok(Some(entry))
}

fn children(store: &dyn ObjectStore, root: &Oid, path: &str) -> Result<Vec<TreeEntry>, String> {
    let Some(mut entry) = lookup(store, root, path)? else {
        return Ok(Vec::new());
    };
    if entry.mode == Mode::Commit {
        entry.oid = store.read_commit(&entry.oid)?.tree;
        entry.mode = Mode::Tree;
    }
    if entry.mode != Mode::Tree {
        return Ok(Vec::new());
    }
    Snapshot::new(store, entry.oid).list("")
}

// Feature boundaries are sibling gitlinks in descending filename order.
// The final entry has no preceding boundary and is browsed as content.
fn pair(
    store: &GitStore,
    root: &Oid,
    folder: &str,
    name: Option<&str>,
) -> Result<Option<(TreeEntry, TreeEntry)>, String> {
    let mut commits: Vec<_> = children(store, root, folder)?
        .into_iter()
        .filter(|e| e.mode == Mode::Commit)
        .collect();
    commits.sort_by(|a, b| b.name.cmp(&a.name));
    let index = match name {
        Some(name) => match commits.iter().position(|e| e.name == name) {
            Some(index) => index,
            None => return Ok(None),
        },
        None => 0,
    };
    Ok(commits
        .get(index)
        .zip(commits.get(index + 1))
        .map(|(after, before)| (before.clone(), after.clone())))
}

struct Comparison {
    before: Oid,
    after: Oid,
    path: String,
    title: String,
}

fn comparison(store: &GitStore, root: &Oid, path: &str) -> Result<Option<Comparison>, String> {
    let mut folder = String::new();
    for part in path.split('/').filter(|s| !s.is_empty()) {
        let entry = children(store, root, &folder)?
            .into_iter()
            .find(|e| e.name == part);
        if entry.is_some_and(|e| e.mode == Mode::Commit) {
            return pair(store, root, &folder, Some(part))?
                .map(|(before, after)| {
                    let prefix = if folder.is_empty() {
                        part.to_string()
                    } else {
                        format!("{folder}/{part}")
                    };
                    Ok(Comparison {
                        title: format!(
                            "{}/{} {} -> {} {}",
                            folder,
                            before.name,
                            &before.oid.as_str()[..7],
                            after.name,
                            &after.oid.as_str()[..7]
                        ),
                        before: store.read_commit(&before.oid)?.tree,
                        after: store.read_commit(&after.oid)?.tree,
                        path: path
                            .strip_prefix(&prefix)
                            .unwrap()
                            .trim_start_matches('/')
                            .into(),
                    })
                })
                .transpose();
        }
        folder = if folder.is_empty() {
            part.into()
        } else {
            format!("{folder}/{part}")
        };
    }
    Ok(None)
}

pub fn list(t: &GitTransport, tree: &str, path: &str) -> Result<Vec<Entry>, String> {
    let store = open_store(t)?;
    let root = oid(tree, "snapshot tree")?;
    let comparison = comparison(&store, &root, path)?;
    let comparing = comparison.is_some();
    let (before, after) = match comparison {
        Some(c) => (
            children(&store, &c.before, &c.path)?,
            children(&store, &c.after, &c.path)?,
        ),
        None => (Vec::new(), children(&store, &root, path)?),
    };
    let names: std::collections::BTreeSet<_> =
        before.iter().chain(&after).map(|e| &e.name).collect();
    let mut out = Vec::new();
    for name in names {
        let a = before.iter().find(|e| &e.name == name);
        let b = after.iter().find(|e| &e.name == name);
        let e = b.or(a).unwrap();
        out.push(Entry {
            path: if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            },
            name: name.clone(),
            directory: matches!(e.mode, Mode::Tree | Mode::Commit),
            commit: (e.mode == Mode::Commit).then(|| e.oid.to_string()),
            change: if !comparing {
                ' '
            } else {
                match (a, b) {
                    (None, Some(_)) => '+',
                    (Some(_), None) => '-',
                    (Some(a), Some(b)) if a.mode != b.mode || a.oid != b.oid => '~',
                    _ => ' ',
                }
            },
        });
    }
    out.sort_by(|a, b| b.directory.cmp(&a.directory).then(b.name.cmp(&a.name)));
    Ok(out)
}

fn blob(
    t: &GitTransport,
    store: &GitStore,
    entry: Option<&TreeEntry>,
) -> Result<Option<Vec<u8>>, String> {
    let Some(entry) = entry else {
        return Ok(Some(Vec::new()));
    };
    if matches!(entry.mode, Mode::Tree | Mode::Commit) {
        return Ok(None);
    }
    store.ensure_local(&entry.oid)?;
    let size = t
        .git_capture(&["cat-file", "-s", entry.oid.as_str()], None)?
        .trim()
        .parse::<usize>()
        .map_err(|e| e.to_string())?;
    if size > MAX_BLOB {
        return Ok(None);
    }
    let bytes = store.read_blob(&entry.oid)?;
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return Ok(None);
    }
    Ok(Some(bytes))
}

pub fn preview(t: &GitTransport, tree: &str, path: &str) -> Result<Preview, String> {
    let store = open_store(t)?;
    let root = oid(tree, "snapshot tree")?;
    let mut compare = comparison(&store, &root, path)?;
    if compare.is_none() {
        if let Some((before, after)) = pair(&store, &root, path, None)? {
            compare = Some(Comparison {
                title: format!(
                    "{path}: {} {} -> {} {}",
                    before.name,
                    &before.oid.as_str()[..7],
                    after.name,
                    &after.oid.as_str()[..7]
                ),
                before: store.read_commit(&before.oid)?.tree,
                after: store.read_commit(&after.oid)?.tree,
                path: String::new(),
            });
        }
    }
    if let Some(c) = compare {
        let text = t.git_capture(
            &[
                "--literal-pathspecs",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--color=never",
                c.before.as_str(),
                c.after.as_str(),
                "--",
                if c.path.is_empty() { "." } else { &c.path },
            ],
            None,
        )?;
        return Ok(Preview {
            title: c.title,
            text: if text.is_empty() {
                "No changes.".into()
            } else {
                text
            },
        });
    }
    let entry = lookup(&store, &root, path)?.ok_or("file no longer exists")?;
    let text = if matches!(entry.mode, Mode::Tree | Mode::Commit) {
        children(&store, &root, path)?
            .iter()
            .map(|e| {
                format!(
                    "{}{}",
                    e.name,
                    if matches!(e.mode, Mode::Tree | Mode::Commit) {
                        "/"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        match blob(t, &store, Some(&entry))? {
            Some(bytes) => {
                if entry.mode == Mode::Link {
                    format!(
                        "Symlink target (not followed): {}",
                        String::from_utf8_lossy(&bytes)
                    )
                } else {
                    String::from_utf8(bytes).map_err(|e| e.to_string())?
                }
            }
            None => format!("Binary or file larger than 256 KiB. Object: {}", entry.oid),
        }
    };
    Ok(Preview {
        title: path.into(),
        text,
    })
}
