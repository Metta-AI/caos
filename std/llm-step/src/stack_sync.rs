//! Bring a stack up to date with its remote, the way `gt sync` does
//! (design/stack-publish.md, "stack-sync"): the trunk's head becomes `00-base`,
//! a branch GitHub rewrote is adopted under the layer's local edits, a branch
//! someone else pushed to is merged in, and each layer is restacked onto the
//! one below. Nothing is pushed: the merges are minted in the conversation's
//! store and the stack's gitlinks move in one append, so `stack-publish` can
//! fast-forward afterwards.
use std::process::Command;

use super::*;
use conversation_protocol::v3::reconcile::MergeOutcome;
use conversation_protocol::v3::source_trees::validate_branch;
use conversation_protocol::v3::tree::Signature;
use conversation_protocol::v3::GitStore;

use crate::publish_source::bool_or_string;
use crate::stack_publish::{github_repo, infer_trunk, provenance, read_stack, Github};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    stack: String,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    trunk: Option<String>,
    #[serde(default, rename = "branch-prefix", alias = "branch_prefix")]
    branch_prefix: Option<String>,
    #[serde(
        default,
        rename = "remove-merged",
        alias = "remove_merged",
        deserialize_with = "bool_or_string"
    )]
    remove_merged: bool,
    #[serde(
        default,
        rename = "dry-run",
        alias = "dry_run",
        deserialize_with = "bool_or_string"
    )]
    dry_run: bool,
}

