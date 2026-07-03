//! fold-wasm: isolate guest port of the recursive fold worker.
//!
//! It mirrors `worker-fold`'s argument shape exactly, but batches child folds
//! through the isolate host's `run_many` op so recursion can fan out in parallel.

use isolate_common::{entry, get, job, out, put_blob, put_tree, run, run_many, tree};
use isolate_common::{Entry, RunRequest};

const EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

entry!(fold_run);

fn fold_run() -> Result<(), String> {
    let job = job()?;
    let args = tree(&job.args)?;
    let in_entry = find_entry(&args, "in")?.clone();
    let post_entry = find_entry(&args, "post")?.clone();
    let pre_entry = optional_entry(&args, "pre").cloned();

    let post = read_ref(&post_entry, "post")?;
    let pre = pre_entry
        .as_ref()
        .map(|entry| read_ref(entry, "pre"))
        .transpose()?;

    let children = children_for(&in_entry, pre.as_deref())?;
    let fold_image = std_image(&job.std, "fold-wasm")?;

    let post_blob = put_blob(post.as_bytes())?;
    let pre_blob = match pre.as_ref() {
        Some(pre) => Some(put_blob(pre.as_bytes())?),
        None => None,
    };
    let empty_tree = put_tree(&[])?;
    if empty_tree != EMPTY_TREE_HASH {
        return Err(format!(
            "empty tree hash mismatch: expected {EMPTY_TREE_HASH}, got {empty_tree}"
        ));
    }

    let requests = child_requests(
        &children,
        pre.as_deref(),
        pre_blob.as_deref(),
        &post,
        &post_blob,
        &fold_image,
        &empty_tree,
    )?;
    let child_results = run_many(&requests)?;
    if child_results.len() != children.len() {
        return Err(format!(
            "run_many returned {} results for {} children",
            child_results.len(),
            children.len()
        ));
    }

    let mut result_entries = Vec::with_capacity(children.len());
    for (child, result) in children.iter().zip(child_results) {
        let result = result.map_err(|e| format!("folding {}: {e}", child.name))?;
        result_entries.push(Entry {
            name: child.name.clone(),
            kind: result.kind,
            hash: result.hash,
            mode: None,
        });
    }
    let children_tree = put_tree(&result_entries)?;
    let final_args = put_tree(&[
        renamed(&in_entry, "in"),
        Entry::tree("children", children_tree),
    ])?;
    let result = run(&post, &final_args)?;
    out(&result.kind, &result.hash)
}

fn children_for(in_entry: &Entry, pre: Option<&str>) -> Result<Vec<Entry>, String> {
    if let Some(pre) = pre {
        let args = put_tree(&[renamed(in_entry, "in")])?;
        let result = run(pre, &args)?;
        if result.kind != "tree" {
            return Err(format!("pre returned {}, expected tree", result.kind));
        }
        tree(&result.hash)
    } else if in_entry.is_tree() {
        tree(&in_entry.hash)
    } else {
        Ok(Vec::new())
    }
}

fn child_requests(
    children: &[Entry],
    pre: Option<&str>,
    pre_blob: Option<&str>,
    post: &str,
    post_blob: &str,
    fold_image: &str,
    empty_tree: &str,
) -> Result<Vec<RunRequest>, String> {
    let mut requests = Vec::with_capacity(children.len());
    for child in children {
        if pre.is_none() && !child.is_tree() {
            let args = put_tree(&[
                renamed(child, "in"),
                Entry::tree("children", empty_tree.to_string()),
            ])?;
            requests.push(RunRequest {
                image: post.to_string(),
                args,
            });
        } else {
            let mut entries = Vec::with_capacity(3);
            if let Some(pre_blob) = pre_blob {
                entries.push(Entry::blob("pre", pre_blob.to_string()));
            }
            entries.push(Entry::blob("post", post_blob.to_string()));
            entries.push(renamed(child, "in"));
            let args = put_tree(&entries)?;
            requests.push(RunRequest {
                image: fold_image.to_string(),
                args,
            });
        }
    }
    Ok(requests)
}

fn std_image(std: &str, name: &str) -> Result<String, String> {
    if std.is_empty() {
        return Err("job std is empty; cannot resolve fold-wasm".to_string());
    }
    tree(std)?
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.hash)
        .ok_or_else(|| format!("no builtin {name:?} in std tree {std}"))
}

fn find_entry<'a>(entries: &'a [Entry], name: &str) -> Result<&'a Entry, String> {
    optional_entry(entries, name).ok_or_else(|| format!("args missing {name:?}"))
}

fn optional_entry<'a>(entries: &'a [Entry], name: &str) -> Option<&'a Entry> {
    entries.iter().find(|entry| entry.name == name)
}

fn read_ref(entry: &Entry, name: &str) -> Result<String, String> {
    if entry.kind != "blob" {
        return Err(format!("{name} must be a blob, got {}", entry.kind));
    }
    let bytes = get(&entry.hash)?;
    let text = String::from_utf8(bytes).map_err(|e| format!("{name} is not UTF-8: {e}"))?;
    Ok(text.trim().to_string())
}

// Args-tree entries carry plain blob/tree modes, never the source entry's
// mode: the container client's arg builder does the same (a symlink child
// becomes an ordinary blob arg), and matching it byte-for-byte is what lets a
// short-circuited leaf request alias the container fold's identical request.
fn renamed(entry: &Entry, name: &str) -> Entry {
    Entry {
        name: name.to_string(),
        kind: entry.kind.clone(),
        hash: entry.hash.clone(),
        mode: None,
    }
}
