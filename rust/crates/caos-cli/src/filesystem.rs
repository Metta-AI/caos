//! Snapshot browsing and explicitly applied shell proposals.
use super::*;
use conversation_protocol::v3::tree::{diff, Snapshot, TreeEntry};
use conversation_protocol::v3::Mode;

const MAX_BLOB: usize = 256 * 1024;
const MAX_SEARCH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SnapshotInfo {
    pub head: String,
    pub tree: String,
    pub parent: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub directory: bool,
    pub commit: Option<String>,
    pub change: char,
    pub detail: String,
    pub line: usize,
}

pub fn snapshot(
    t: &GitTransport,
    id: &str,
    revision: Option<&str>,
) -> Result<SnapshotInfo, String> {
    let store = open_store(t)?;
    let (_, current) =
        fetch_validated_head(t, &store, id)?.ok_or("conversation has no snapshot yet")?;
    let head = revision
        .map(|r| oid(r, "conversation snapshot"))
        .transpose()?
        .unwrap_or(current.clone());
    if !spine_contains(&store, current, &head)? {
        return Err("snapshot is not in this conversation's history".into());
    }
    let commit = store.read_commit(&head)?;
    Ok(SnapshotInfo {
        head: head.to_string(),
        tree: commit.tree.to_string(),
        parent: commit
            .parents
            .first()
            .filter(|p| p.as_str() != G3)
            .map(ToString::to_string),
    })
}