fn blank(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

fn parameters(call: &Call) -> Result<Parameters, String> {
    let mut p: Parameters = serde_json::from_value(call.input.clone())
        .map_err(|error| format!("invalid stack-sync arguments: {error}"))?;
    p.stack = p.stack.trim().trim_matches('/').to_string();
    if p.stack.is_empty() {
        return Err("stack must name a directory, such as sources/repo".into());
    }
    paths::validate_source_tree_name(&p.stack)?;
    p.repository = blank(p.repository);
    if let Some(repository) = &p.repository {
        git_locator::import::remote(repository)?;
    }
    p.trunk = blank(p.trunk);
    if let Some(trunk) = &p.trunk {
        validate_branch(trunk)?;
    }
    p.branch_prefix = blank(p.branch_prefix);
    Ok(p)
}

fn short(oid: &Oid) -> &str {
    &oid.as_str()[..12]
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// The author of the merge commits this operation mints.
fn signature() -> Signature {
    Signature {
        name: "caos".into(),
        email: "caos@caos".into(),
        time: now_secs(),
        offset: "+0000".into(),
    }
}

/// The nearest first-parent ancestor of `tip`, `tip` itself first, whose tree
/// is `tree`, within `limit` commits. A remote head with the tree of a commit
/// we hold is a rewrite of that commit: GitHub rebasing the branches above a
/// merged stacked PR produces exactly one.
pub(super) fn rewrite_of(
    store: &GitStore,
    tip: &Oid,
    tree: &Oid,
    limit: usize,
) -> Result<Option<Oid>, String> {
    let mut current = tip.clone();
    for _ in 0..limit {
        let info = store.read_commit(&current).map_err(String::from)?;
        if &info.tree == tree {
            return Ok(Some(current));
        }
        match info.parents.first() {
            Some(parent) => current = parent.clone(),
            None => return Ok(None),
        }
    }
    Ok(None)
}

pub(super) enum Merged {
    Clean(Oid),
    Conflict(Vec<String>),
}

/// Three-way merge `theirs` into `ours` from `base`, minting the merge commit
/// with parents [ours, theirs] when it is clean.
pub(super) fn merge_into(
    store: &mut GitStore,
    base: &Oid,
    ours: &Oid,
    theirs: &Oid,
    message: &str,
) -> Result<Merged, String> {
    for commit in [base, ours, theirs] {
        store.ensure_local(commit)?;
    }
    match store.merge(base, ours, theirs)? {
        MergeOutcome::Merged { tree } => {
            let commit = store.commit(
                &tree,
                &[ours.clone(), theirs.clone()],
                message,
                &signature(),
            )?;
            Ok(Merged::Clean(commit))
        }
        MergeOutcome::Conflict { paths } => Ok(Merged::Conflict(paths)),
    }
}

/// Fetch a remote commit into the server's store, as `import_source` does, so
/// the conversation's store can reach it.
fn import_commit(repository: &str, commit: &Oid, token_file: Option<&str>) -> Result<(), String> {
    let mut command = Command::new("caos");
    command.args(["import-git", repository, commit.as_str()]);
    if let Some(path) = token_file {
        command.arg(format!("--github-token-file={path}"));
    }
    let output = command
        .output()
        .map_err(|error| format!("starting import-git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "importing {commit} from the remote failed; check the repository and the token"
        ));
    }
    Ok(())
}

fn observe(repository: &str, branch: &str, token: Option<&str>) -> Result<Option<Oid>, String> {
    git_locator::publish::read_branch(repository, branch, token)
        .and_then(|head| head.map(|h| Oid::parse(&h, "remote head")).transpose())
}

/// The number of a merged pull request whose head was `branch`, if any.
fn merged_pr(github: &Github, branch: &str) -> Result<Option<u64>, String> {
    let found = github.expect(
        "GET",
        &github.repo_path(&format!(
            "/pulls?head={}:{branch}&state=closed",
            github.owner
        )),
        None,
        &[200],
    )?;
    Ok(found
        .as_array()
        .into_iter()
        .flatten()
        .find(|pr| !pr["merged_at"].is_null())
        .and_then(|pr| pr["number"].as_u64()))
}

fn merge_hint(path: &str, theirs: &Oid) -> String {
    format!(
        "Run run_tool(path=\"caos-std/merge\", arguments={{\"source-tree\": \"{path}\", \"theirs\": \"{theirs}\"}}) to get a conflict ledger, resolve it, and run stack-sync again."
    )
}

/// Complete the call, moving the stack's gitlinks in the same append.
fn complete(
    state: &mut progress::State,
    site: &CallSite<'_>,
    text: &str,
    is_error: bool,
    files: Vec<(String, Option<(Mode, Vec<u8>)>)>,
) -> Result<(), String> {
    for _ in 0..32 {
        state.reload()?;
        if state
            .conversation()?
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(());
        }
        let block = result_block(&site.call.id, text, is_error);
        let stub = site.stub(None);
        let mut record = completed_record(
            &stub,
            ToolResult::Complete {
                observation: observation_path(&stub),
                proposal: None,
            },
            None,
        );
        record.files = files.iter().map(|(name, _)| name.clone()).collect();
        record.files.sort();
        let expected = state.head().clone();
        if matches!(
            state.try_append_at(&expected, tool_complete_transition(record, &block, files.clone())?)?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(());
        }
    }
    Err("conversation kept moving while completing stack-sync".into())
}