pub fn history(t: &GitTransport, head: &str) -> Result<Vec<SnapshotInfo>, String> {
    let store = open_store(t)?;
    let mut cursor = oid(head, "snapshot")?;
    let mut out = Vec::new();
    for _ in 0..200 {
        if cursor.as_str() == G3 {
            break;
        }
        let commit = store.read_commit(&cursor)?;
        let parent = commit.parents.first().filter(|p| p.as_str() != G3).cloned();
        out.push(SnapshotInfo {
            head: cursor.to_string(),
            tree: commit.tree.to_string(),
            parent: parent.as_ref().map(ToString::to_string),
        });
        let Some(next) = parent else { break };
        cursor = next;
    }
    Ok(out)
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

pub fn list(
    t: &GitTransport,
    tree: &str,
    baseline: &str,
    path: &str,
    changes: bool,
) -> Result<Vec<Entry>, String> {
    let store = open_store(t)?;
    let before = children(&store, &oid(baseline, "baseline tree")?, path)?;
    let after = children(&store, &oid(tree, "snapshot tree")?, path)?;
    let names: std::collections::BTreeSet<_> =
        before.iter().chain(&after).map(|e| &e.name).collect();
    let mut out = Vec::new();
    for name in names {
        let a = before.iter().find(|e| &e.name == name);
        let b = after.iter().find(|e| &e.name == name);
        let change = match (a, b) {
            (None, Some(_)) => '+',
            (Some(_), None) => '-',
            (Some(a), Some(b)) if a.mode != b.mode || a.oid != b.oid => '~',
            _ => ' ',
        };
        if (changes && change == ' ') || (!changes && b.is_none()) {
            continue;
        }
        let e = b.or(a).expect("name belongs to one tree");
        out.push(Entry {
            path: if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            },
            name: name.clone(),
            directory: matches!(e.mode, Mode::Tree | Mode::Commit),
            commit: (e.mode == Mode::Commit).then(|| e.oid.to_string()),
            change,
            detail: if e.mode == Mode::Link {
                "symlink".into()
            } else {
                String::new()
            },
            line: 0,
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

pub fn read(
    t: &GitTransport,
    tree: &str,
    baseline: &str,
    path: &str,
    changes: bool,
) -> Result<String, String> {
    let mut store = open_store(t)?;
    let before = lookup(&store, &oid(baseline, "baseline tree")?, path)?;
    let after = lookup(&store, &oid(tree, "snapshot tree")?, path)?;
    if changes {
        if blob(t, &store, before.as_ref())?.is_none() || blob(t, &store, after.as_ref())?.is_none()
        {
            return Ok(
                "Binary, directory, or file larger than 256 KiB; text diff unavailable.".into(),
            );
        }
        let empty = store.write_blob(b"")?;
        let a = before.as_ref().map(|e| &e.oid).unwrap_or(&empty);
        let b = after.as_ref().map(|e| &e.oid).unwrap_or(&empty);
        let patch = t.git_capture(
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--color=never",
                a.as_str(),
                b.as_str(),
            ],
            None,
        )?;
        return Ok(if patch.is_empty() {
            "No text changes (the entry mode or commit may differ).".into()
        } else {
            patch
        });
    }
    let entry = after
        .as_ref()
        .or(before.as_ref())
        .ok_or("file no longer exists")?;
    match blob(t, &store, Some(entry))? {
        Some(bytes) => Ok(if entry.mode == Mode::Link {
            format!(
                "Symlink target (not followed): {}",
                String::from_utf8_lossy(&bytes)
            )
        } else {
            String::from_utf8(bytes).map_err(|e| e.to_string())?
        }),
        None => Ok(format!(
            "Binary or file larger than 256 KiB. Object: {}",
            entry.oid
        )),
    }
}

#[derive(Clone, Debug)]
pub struct SearchResults {
    pub entries: Vec<Entry>,
    pub limited: bool,
}

pub fn search(
    t: &GitTransport,
    tree: &str,
    baseline: &str,
    query: &str,
    changes: bool,
) -> Result<SearchResults, String> {
    if query.trim().is_empty() {
        return Err("enter text to search for".into());
    }
    let store = open_store(t)?;
    let root = oid(tree, "snapshot tree")?;
    let mut pending = vec![(String::new(), 0usize)];
    let mut found = Vec::new();
    let mut scanned = 0;
    let mut bytes = 0;
    let query = query.to_lowercase();
    let mut limited = false;
    while let Some((path, depth)) = pending.pop() {
        if depth > 64 {
            limited = true;
            continue;
        }
        for mut row in list(t, tree, baseline, &path, changes)? {
            scanned += 1;
            if scanned > 5000 || bytes > MAX_SEARCH_BYTES || found.len() >= 200 {
                limited = true;
                break;
            }
            if row.directory {
                pending.push((row.path, depth + 1));
                continue;
            }
            let entry = lookup(&store, &root, &row.path)?;
            let Some(entry) = entry else { continue };
            let Some(text) = blob(t, &store, Some(&entry))? else {
                limited = true;
                continue;
            };
            bytes += text.len();
            for (line, text) in String::from_utf8_lossy(&text).lines().enumerate() {
                if text.to_lowercase().contains(&query) {
                    row.line = line;
                    row.detail = format!(
                        "{}: {}",
                        line + 1,
                        text.chars().take(120).collect::<String>()
                    );
                    row.name = row.path.clone();
                    found.push(row.clone());
                    if found.len() >= 200 {
                        limited = true;
                        break;
                    }
                }
            }
        }
        if scanned > 5000 || bytes > MAX_SEARCH_BYTES || found.len() >= 200 {
            break;
        }
    }
    Ok(SearchResults {
        entries: found,
        limited,
    })
}

#[derive(Clone, Debug)]
pub struct ShellResult {
    pub tree: String,
    pub output: String,
}

pub fn shell(
    t: &GitTransport,
    options: &TurnOptions,
    tree: &str,
    command: &str,
) -> Result<ShellResult, String> {
    // Extract the existing shell worker from the selected harness; never invent
    // another shell image or run repository commands on the host.
    let step = resolve_image_arg(t, options.llm_step.as_deref(), LLM_STEP_ARG, &[])?;
    let request = prepare_client_request_with_store(t, &step, &[], &[])?;
    let store = open_store(t)?;
    let request = Snapshot::new(&store, oid(&request, "harness request")?);
    let image = request
        .entry("bash-image")?
        .ok_or("this harness has no shell worker")?
        .oid;
    let quoted = format!("'{}'", command.replace('\'', "'\\''"));
    let script = format!("if [ -d .caos ]; then chmod -R a-w .caos; fi\ntimeout 30 sh -c {quoted}");
    let (kind, result) = run_client_request_with_store(
        t,
        image.as_str(),
        &[
            format!("--tree:hash={tree}"),
            format!("--cmd={script}"),
            "--paths=.".into(),
            format!("--salt={}", caos::fresh_entropy()?),
        ],
        &[],
    )?;
    if kind != "tree" {
        return Err(format!("shell returned {kind}, expected a tree"));
    }
    let result = Snapshot::new(&store, oid(&result, "shell result")?);
    let proposed = result
        .entry("tree")?
        .ok_or("shell returned no filesystem")?
        .oid;
    let changes = diff(&store, Some(&oid(tree, "input tree")?), &proposed)?;
    reject_metadata_changes(&changes)?;
    let output = |name| -> Result<String, String> {
        Ok(String::from_utf8_lossy(&result.read(name)?.unwrap_or_default()).into_owned())
    };
    Ok(ShellResult {
        tree: proposed.to_string(),
        output: format!(
            "$ {command}\n{}{}\nExit {}",
            output("stdout")?,
            output("stderr")?,
            output("exit")?.trim()
        ),
    })
}

fn reject_metadata_changes(
    changes: &[conversation_protocol::v3::tree::Change],
) -> Result<(), String> {
    if changes
        .iter()
        .any(|c| c.path == ".caos" || c.path.starts_with(".caos/"))
    {
        return Err(
            ".caos is read-only protocol metadata; none of the shell edits were accepted".into(),
        );
    }
    Ok(())
}

pub fn apply_shell(
    t: &GitTransport,
    id: &str,
    base: &str,
    proposal: &str,
) -> Result<String, String> {
    let store = open_store(t)?;
    let base = oid(base, "shell input snapshot")?;
    let proposal = oid(proposal, "shell proposal tree")?;
    let changes = diff(&store, Some(&store.read_commit(&base)?.tree), &proposal)?;
    reject_metadata_changes(&changes)?;
    t.ensure_pushed(proposal.as_str())?;
    append_transition(
        t,
        id,
        &refs::head_ref(id)?,
        "applying shell edits",
        |store, head| {
            if !spine_contains(store, head.clone(), &base)? {
                return Err("shell input is not in this conversation's history".into());
            }
            let tree = store.read_commit(head)?.tree;
            let (files, conflicts) =
                conversation_protocol::v3::reconcile::plan_file_changes(store, &changes, &tree)?;
            if !conflicts.is_empty() {
                return Err(format!("Nothing applied: concurrent edits conflict at {}. Shell proposal {proposal} is retained.",conflicts.join(", ")));
            }
            if files.is_empty() {
                return Ok(Step::Done(head.to_string()));
            }
            for (_, value) in &files {
                if let Some((Mode::Commit, bytes)) = value {
                    ensure_code_commit(t, store, &Oid::parse_line(bytes, "source tree")?)?;
                }
            }
            Ok(Step::Mint(Transition::FilesApply { files }))
        },
    )
}