pub(super) fn execute(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let p = match parameters(site.call) {
        Ok(p) => p,
        Err(error) => return site.fail(state, &error),
    };
    let dir = p.stack.clone();
    let read = {
        let view = state.conversation()?;
        read_stack(&view, &dir).and_then(|stack| {
            let prov = provenance(&view, &dir)?;
            let repository = p
                .repository
                .clone()
                .or_else(|| prov.repository.clone())
                .ok_or_else(|| {
                    format!("no repository: {dir}/00-base.source.json names none, so pass repository")
                })?;
            git_locator::import::remote(&repository)?;
            let trunk = p
                .trunk
                .clone()
                .or_else(|| infer_trunk(&prov))
                .ok_or_else(|| {
                    format!("no trunk: {dir}/00-base.source.json names no branch, so pass trunk")
                })?;
            validate_branch(&trunk)?;
            let source_json = view.snapshot().read(&format!("{dir}/00-base.source.json"))?;
            Ok((stack, repository, trunk, source_json))
        })
    };
    let (stack, repository, trunk, source_json) = match read {
        Ok(read) => read,
        Err(error) => {
            return site.fail(state, &format!("stack-sync refused; nothing changed: {error}"))
        }
    };
    let token_file = import_source::token_file(&repository);
    let token = token_file
        .map(fs::read_to_string)
        .transpose()
        .map_err(|_| "reading GitHub token")?;
    let token = token.as_deref().map(str::trim_end);
    let prefix = p.branch_prefix.clone().unwrap_or_default();
    let github = github_repo(&repository).map(|(owner, repo)| Github {
        owner,
        repo,
        token: token.map(str::to_string),
    });

    let mut lines = vec![format!("repository {repository}  trunk {trunk}")];
    let mut files: Vec<(String, Option<(Mode, Vec<u8>)>)> = Vec::new();
    let mut minted: Vec<Oid> = Vec::new();
    let mut stopped: Option<String> = None;

    // The trunk first: its head becomes 00-base.
    let trunk_head = match observe(&repository, &trunk, token) {
        Ok(Some(head)) => head,
        Ok(None) => {
            return site.fail(
                state,
                &format!("stack-sync refused; nothing changed: the trunk {trunk} does not exist on {repository}"),
            )
        }
        Err(error) => {
            return site.fail(
                state,
                &format!("stack-sync refused; nothing changed: {error} for the trunk {trunk}"),
            )
        }
    };
    let mut below = stack.base.clone();
    let mut below_name = "00-base".to_string();
    if trunk_head == stack.base {
        lines.push(format!(
            "00-base  {trunk}  {}  at the trunk's head",
            short(&stack.base)
        ));
    } else {
        if let Err(error) = import_commit(&repository, &trunk_head, token_file) {
            return site.fail(state, &format!("stack-sync refused; nothing changed: {error}"));
        }
        state.store().ensure_local(&trunk_head)?;
        files.push((
            format!("{dir}/00-base"),
            Some((Mode::Commit, trunk_head.encode_line())),
        ));
        if let Some(bytes) = &source_json {
            if let Ok(mut value) = serde_json::from_slice::<Value>(bytes) {
                value["commit"] = json!(trunk_head.as_str());
                value["observed_at"] = json!(now_secs());
                files.push((
                    format!("{dir}/00-base.source.json"),
                    Some((Mode::Blob, canonical_payload_bytes(&value)?)),
                ));
            }
        }
        lines.push(format!(
            "00-base  {trunk}  {} -> {}  the trunk moved",
            short(&stack.base),
            short(&trunk_head)
        ));
        below = trunk_head.clone();
    }

    // Each layer, bottom to top.
    for layer in &stack.layers {
        let path = format!("{dir}/{}", layer.name);
        let mut tip = layer.commit.clone();
        let mut notes: Vec<String> = Vec::new();
        let mut removed = false;
        if layer.description.is_some() {
            let branch = format!("{prefix}{}", layer.short);
            if let Some(github) = &github {
                match merged_pr(github, &branch) {
                    Ok(Some(number)) => {
                        notes.push(format!("PR #{number} is merged"));
                        if p.remove_merged {
                            files.push((path.clone(), None));
                            files.push((format!("{path}.pr.md"), None));
                            notes.push("removed with its .pr.md".into());
                            removed = true;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => notes.push(format!("could not check its PR: {error}")),
                }
            }
            if !removed {
                match observe(&repository, &branch, token) {
                    Err(error) => stopped = Some(format!("{error} for branch {branch}")),
                    Ok(None) => notes.push(format!("{branch}: not published")),
                    Ok(Some(remote)) if remote == tip => notes.push(format!("{branch}: in sync")),
                    Ok(Some(remote)) => {
                        if state.store().is_ancestor(&remote, &tip)? {
                            notes.push(format!("{branch}: local edits ahead of the remote"));
                        } else {
                            match import_commit(&repository, &remote, token_file) {
                                Err(error) => stopped = Some(error),
                                Ok(()) => {
                                    state.store().ensure_local(&remote)?;
                                    let remote_tree = state
                                        .store()
                                        .read_commit(&remote)
                                        .map_err(String::from)?
                                        .tree;
                                    match rewrite_of(state.store(), &tip, &remote_tree, 500)? {
                                        Some(same) if same == tip => {
                                            tip = remote.clone();
                                            notes.push(format!(
                                                "{branch}: rewritten remotely with the same content, adopted {}",
                                                short(&remote)
                                            ));
                                        }
                                        Some(same) => match merge_into(
                                            state.store_mut(),
                                            &same,
                                            &tip,
                                            &remote,
                                            &format!("stack-sync: adopt the rewritten {branch}\n"),
                                        )? {
                                            Merged::Clean(commit) => {
                                                tip = commit;
                                                notes.push(format!(
                                                    "{branch}: rewritten remotely, adopted {} under the local edits",
                                                    short(&remote)
                                                ));
                                            }
                                            Merged::Conflict(paths) => {
                                                stopped = Some(format!(
                                                    "{branch}: adopting the rewritten remote {remote} conflicts in {}. {}",
                                                    paths.join(", "),
                                                    merge_hint(&path, &remote)
                                                ))
                                            }
                                        },
                                        None => match state.store().merge_base(&tip, &remote)? {
                                            None => {
                                                stopped = Some(format!(
                                                    "{branch}: the remote head {remote} shares no history with the layer. {}",
                                                    merge_hint(&path, &remote)
                                                ))
                                            }
                                            Some(base) => match merge_into(
                                                state.store_mut(),
                                                &base,
                                                &tip,
                                                &remote,
                                                &format!("stack-sync: merge the remote {branch}\n"),
                                            )? {
                                                Merged::Clean(commit) => {
                                                    tip = commit;
                                                    notes.push(format!(
                                                        "{branch}: merged the remote's changes ({})",
                                                        short(&remote)
                                                    ));
                                                }
                                                Merged::Conflict(paths) => {
                                                    stopped = Some(format!(
                                                        "{branch}: merging the remote {remote} conflicts in {}. {}",
                                                        paths.join(", "),
                                                        merge_hint(&path, &remote)
                                                    ))
                                                }
                                            },
                                        },
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if stopped.is_none() && !removed && !state.store().is_ancestor(&below, &tip)? {
            match state.store().merge_base(&below, &tip)? {
                None => {
                    stopped = Some(format!(
                        "{}: shares no history with {below_name}. {}",
                        layer.name,
                        merge_hint(&path, &below)
                    ))
                }
                Some(base) => match merge_into(
                    state.store_mut(),
                    &base,
                    &tip,
                    &below,
                    &format!("stack-sync: restack {} onto {below_name}\n", layer.name),
                )? {
                    Merged::Clean(commit) => {
                        tip = commit;
                        notes.push(format!("restacked onto {below_name}"));
                    }
                    Merged::Conflict(paths) => {
                        stopped = Some(format!(
                            "restacking {} onto {below_name} conflicts in {}. {}",
                            layer.name,
                            paths.join(", "),
                            merge_hint(&path, &below)
                        ))
                    }
                },
            }
        }
        if stopped.is_none() && !removed {
            if tip == layer.commit {
                notes.push("unchanged".into());
            } else {
                files.push((path.clone(), Some((Mode::Commit, tip.encode_line()))));
                minted.push(tip.clone());
            }
            below = tip.clone();
            below_name = layer.name.clone();
        }
        lines.push(format!("{}  {}", layer.name, notes.join(", ")));
        if stopped.is_some() {
            break;
        }
    }

    let mut text = String::new();
    if p.dry_run {
        text.push_str("dry run: nothing changed.\n");
    }
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    match &stopped {
        Some(problem) => {
            text.push_str("FAILED: ");
            text.push_str(problem);
            text.push_str("\nThe layers below it are synced; the ones above were not touched.\n");
        }
        None if files.is_empty() => text.push_str("Nothing to do: the stack is in sync.\n"),
        None => text.push_str("Next: run caos-std/stack-publish to push.\n"),
    }
    if p.dry_run {
        let block = result_block(&site.call.id, &text, stopped.is_some());
        return site.complete(state, block, None, None, None);
    }
    for commit in &minted {
        state.push_code(commit)?;
    }
    complete(state, site, &text, stopped.is_some(), files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversation_protocol::v3::tree::{CommitInfo, TreeBuilder};

    fn scratch_store(name: &str) -> GitStore {
        let dir = std::env::temp_dir().join(format!("stack-sync-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let status = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&dir)
            .status()
            .unwrap();
        assert!(status.success());
        GitStore::open(&dir, None).unwrap()
    }

    fn tree(store: &mut GitStore, files: &[(&str, &str)]) -> Oid {
        let mut builder = TreeBuilder::from(None);
        for (path, content) in files {
            builder.put(path, Mode::Blob, content.as_bytes().to_vec());
        }
        builder.build(store).unwrap()
    }

    fn commit(store: &mut GitStore, tree: &Oid, parents: &[&Oid], message: &str) -> Oid {
        store
            .write_commit(&CommitInfo {
                tree: tree.clone(),
                parents: parents.iter().map(|p| (*p).clone()).collect(),
                author: signature(),
                committer: signature(),
                extra_headers: Vec::new(),
                message: message.as_bytes().to_vec(),
            })
            .unwrap()
    }

    #[test]
    fn a_rebased_branch_with_the_same_tree_is_a_rewrite_of_the_layer() {
        let mut store = scratch_store("same-tree");
        let f = &[("f", "1\n")];
        let fx = &[("f", "1\n"), ("x", "x\n")];
        let fxy = &[("f", "1\n"), ("x", "x\n"), ("y", "y\n")];
        let (t_f, t_fx, t_fxy) = (tree(&mut store, f), tree(&mut store, fx), tree(&mut store, fxy));
        let main = commit(&mut store, &t_f, &[], "main");
        let layer0 = commit(&mut store, &t_fx, &[&main], "x");
        let layer1 = commit(&mut store, &t_fxy, &[&layer0], "y");
        // GitHub squash-merges layer0 and rebases layer1's branch onto it.
        let squashed = commit(&mut store, &t_fx, &[&main], "x (squashed)");
        let rebased = commit(&mut store, &t_fxy, &[&squashed], "y (rebased)");
        assert_eq!(
            rewrite_of(&store, &layer1, &t_fxy, 500).unwrap(),
            Some(layer1.clone())
        );
        // With a local edit on top, the rewrite is of the commit below the edit,
        // and merging from it keeps the edit on the rebased history.
        let t_fxyz = tree(&mut store, &[("f", "1\n"), ("x", "x\n"), ("y", "y\n"), ("z", "z\n")]);
        let edited = commit(&mut store, &t_fxyz, &[&layer1], "z");
        assert_eq!(
            rewrite_of(&store, &edited, &t_fxy, 500).unwrap(),
            Some(layer1.clone())
        );
        let Merged::Clean(adopted) =
            merge_into(&mut store, &layer1, &edited, &rebased, "adopt\n").unwrap()
        else {
            panic!("a rewrite with the same tree must merge cleanly")
        };
        let info = store.read_commit(&adopted).unwrap();
        assert_eq!(info.tree, t_fxyz);
        assert_eq!(info.parents, vec![edited.clone(), rebased.clone()]);
        assert!(store.is_ancestor(&rebased, &adopted).unwrap());
        assert_eq!(rewrite_of(&store, &edited, &t_f, 1).unwrap(), None);
    }

    #[test]
    fn a_rebase_onto_a_moved_trunk_merges_cleanly_from_the_common_ancestor() {
        let mut store = scratch_store("moved-trunk");
        let t_f = tree(&mut store, &[("f", "1\n")]);
        let t_fx = tree(&mut store, &[("f", "1\n"), ("x", "x\n")]);
        let t_fxy = tree(&mut store, &[("f", "1\n"), ("x", "x\n"), ("y", "y\n")]);
        let t_fw = tree(&mut store, &[("f", "1\n"), ("w", "w\n")]);
        let t_fwx = tree(&mut store, &[("f", "1\n"), ("w", "w\n"), ("x", "x\n")]);
        let t_fwxy = tree(
            &mut store,
            &[("f", "1\n"), ("w", "w\n"), ("x", "x\n"), ("y", "y\n")],
        );
        let main = commit(&mut store, &t_f, &[], "main");
        let layer0 = commit(&mut store, &t_fx, &[&main], "x");
        let layer1 = commit(&mut store, &t_fxy, &[&layer0], "y");
        // Someone else lands w on main, then layer0 squash-merges and layer1 is
        // rebased: its tree now carries w, so it is not a rewrite of layer1.
        let moved = commit(&mut store, &t_fw, &[&main], "w");
        let squashed = commit(&mut store, &t_fwx, &[&moved], "x (squashed)");
        let rebased = commit(&mut store, &t_fwxy, &[&squashed], "y (rebased)");
        assert_eq!(rewrite_of(&store, &layer1, &t_fwxy, 500).unwrap(), None);
        assert!(!store.is_ancestor(&rebased, &layer1).unwrap());
        let base = store.merge_base(&layer1, &rebased).unwrap().unwrap();
        assert_eq!(base, main);
        let Merged::Clean(merged) =
            merge_into(&mut store, &base, &layer1, &rebased, "merge\n").unwrap()
        else {
            panic!("identical changes on both sides must merge cleanly")
        };
        let info = store.read_commit(&merged).unwrap();
        assert_eq!(info.tree, t_fwxy);
        assert!(store.is_ancestor(&rebased, &merged).unwrap());
        assert!(store.is_ancestor(&layer1, &merged).unwrap());
        // And a real conflict is reported as one, with its path.
        let t_conflict = tree(&mut store, &[("f", "2\n"), ("x", "x\n"), ("y", "y\n")]);
        let ours = commit(&mut store, &t_conflict, &[&layer1], "f=2");
        let t_other = tree(&mut store, &[("f", "3\n"), ("x", "x\n"), ("y", "y\n")]);
        let theirs = commit(&mut store, &t_other, &[&layer1], "f=3");
        let Merged::Conflict(paths) =
            merge_into(&mut store, &layer1, &ours, &theirs, "conflict\n").unwrap()
        else {
            panic!("two edits of one line must conflict")
        };
        assert_eq!(paths, vec!["f".to_string()]);
    }

    #[test]
    fn parameters_are_strings_with_defaults() {
        let call = |input: Value| Call {
            id: "a".into(),
            name: "run_tool".into(),
            input,
        };
        let p = parameters(&call(json!({"stack": "sources/repo/"}))).unwrap();
        assert_eq!(p.stack, "sources/repo");
        assert!(!p.remove_merged && !p.dry_run);
        let p = parameters(&call(json!({
            "stack": "sources/repo", "remove-merged": "true", "dry-run": "true", "branch-prefix": "me/"
        })))
        .unwrap();
        assert!(p.remove_merged && p.dry_run);
        assert_eq!(p.branch_prefix.as_deref(), Some("me/"));
        for input in [
            json!({}),
            json!({"stack": "sources/repo", "force": "true"}),
            json!({"stack": "sources/repo", "trunk": "bad..name"}),
        ] {
            assert!(parameters(&call(input.clone())).is_err(), "{input}");
        }
    }
}
