use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Output;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

use caos::chat::WorkspaceDiff;
use gix::bstr::{BStr, BString, ByteSlice};

/// Check out a conversation's head commit in the local working tree.
///
/// This is deliberately client policy rather than part of the chat engine:
/// the TUI chooses when to mutate the checkout and requires confirmation before
/// calling it. Rather than applying the base-to-head diff as unstaged changes,
/// this moves the local HEAD onto the conversation head commit so the checkout
/// exactly matches it.
pub(crate) fn load_conversation_workspace(head: &str, cwd: &Path) -> Result<(), String> {
    let repo = open_repo(cwd)?;
    let mut status = repo
        .status(gix::progress::Discard)
        .map_err(|error| format!("preparing worktree status: {error}"))?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_iter(Vec::<BString>::new())
        .map_err(|error| format!("reading worktree status: {error}"))?;
    if status
        .next()
        .transpose()
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err(
            "the working tree is not clean; commit or stash local changes before checking out the conversation head"
                .to_string(),
        );
    }

    let commit_id = parse_oid(head)?;
    let commit = repo
        .find_commit(commit_id)
        .map_err(|error| format!("reading conversation commit {head}: {error}"))?;
    let tree_id = commit
        .tree_id()
        .map_err(|error| format!("reading conversation tree {head}: {error}"))?;
    let old_index = repo
        .index_or_empty()
        .map_err(|error| format!("opening worktree index: {error}"))?;
    let mut new_index = repo
        .index_from_tree(&tree_id)
        .map_err(|error| format!("building index for {head}: {error}"))?;
    let new_paths = new_index
        .entries_with_paths_by_filter_map(|path, _| Some(path.to_owned()))
        .map(|(path, _)| path.to_owned())
        .collect::<HashSet<_>>();
    let workdir = repo
        .workdir()
        .ok_or_else(|| "the TUI requires a non-bare repository".to_string())?;
    let mut removed = Vec::new();
    for (path, ()) in old_index.entries_with_paths_by_filter_map(|_, _| Some(())) {
        if !new_paths.contains(path) {
            removed.push(git_path(workdir, path));
        }
    }
    removed.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in removed {
        remove_worktree_entry(&path)?;
        remove_empty_parents(path.parent(), workdir)?;
    }

    let mut options = repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .map_err(|error| format!("preparing checkout: {error}"))?;
    options.overwrite_existing = true;
    let objects_dir = repo.clone().into_sync().objects_dir().to_owned();
    let objects = gix::odb::at(objects_dir)
        .and_then(|objects| objects.into_arc())
        .map_err(|error| format!("opening repository objects for checkout: {error}"))?;
    let outcome = gix::worktree::state::checkout(
        &mut new_index,
        workdir,
        objects,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &AtomicBool::new(false),
        options,
    )
    .map_err(|error| format!("checking out conversation {head}: {error}"))?;
    if !outcome.errors.is_empty() || !outcome.collisions.is_empty() {
        return Err(format!(
            "checking out conversation {head} left {} errors and {} path collisions",
            outcome.errors.len(),
            outcome.collisions.len()
        ));
    }
    new_index
        .write(Default::default())
        .map_err(|error| format!("writing worktree index: {error}"))?;
    repo.reference(
        "HEAD",
        commit_id,
        gix::refs::transaction::PreviousValue::Any,
        format!("checkout: moving to {head}"),
    )
    .map_err(|error| format!("detaching HEAD at {head}: {error}"))?;
    Ok(())
}

/// Commit the current working tree onto the local `HEAD` and return the tree
/// hash the commit carries.
///
/// This is the inverse of `load_conversation_workspace`: after checking out a
/// conversation head and editing files, `/update-tree` folds those files into a
/// user-authored turn. It deliberately DOES commit — staging every non-ignored
/// change and committing when the tree is dirty — so the checkout is left
/// clean and its `HEAD` matches exactly what the turn receives. A later
/// `Ctrl+L` onto the conversation's new head then succeeds instead of tripping
/// the clean-tree guard. When the working tree is already clean (the user
/// committed the changes themselves), nothing is committed and the current
/// `HEAD`'s tree is returned. gix's status walk respects `.gitignore`, so the commit
/// mirrors what a normal commit of the working tree would contain.
pub(crate) fn commit_working_tree(message: &str, cwd: &Path) -> Result<String, String> {
    let repo = open_repo(cwd)?;
    let mut index = repo
        .index_or_empty()
        .map_err(|error| format!("opening worktree index: {error}"))?
        .as_ref()
        .clone();
    let changes = repo
        .status(gix::progress::Discard)
        .map_err(|error| format!("preparing worktree status: {error}"))?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(Vec::<BString>::new())
        .map_err(|error| format!("reading worktree status: {error}"))?
        .map(|item| {
            let item = item.map_err(|error| error.to_string())?;
            Ok((item.summary(), item.rela_path().to_owned()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    for (summary, path) in changes {
        use gix::status::index_worktree::iter::Summary;
        match summary {
            Some(Summary::Removed) => {
                index.remove_entries(|_, entry_path, _| entry_path == path.as_bstr());
            }
            Some(Summary::Added | Summary::Modified | Summary::TypeChange) => {
                stage_path(&repo, &mut index, path.as_bstr())?;
            }
            Some(Summary::Conflict | Summary::IntentToAdd) => {
                return Err(format!(
                    "cannot commit worktree while {path} has unresolved index state"
                ));
            }
            Some(Summary::Renamed | Summary::Copied) => {
                unreachable!("rename tracking is disabled for worktree status")
            }
            None => {}
        }
    }
    index.sort_entries();
    let tree_id = write_index_tree(&repo, &index)?;
    index
        .write(Default::default())
        .map_err(|error| format!("writing worktree index: {error}"))?;

    let head = repo
        .head_commit()
        .map_err(|error| format!("reading HEAD commit: {error}"))?;
    let head_tree = head
        .tree_id()
        .map_err(|error| format!("reading HEAD tree: {error}"))?;
    if tree_id != head_tree.detach() {
        repo.commit("HEAD", message, tree_id, [head.id])
            .map_err(|error| format!("committing worktree: {error}"))?;
    }
    Ok(tree_id.to_string())
}

/// Publish the virtual workspace as a clean branch without checking it out.
///
/// Conversation commits retain their internal step DAG as second parents. A
/// PR should not expose that implementation history or retain superseded
/// snapshots, so the publish branch is one clean commit above the freshly
/// fetched PR-base tip. Publishing to the default branch merges the complete
/// workspace; publishing elsewhere replays this conversation's delta so a
/// child can target its parent's clean snapshot without duplicating that work.
pub(crate) fn publish_conversation_pr(
    name: &str,
    diff: &WorkspaceDiff,
    pr_base: &str,
    default_base: &str,
) -> Result<String, String> {
    let cwd = Path::new(".");
    let branch = format!("caos/{name}");
    let github_repo = github_repo_name(&open_repo(cwd)?)?;
    let pr_base_commit = fetch_remote_branch_tip(pr_base, cwd)?;
    let change_base = (pr_base != default_base).then_some(diff.base_commit.as_str());
    prepare_publish_branch(name, diff, &pr_base_commit, change_base, cwd)?;
    push_publish_branch(&branch, cwd)?;

    let owner = github_repo
        .split_once('/')
        .map(|(owner, _)| owner)
        .ok_or_else(|| format!("invalid GitHub repository {github_repo:?}"))?;
    let endpoint = format!(
        "repos/{github_repo}/pulls?head={}&base={}&state=open&per_page=1",
        query_component(&format!("{owner}:{branch}")),
        query_component(pr_base),
    );
    let existing = gh_api("GET", &endpoint, None, false, cwd)?
        .expect("a successful GitHub API response has a body");
    if let Some(url) = existing
        .as_array()
        .and_then(|pulls| pulls.first())
        .and_then(|pull| pull.get("html_url"))
        .and_then(|url| url.as_str())
    {
        return Ok(url.to_string());
    }
    let body = format!(
        "Published from virtual CAOS conversation `{name}` at `{}`.",
        short_hash(&diff.head)
    );
    let created = gh_api(
        "POST",
        &format!("repos/{github_repo}/pulls"),
        Some(&serde_json::json!({
            "head": branch,
            "base": pr_base,
            "title": format!("CAOS conversation {name}"),
            "body": body,
        })),
        false,
        cwd,
    )?
    .expect("a successful GitHub API response has a body");
    created
        .get("html_url")
        .and_then(|url| url.as_str())
        .map(str::to_string)
        .ok_or_else(|| "GitHub returned no URL for the created pull request".to_string())
}

/// Resolve the default branch and its tip from the LOCAL branch, without
/// touching the network.
///
/// Starting a new conversation only needs a base commit to build on, and the
/// tip of your local default branch (e.g. `main`) is a fine one. This discovers
/// the default branch *name* from the `origin/HEAD` symref, then reads the local
/// `refs/heads/<name>` — not the `origin/<name>` tracking ref — so it reflects
/// your checked-out branch as it is right now. It performs no remote operation,
/// so it stays instant (e.g. on every Ctrl+N) instead of blocking on
/// round-trips to `origin`. Publishing a PR still fetches, where a fresh remote
/// tip matters.
pub(crate) fn local_default_branch_tip(cwd: &Path) -> Result<(String, String), String> {
    // `refs/remotes/origin/HEAD` is the local symref recording origin's default
    // branch; it is normally set when the repository is cloned.
    let repo = open_repo(cwd)?;
    let head = repo
        .find_reference("refs/remotes/origin/HEAD")
        .map_err(|error| format!("could not resolve origin's default branch locally: {error}"))?;
    let head_target = head.target();
    let head_ref = head_target
        .try_name()
        .ok_or_else(|| "refs/remotes/origin/HEAD is not symbolic".to_string())?
        .as_bstr()
        .to_str_lossy();
    let branch = head_ref
        .strip_prefix("refs/remotes/origin/")
        .ok_or_else(|| format!("origin/HEAD points outside refs/remotes/origin: {head_ref}"))?
        .to_string();
    let local_ref = format!("refs/heads/{branch}");
    let mut reference = repo
        .find_reference(&local_ref)
        .map_err(|error| format!("local default branch {branch:?} not found: {error}"))?;
    let commit = reference
        .peel_to_commit()
        .map_err(|error| format!("local default branch {branch:?} is not a commit: {error}"))?;
    Ok((branch, commit.id.to_string()))
}

pub(crate) fn remote_default_branch(cwd: &Path) -> Result<String, String> {
    let repo = open_repo(cwd)?;
    let remote = repo
        .find_remote("origin")
        .map_err(|error| format!("finding origin: {error}"))?;
    let connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(|error| format!("connecting to origin: {error}"))?;
    let options = gix::remote::ref_map::Options {
        prefix_from_spec_as_filter_on_remote: false,
        ..Default::default()
    };
    let (ref_map, _) = connection
        .ref_map(gix::progress::Discard, options)
        .map_err(|error| format!("listing origin references: {error}"))?;
    for reference in ref_map.remote_refs {
        if let gix::protocol::handshake::Ref::Symbolic {
            full_ref_name,
            target,
            ..
        } = reference
        {
            if full_ref_name == "HEAD" {
                return parse_remote_head_target(target.as_bstr());
            }
        }
    }
    Err("origin HEAD did not advertise a default branch".to_string())
}

fn parse_remote_head_target(target: &BStr) -> Result<String, String> {
    let target = target.to_str_lossy();
    let branch = target
        .strip_prefix("refs/heads/")
        .ok_or_else(|| format!("origin HEAD points outside refs/heads: {target}"))?;
    if branch.is_empty() {
        return Err("origin HEAD advertises an empty default branch".to_string());
    }
    Ok(branch.to_string())
}

#[cfg(test)]
fn parse_remote_default_branch(output: &str) -> Result<String, String> {
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(marker), Some(reference), Some(target)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if marker == "ref:" && target == "HEAD" {
            let branch = reference
                .strip_prefix("refs/heads/")
                .ok_or_else(|| format!("origin HEAD points outside refs/heads: {reference}"))?;
            if branch.is_empty() {
                return Err("origin HEAD advertises an empty default branch".to_string());
            }
            return Ok(branch.to_string());
        }
    }
    Err("origin HEAD did not advertise a default branch".to_string())
}

fn fetch_remote_branch_tip(branch: &str, cwd: &Path) -> Result<String, String> {
    let remote_ref = format!("refs/heads/{branch}");
    let tracking_ref = format!("refs/remotes/origin/{branch}");
    let repo = open_repo(cwd)?;
    let mut remote = repo
        .find_remote("origin")
        .map_err(|error| format!("finding origin: {error}"))?;
    let refspec = BString::from(format!("+{remote_ref}:{tracking_ref}"));
    remote
        .replace_refspecs([refspec], gix::remote::Direction::Fetch)
        .map_err(|error| format!("configuring fetch for {remote_ref}: {error}"))?;
    let connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(|error| format!("connecting to origin: {error}"))?;
    connection
        .prepare_fetch(gix::progress::Discard, Default::default())
        .map_err(|error| format!("preparing fetch of {remote_ref}: {error}"))?
        .receive(gix::progress::Discard, &AtomicBool::new(false))
        .map_err(|error| format!("fetching {remote_ref}: {error}"))?;
    let mut reference = repo
        .find_reference(&tracking_ref)
        .map_err(|error| format!("reading fetched {tracking_ref}: {error}"))?;
    let tip = reference
        .peel_to_commit()
        .map_err(|error| format!("fetched {tracking_ref} is not a commit: {error}"))?;
    Ok(tip.id.to_string())
}

fn push_publish_branch(branch: &str, cwd: &Path) -> Result<(), String> {
    let branch_ref = format!("refs/heads/{branch}");
    let repo = open_repo(cwd)?;
    let local_tip = repo
        .find_reference(&branch_ref)
        .map_err(|error| format!("reading local publish branch {branch}: {error}"))?
        .id()
        .detach();
    if let Some(path) = local_origin_path(&repo)? {
        return push_to_local_origin(&repo, &path, &branch_ref, local_tip);
    }
    push_to_github(&repo, &branch_ref, local_tip, cwd)
}

#[cfg(test)]
fn remote_branch_tip(branch_ref: &str, cwd: &Path) -> Result<Option<String>, String> {
    let repo = open_repo(cwd)?;
    let remote = repo
        .find_remote("origin")
        .map_err(|error| format!("finding origin: {error}"))?;
    let connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(|error| format!("connecting to origin: {error}"))?;
    let options = gix::remote::ref_map::Options {
        prefix_from_spec_as_filter_on_remote: false,
        ..Default::default()
    };
    let (ref_map, _) = connection
        .ref_map(gix::progress::Discard, options)
        .map_err(|error| format!("listing origin references: {error}"))?;
    for reference in ref_map.remote_refs {
        let (name, id) = match reference {
            gix::protocol::handshake::Ref::Direct {
                full_ref_name,
                object,
            } => (full_ref_name, object),
            gix::protocol::handshake::Ref::Peeled {
                full_ref_name, tag, ..
            } => (full_ref_name, tag),
            gix::protocol::handshake::Ref::Symbolic {
                full_ref_name,
                object,
                ..
            } => (full_ref_name, object),
            gix::protocol::handshake::Ref::Unborn { .. } => continue,
        };
        if name == branch_ref {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
}

fn local_origin_path(repo: &gix::Repository) -> Result<Option<PathBuf>, String> {
    let remote = repo
        .find_remote("origin")
        .map_err(|error| format!("finding origin: {error}"))?;
    let url = remote
        .url(gix::remote::Direction::Push)
        .or_else(|| remote.url(gix::remote::Direction::Fetch))
        .ok_or_else(|| "origin has no URL".to_string())?
        .to_bstring()
        .to_string();
    if url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("ssh://")
        || url.starts_with("git://")
        || url.contains('@')
    {
        return Ok(None);
    }
    let path = url.strip_prefix("file://").unwrap_or(&url);
    let path = PathBuf::from(path);
    Ok(Some(if path.is_absolute() {
        path
    } else {
        repo.workdir().unwrap_or_else(|| repo.git_dir()).join(path)
    }))
}

fn github_repo_name(repo: &gix::Repository) -> Result<String, String> {
    let remote = repo
        .find_remote("origin")
        .map_err(|error| format!("finding origin: {error}"))?;
    let url = remote
        .url(gix::remote::Direction::Push)
        .or_else(|| remote.url(gix::remote::Direction::Fetch))
        .ok_or_else(|| "origin has no URL".to_string())?;
    let display_url = url.to_bstring().to_string();
    if url.host() != Some("github.com") {
        return Err(format!("origin is not a GitHub repository: {display_url}"));
    }
    let path = url
        .path
        .to_str()
        .map_err(|_| format!("origin URL is not UTF-8: {display_url}"))?
        .trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segments = path.split('/');
    let (Some(owner), Some(repo_name), None) = (segments.next(), segments.next(), segments.next())
    else {
        return Err(format!(
            "cannot derive owner/repository from origin URL {display_url}"
        ));
    };
    if owner.is_empty() || repo_name.is_empty() {
        return Err(format!(
            "cannot derive owner/repository from origin URL {display_url}"
        ));
    }
    Ok(format!("{owner}/{repo_name}"))
}

fn query_component(value: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a String");
        }
    }
    encoded
}

/// Publish a branch to GitHub through its Git database API.
///
/// gix intentionally doesn't implement pushing. PR publication already
/// requires `gh`, so use that authenticated client to upload the changed Git
/// objects and update the branch without adding a dependency on the `git`
/// executable. Local filesystem remotes take the direct gix path below.
fn push_to_github(
    repo: &gix::Repository,
    branch_ref: &str,
    local_tip: gix::ObjectId,
    cwd: &Path,
) -> Result<(), String> {
    let repo_name = github_repo_name(repo)?;

    // Uploading objects doesn't mutate the branch, so retain the first value
    // and check it again immediately before the ref update. GitHub's REST ref
    // update has no expected-old field; this rejects changes observed during
    // the potentially long object-upload window.
    let expected_tip = github_branch_tip(&repo_name, branch_ref, cwd)?;
    let github_tip =
        upload_github_commit(repo, &repo_name, local_tip, expected_tip.as_deref(), cwd)?;
    let observed_tip = github_branch_tip(&repo_name, branch_ref, cwd)?;
    if observed_tip != expected_tip {
        return Err(format!(
            "origin branch {branch_ref} changed while its objects were being uploaded"
        ));
    }
    if observed_tip.as_deref() == Some(github_tip.as_str()) {
        return Ok(());
    }

    if observed_tip.is_some() {
        let ref_path = branch_ref
            .strip_prefix("refs/")
            .ok_or_else(|| format!("invalid branch reference {branch_ref}"))?;
        gh_api(
            "PATCH",
            &format!("repos/{repo_name}/git/refs/{ref_path}"),
            Some(&serde_json::json!({"sha": github_tip, "force": true})),
            false,
            cwd,
        )?;
    } else {
        gh_api(
            "POST",
            &format!("repos/{repo_name}/git/refs"),
            Some(&serde_json::json!({"ref": branch_ref, "sha": github_tip})),
            false,
            cwd,
        )?;
    }
    Ok(())
}

fn github_branch_tip(
    repo_name: &str,
    branch_ref: &str,
    cwd: &Path,
) -> Result<Option<String>, String> {
    let ref_path = branch_ref
        .strip_prefix("refs/")
        .ok_or_else(|| format!("invalid branch reference {branch_ref}"))?;
    let Some(value) = gh_api(
        "GET",
        &format!("repos/{repo_name}/git/ref/{ref_path}"),
        None,
        true,
        cwd,
    )?
    else {
        return Ok(None);
    };
    value
        .pointer("/object/sha")
        .and_then(serde_json::Value::as_str)
        .map(|sha| Some(sha.to_string()))
        .ok_or_else(|| format!("GitHub returned no object hash for {branch_ref}"))
}

fn upload_github_commit(
    repo: &gix::Repository,
    repo_name: &str,
    local_tip: gix::ObjectId,
    current_remote_tip: Option<&str>,
    cwd: &Path,
) -> Result<String, String> {
    let commit = repo
        .find_commit(local_tip)
        .map_err(|error| format!("reading publish commit {local_tip}: {error}"))?;
    let tree = commit
        .tree_id()
        .map_err(|error| format!("reading publish tree: {error}"))?
        .detach();
    let parents = commit
        .parent_ids()
        .map(|id| id.detach())
        .collect::<Vec<_>>();
    let [parent] = parents.as_slice() else {
        return Err("a publish commit must have exactly one parent".to_string());
    };
    let message = commit
        .message_raw()
        .map_err(|error| format!("reading publish commit message: {error}"))?
        .to_str_lossy()
        .into_owned();

    // REST-created commits have GitHub-provided author metadata and therefore
    // need not have the local commit's hash. Reuse a matching remote commit so
    // publishing an unchanged conversation is still idempotent.
    if let Some(remote_tip) = current_remote_tip {
        let value = gh_api(
            "GET",
            &format!("repos/{repo_name}/git/commits/{remote_tip}"),
            None,
            false,
            cwd,
        )?
        .expect("a successful GitHub API response has a body");
        let tree_name = tree.to_string();
        let parent_name = parent.to_string();
        let same_tree =
            value.pointer("/tree/sha").and_then(|v| v.as_str()) == Some(tree_name.as_str());
        let same_parent = value
            .get("parents")
            .and_then(|parents| parents.as_array())
            .is_some_and(|parents| {
                parents.len() == 1
                    && parents[0].get("sha").and_then(|sha| sha.as_str())
                        == Some(parent_name.as_str())
            });
        let same_message = value
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|remote| remote.trim_end() == message.trim_end());
        if same_tree && same_parent && same_message {
            return Ok(remote_tip.to_string());
        }
    }

    let parent_commit = repo
        .find_commit(*parent)
        .map_err(|error| format!("reading publish parent {parent}: {error}"))?;
    let base_tree = parent_commit
        .tree_id()
        .map_err(|error| format!("reading publish parent tree: {error}"))?
        .detach();
    let github_tree = upload_github_tree(repo, repo_name, base_tree, tree, cwd)?;
    let value = gh_api(
        "POST",
        &format!("repos/{repo_name}/git/commits"),
        Some(&serde_json::json!({
            "message": message,
            "tree": github_tree,
            "parents": [parent.to_string()],
        })),
        false,
        cwd,
    )?
    .expect("a successful GitHub API response has a body");
    value
        .get("sha")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .ok_or_else(|| "GitHub returned no hash for the uploaded commit".to_string())
}

#[derive(Clone, Copy)]
struct GithubTreeEntry {
    mode: gix::objs::tree::EntryMode,
    id: gix::ObjectId,
}

fn upload_github_tree(
    repo: &gix::Repository,
    repo_name: &str,
    base_tree: gix::ObjectId,
    desired_tree: gix::ObjectId,
    cwd: &Path,
) -> Result<String, String> {
    // Every object below the parent tree is already present on GitHub. Walk the
    // desired tree bottom-up, uploading only objects outside that known set.
    // Building each changed directory as a complete tree also handles file ↔
    // directory type changes without overlapping path updates.
    let mut known_remote = HashSet::new();
    collect_tree_closure(repo, base_tree, &mut known_remote)?;
    upload_github_tree_inner(repo, repo_name, desired_tree, &mut known_remote, cwd)?;
    Ok(desired_tree.to_string())
}

fn upload_github_tree_inner(
    repo: &gix::Repository,
    repo_name: &str,
    tree_id: gix::ObjectId,
    known_remote: &mut HashSet<gix::ObjectId>,
    cwd: &Path,
) -> Result<(), String> {
    if known_remote.contains(&tree_id) {
        return Ok(());
    }
    let entries = read_tree_entries(repo, tree_id)?;
    let mut body = Vec::with_capacity(entries.len());
    for (name, entry) in entries {
        match entry.mode.kind() {
            gix::objs::tree::EntryKind::Tree => {
                upload_github_tree_inner(repo, repo_name, entry.id, known_remote, cwd)?;
            }
            gix::objs::tree::EntryKind::Commit => {
                // A submodule commit belongs to another repository and isn't
                // expected to exist in this repository's object database.
            }
            _ if !known_remote.contains(&entry.id) => {
                upload_github_blob(repo, repo_name, entry.id, cwd)?;
                known_remote.insert(entry.id);
            }
            _ => {}
        }
        body.push(serde_json::json!({
            "path": name.to_str().map_err(|_| format!("GitHub cannot publish non-UTF-8 path {name:?}"))?,
            "mode": format!("{:06o}", entry.mode.value()),
            "type": github_tree_entry_type(entry.mode),
            "sha": entry.id.to_string(),
        }));
    }

    let value = gh_api(
        "POST",
        &format!("repos/{repo_name}/git/trees"),
        Some(&serde_json::json!({"tree": body})),
        false,
        cwd,
    )?
    .expect("a successful GitHub API response has a body");
    let uploaded = value
        .get("sha")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "GitHub returned no hash for the uploaded tree".to_string())?;
    if uploaded != tree_id.to_string() {
        return Err(format!(
            "GitHub stored publish tree {tree_id} as {uploaded}"
        ));
    }
    known_remote.insert(tree_id);
    Ok(())
}

fn collect_tree_closure(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    output: &mut HashSet<gix::ObjectId>,
) -> Result<(), String> {
    if !output.insert(tree_id) {
        return Ok(());
    }
    for entry in read_tree_entries(repo, tree_id)?.into_values() {
        if entry.mode.kind() == gix::objs::tree::EntryKind::Tree {
            collect_tree_closure(repo, entry.id, output)?;
        } else {
            output.insert(entry.id);
        }
    }
    Ok(())
}

fn read_tree_entries(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
) -> Result<BTreeMap<BString, GithubTreeEntry>, String> {
    let object = repo
        .find_object(tree_id)
        .map_err(|error| format!("reading tree {tree_id}: {error}"))?;
    let tree = gix::objs::TreeRef::from_bytes(&object.data, repo.object_hash())
        .map_err(|error| format!("decoding tree {tree_id}: {error}"))?;
    Ok(tree
        .entries
        .into_iter()
        .map(|entry| {
            (
                entry.filename.to_owned(),
                GithubTreeEntry {
                    mode: entry.mode,
                    id: entry.oid.to_owned(),
                },
            )
        })
        .collect())
}

fn github_tree_entry_type(mode: gix::objs::tree::EntryMode) -> &'static str {
    match mode.kind() {
        gix::objs::tree::EntryKind::Tree => "tree",
        gix::objs::tree::EntryKind::Commit => "commit",
        _ => "blob",
    }
}

fn upload_github_blob(
    repo: &gix::Repository,
    repo_name: &str,
    id: gix::ObjectId,
    cwd: &Path,
) -> Result<(), String> {
    let object = repo
        .find_object(id)
        .map_err(|error| format!("reading blob {id}: {error}"))?;
    if object.kind != gix::object::Kind::Blob {
        return Err(format!("tree entry {id} is not a blob"));
    }
    let value = gh_api(
        "POST",
        &format!("repos/{repo_name}/git/blobs"),
        Some(&serde_json::json!({
            "content": super::base64_encode(&object.data),
            "encoding": "base64",
        })),
        false,
        cwd,
    )?
    .expect("a successful GitHub API response has a body");
    let uploaded = value
        .get("sha")
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("GitHub returned no hash for blob {id}"))?;
    if uploaded != id.to_string() {
        return Err(format!("GitHub stored blob {id} as {uploaded}"));
    }
    Ok(())
}

fn gh_api(
    method: &str,
    endpoint: &str,
    body: Option<&serde_json::Value>,
    allow_not_found: bool,
    cwd: &Path,
) -> Result<Option<serde_json::Value>, String> {
    let encoded = body
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|error| format!("encoding GitHub API request: {error}"))?;
    let mut command = Command::new("gh");
    command
        .arg("api")
        .arg("--method")
        .arg(method)
        .arg(endpoint)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if encoded.is_some() {
        command.arg("--input").arg("-").stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("running gh api: {error}"))?;
    if let Some(encoded) = encoded {
        child
            .stdin
            .take()
            .ok_or_else(|| "gh api stdin was not available".to_string())?
            .write_all(&encoded)
            .map_err(|error| format!("writing GitHub API request: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("waiting for gh api: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if allow_not_found && detail.contains("HTTP 404") {
            return Ok(None);
        }
        return Err(if detail.is_empty() {
            format!("gh api exited with {}", output.status)
        } else {
            detail
        });
    }
    serde_json::from_slice(&output.stdout)
        .map(Some)
        .map_err(|error| format!("decoding GitHub API response: {error}"))
}

fn push_to_local_origin(
    source: &gix::Repository,
    remote_path: &Path,
    branch_ref: &str,
    local_tip: gix::ObjectId,
) -> Result<(), String> {
    let target = gix::open(remote_path)
        .map_err(|error| format!("opening local origin {}: {error}", remote_path.display()))?;
    let previous = target
        .try_find_reference(branch_ref)
        .map_err(|error| format!("reading origin ref {branch_ref}: {error}"))?
        .and_then(|reference| reference.try_id().map(|id| id.detach()));
    copy_object_closure(source, &target, local_tip)?;
    let expected = match previous {
        Some(id) => gix::refs::transaction::PreviousValue::MustExistAndMatch(id.into()),
        None => gix::refs::transaction::PreviousValue::MustNotExist,
    };
    target
        .reference(
            branch_ref,
            local_tip,
            expected,
            "push: update CAOS publish branch",
        )
        .map(|_| ())
        .map_err(|error| format!("updating origin ref {branch_ref}: {error}"))
}

fn copy_object_closure(
    source: &gix::Repository,
    target: &gix::Repository,
    root: gix::ObjectId,
) -> Result<(), String> {
    let mut pending = vec![root];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) || target.find_object(id).is_ok() {
            continue;
        }
        let object = source
            .find_object(id)
            .map_err(|error| format!("reading object {id} for push: {error}"))?;
        pending.extend(object_child_ids(
            object.kind,
            &object.data,
            source.object_hash(),
        )?);
        let stored = gix::objs::Write::write_buf(&target.objects, object.kind, &object.data)
            .map_err(|error| format!("writing object {id} to origin: {error}"))?;
        if stored != id {
            return Err(format!("origin stored object {id} as {stored}"));
        }
    }
    Ok(())
}

fn object_child_ids(
    kind: gix::object::Kind,
    data: &[u8],
    hash_kind: gix::hash::Kind,
) -> Result<Vec<gix::ObjectId>, String> {
    Ok(match kind {
        gix::object::Kind::Blob => Vec::new(),
        gix::object::Kind::Tree => gix::objs::TreeRef::from_bytes(data, hash_kind)
            .map_err(|error| format!("reading tree object: {error}"))?
            .entries
            .into_iter()
            .map(|entry| entry.oid.to_owned())
            .collect(),
        gix::object::Kind::Commit => {
            let commit = gix::objs::CommitRef::from_bytes(data, hash_kind)
                .map_err(|error| format!("reading commit object: {error}"))?;
            let mut children = Vec::with_capacity(commit.parents.len() + 1);
            children.push(commit.tree());
            children.extend(commit.parents());
            children
        }
        gix::object::Kind::Tag => vec![gix::objs::TagRef::from_bytes(data, hash_kind)
            .map_err(|error| format!("reading tag object: {error}"))?
            .target()],
    })
}

pub(crate) fn prepare_publish_branch(
    name: &str,
    diff: &WorkspaceDiff,
    publish_base: &str,
    change_base: Option<&str>,
    cwd: &Path,
) -> Result<String, String> {
    let branch = format!("caos/{name}");
    let branch_ref = format!("refs/heads/{branch}");
    let repo = open_repo(cwd)?;
    let publish_tree = merge_publish_tree(&diff.head, publish_base, change_base, cwd)?;
    let publish_tree_id = parse_oid(&publish_tree)?;
    let publish_base_id = parse_oid(publish_base)?;
    let previous = repo
        .try_find_reference(&branch_ref)
        .map_err(|error| format!("reading {branch_ref}: {error}"))?
        .map(|mut reference| reference.peel_to_commit())
        .transpose()
        .map_err(|error| format!("reading {branch_ref} commit: {error}"))?;
    let reusable = previous.as_ref().is_some_and(|commit| {
        commit
            .tree_id()
            .is_ok_and(|tree| tree.detach() == publish_tree_id)
            && commit.parent_ids().next().map(|parent| parent.detach()) == Some(publish_base_id)
    });
    let publish_commit = if reusable {
        previous
            .as_ref()
            .expect("a reusable publish commit exists")
            .id
    } else {
        repo.new_commit(
            format!(
                "CAOS conversation {} at {}",
                short_hash(name),
                short_hash(&diff.head)
            ),
            publish_tree_id,
            [publish_base_id],
        )
        .map_err(|error| format!("creating publish commit: {error}"))?
        .id
    };
    let previous_id = previous.as_ref().map(|commit| commit.id);
    if previous_id != Some(publish_commit) {
        let expected = match previous_id {
            Some(id) => gix::refs::transaction::PreviousValue::MustExistAndMatch(id.into()),
            None => gix::refs::transaction::PreviousValue::MustNotExist,
        };
        repo.reference(
            branch_ref.as_str(),
            publish_commit,
            expected,
            format!("publish: {branch}"),
        )
        .map_err(|error| format!("updating {branch_ref}: {error}"))?;
    }
    Ok(branch)
}

/// Merge the conversation's final state with the current PR base without
/// touching the real index or working tree. With no `change_base`, Git chooses
/// the natural merge base so a default-branch publish includes inherited
/// filesystem work. With one, only changes after that commit are replayed onto
/// a non-default PR base. Only the resulting tree is retained either way.
fn merge_publish_tree(
    head: &str,
    publish_base: &str,
    change_base: Option<&str>,
    cwd: &Path,
) -> Result<String, String> {
    let repo = open_repo(cwd)?;
    let head_id = parse_oid(head)?;
    let publish_base_id = parse_oid(publish_base)?;
    // A child conversation starts from its parent's internal commit, while a
    // stacked PR targets the parent's clean snapshot commit. Those commits
    // intentionally have different histories. Give the selected snapshot a
    // temporary parent at the conversation's starting point so merge-tree
    // applies only this conversation's delta instead of re-merging its parent.
    let merge_tip = if let Some(change_base) = change_base {
        let publish_commit = repo
            .find_commit(publish_base_id)
            .map_err(|error| format!("reading publish-base tree: {error}"))?;
        let publish_tree = publish_commit
            .tree_id()
            .map_err(|error| format!("reading publish-base tree: {error}"))?;
        repo.new_commit(
            "temporary CAOS publish merge base",
            publish_tree,
            [parse_oid(change_base)?],
        )
        .map_err(|error| format!("creating temporary publish merge base: {error}"))?
        .id
    } else {
        publish_base_id
    };
    let labels = gix::merge::blob::builtin_driver::text::Labels {
        ancestor: Some("base".into()),
        current: Some("selected PR base".into()),
        other: Some("conversation".into()),
    };
    let options = repo
        .tree_merge_options()
        .map_err(|error| format!("preparing publish merge: {error}"))?
        .into();
    let mut outcome = repo
        .merge_commits(merge_tip, head_id, labels, options)
        .map_err(|error| format!("merging conversation into selected PR base: {error}"))?;
    let unresolved = gix::merge::tree::TreatAsUnresolved::git();
    if outcome.tree_merge.has_unresolved_conflicts(unresolved) {
        let mut conflicts = outcome
            .tree_merge
            .conflicts
            .iter()
            .filter(|conflict| conflict.is_unresolved(unresolved))
            .map(|conflict| conflict.ours.location().to_str_lossy().into_owned())
            .collect::<Vec<_>>();
        conflicts.sort();
        conflicts.dedup();
        let paths = conflicts
            .iter()
            .map(|path| format!("  {path}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "conversation changes conflict with the selected PR base branch:\n{paths}\nresolve these files against the latest selected base before publishing"
        ));
    }
    outcome
        .tree_merge
        .tree
        .write()
        .map(|tree| tree.detach().to_string())
        .map_err(|error| format!("writing publish merge tree: {error}"))
}

fn open_repo(cwd: &Path) -> Result<gix::Repository, String> {
    gix::discover(cwd).map_err(|error| format!("opening git repository: {error}"))
}

fn parse_oid(value: &str) -> Result<gix::ObjectId, String> {
    gix::ObjectId::from_hex(value.trim().as_bytes())
        .map_err(|error| format!("invalid git object id {value:?}: {error}"))
}

fn git_path(workdir: &Path, path: &BStr) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        workdir.join(OsStr::from_bytes(path.as_ref()))
    }
    #[cfg(not(unix))]
    {
        workdir.join(path.to_str_lossy().as_ref())
    }
}

fn remove_worktree_entry(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    }
    .map_err(|error| format!("removing {}: {error}", path.display()))
}

fn remove_empty_parents(mut path: Option<&Path>, workdir: &Path) -> Result<(), String> {
    while let Some(dir) = path {
        if dir == workdir {
            break;
        }
        match std::fs::remove_dir(dir) {
            Ok(()) => path = dir.parent(),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(error) => return Err(format!("removing {}: {error}", dir.display())),
        }
    }
    Ok(())
}

fn stage_path(
    repo: &gix::Repository,
    index: &mut gix::index::State,
    path: &BStr,
) -> Result<(), String> {
    let (mut pipeline, _) = repo
        .filter_pipeline(None)
        .map_err(|error| format!("preparing filters for {path}: {error}"))?;
    let Some((id, kind, _)) = pipeline
        .worktree_file_to_object(path, index)
        .map_err(|error| format!("staging {path}: {error}"))?
    else {
        return Err(format!("{path} is not a file, symlink, or submodule"));
    };
    let workdir = repo
        .workdir()
        .ok_or_else(|| "the TUI requires a non-bare repository".to_string())?;
    let metadata = gix::index::fs::Metadata::from_path_no_follow(&git_path(workdir, path))
        .map_err(|error| format!("reading metadata for {path}: {error}"))?;
    let stat = gix::index::entry::Stat::from_fs(&metadata)
        .map_err(|error| format!("reading timestamps for {path}: {error}"))?;
    let mode = gix::index::entry::Mode::from(gix::objs::tree::EntryMode::from(kind));
    if let Some(entry) =
        index.entry_mut_by_path_and_stage(path, gix::index::entry::Stage::Unconflicted)
    {
        entry.id = id;
        entry.mode = mode;
        entry.stat = stat;
    } else {
        index.dangerously_push_entry(stat, id, gix::index::entry::Flags::empty(), mode, path);
    }
    Ok(())
}

#[derive(Default)]
struct IndexTree {
    entries: BTreeMap<BString, IndexTreeEntry>,
}

enum IndexTreeEntry {
    Leaf {
        mode: gix::objs::tree::EntryMode,
        id: gix::ObjectId,
    },
    Tree(IndexTree),
}

impl IndexTree {
    fn insert(
        &mut self,
        path: &BStr,
        mode: gix::objs::tree::EntryMode,
        id: gix::ObjectId,
    ) -> Result<(), String> {
        let mut components = path.split(|byte| *byte == b'/').peekable();
        let mut tree = self;
        while let Some(component) = components.next() {
            let name = BString::from(component);
            if components.peek().is_none() {
                if tree
                    .entries
                    .insert(name, IndexTreeEntry::Leaf { mode, id })
                    .is_some()
                {
                    return Err(format!("duplicate index path {path}"));
                }
                return Ok(());
            }
            tree = match tree
                .entries
                .entry(name)
                .or_insert_with(|| IndexTreeEntry::Tree(IndexTree::default()))
            {
                IndexTreeEntry::Tree(tree) => tree,
                IndexTreeEntry::Leaf { .. } => {
                    return Err(format!("index path {path} traverses through a file"));
                }
            };
        }
        Err("the index contains an empty path".to_string())
    }

    fn write(self, repo: &gix::Repository) -> Result<gix::ObjectId, String> {
        let mut entries = Vec::with_capacity(self.entries.len());
        for (filename, entry) in self.entries {
            let (mode, oid) = match entry {
                IndexTreeEntry::Leaf { mode, id } => (mode, id),
                IndexTreeEntry::Tree(tree) => {
                    (gix::objs::tree::EntryKind::Tree.into(), tree.write(repo)?)
                }
            };
            entries.push(gix::objs::tree::Entry {
                mode,
                filename,
                oid,
            });
        }
        entries.sort();
        repo.write_object(&gix::objs::Tree { entries })
            .map(|id| id.detach())
            .map_err(|error| format!("writing tree object: {error}"))
    }
}

fn write_index_tree(
    repo: &gix::Repository,
    index: &gix::index::State,
) -> Result<gix::ObjectId, String> {
    let mut root = IndexTree::default();
    for entry in index.entries() {
        let path = entry.path(index);
        if entry.stage() != gix::index::entry::Stage::Unconflicted {
            return Err(format!(
                "cannot write tree with unresolved index entry {path}"
            ));
        }
        let mode = entry
            .mode
            .to_tree_entry_mode()
            .ok_or_else(|| format!("unsupported index mode for {path}"))?;
        root.insert(path, mode, entry.id)?;
    }
    root.write(repo)
}

#[cfg(test)]
pub(crate) fn capture_required(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = command_output(program, args, cwd)?;
    require_success(program, output).map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
}

#[cfg(test)]
fn capture_optional(program: &str, args: &[&str], cwd: &Path) -> Result<Option<String>, String> {
    let output = command_output(program, args, cwd)?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
fn command_output(program: &str, args: &[&str], cwd: &Path) -> Result<Output, String> {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("running {program}: {error}"))
}

#[cfg(test)]
fn require_success(program: &str, output: Output) -> Result<Vec<u8>, String> {
    if output.status.success() {
        return Ok(output.stdout);
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if detail.is_empty() {
        format!("{program} exited with {}", output.status)
    } else {
        detail
    })
}

fn short_hash(hash: &str) -> &str {
    hash.get(..7).unwrap_or(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIX_ONLY_REPO_ENV: &str = "CAOS_TEST_GIX_ONLY_REPO";
    const GIX_ONLY_HEAD_ENV: &str = "CAOS_TEST_GIX_ONLY_HEAD";

    fn temp_repo(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "caos-cli-tui-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        capture_required("git", &["init", "-q"], &dir).unwrap();
        capture_required("git", &["config", "user.name", "Test User"], &dir).unwrap();
        capture_required("git", &["config", "user.email", "test@example.com"], &dir).unwrap();
        dir
    }

    fn commit_file(dir: &Path, content: &str, message: &str) -> String {
        std::fs::write(dir.join("file.txt"), content).unwrap();
        capture_required("git", &["add", "file.txt"], dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", message], dir).unwrap();
        capture_required("git", &["rev-parse", "HEAD"], dir).unwrap()
    }

    #[test]
    fn workspace_checkout_and_commit_do_not_require_git_on_path() {
        if let (Ok(repo), Ok(head)) = (
            std::env::var(GIX_ONLY_REPO_ENV),
            std::env::var(GIX_ONLY_HEAD_ENV),
        ) {
            let repo = Path::new(&repo);
            load_conversation_workspace(&head, repo).unwrap();
            std::fs::write(repo.join("file.txt"), "edited without git\n").unwrap();
            let tree = commit_working_tree("gix-only edit", repo).unwrap();
            assert_eq!(
                gix::discover(repo)
                    .unwrap()
                    .head_commit()
                    .unwrap()
                    .tree_id()
                    .unwrap()
                    .to_string(),
                tree
            );
            return;
        }

        let dir = temp_repo("gix-only-test");
        let base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "conversation\n", "conversation");
        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        let empty_path = dir.join("empty-path");
        std::fs::create_dir(&empty_path).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("tui::workspace::tests::workspace_checkout_and_commit_do_not_require_git_on_path")
            .arg("--nocapture")
            .env(GIX_ONLY_REPO_ENV, &dir)
            .env(GIX_ONLY_HEAD_ENV, &head)
            .env("PATH", &empty_path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            capture_required("git", &["show", "HEAD:file.txt"], &dir).unwrap(),
            "edited without git"
        );
        assert_eq!(
            capture_required("git", &["show", "-s", "--format=%s", "HEAD"], &dir).unwrap(),
            "gix-only edit"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_requires_a_clean_checkout_and_checks_out_the_conversation_head() {
        let dir = temp_repo("load-test");
        let base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "conversation result\n", "turn");
        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        load_conversation_workspace(&head, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "conversation result\n"
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            head
        );

        std::fs::write(dir.join("file.txt"), "local edit\n").unwrap();
        assert!(load_conversation_workspace(&head, &dir)
            .unwrap_err()
            .contains("working tree is not clean"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn update_tree_commits_the_working_tree_and_returns_its_tree() {
        let dir = temp_repo("snapshot-test");
        let _base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "head\n", "turn");

        // With a clean checkout nothing is committed and the head's tree is
        // returned unchanged.
        let head_tree =
            capture_required("git", &["rev-parse", &format!("{head}^{{tree}}")], &dir).unwrap();
        assert_eq!(commit_working_tree("noop", &dir).unwrap(), head_tree);
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            head
        );

        // Edit a tracked file and add an untracked one, leaving both
        // uncommitted.
        std::fs::write(dir.join("file.txt"), "local edit\n").unwrap();
        std::fs::write(dir.join("new.txt"), "added\n").unwrap();

        let tree = commit_working_tree("fold in my edits", &dir).unwrap();

        // The returned tree is exactly the working tree.
        assert_ne!(tree, head_tree);
        assert_eq!(
            capture_required("git", &["show", &format!("{tree}:file.txt")], &dir).unwrap(),
            "local edit"
        );
        assert_eq!(
            capture_required("git", &["show", &format!("{tree}:new.txt")], &dir).unwrap(),
            "added"
        );

        // The edits are now committed on HEAD, so the checkout is clean and a
        // later checkout of a new head would succeed.
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD^"], &dir).unwrap(),
            head
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD^{tree}"], &dir).unwrap(),
            tree
        );
        assert!(capture_required("git", &["status", "--porcelain=v1"], &dir)
            .unwrap()
            .is_empty());
        assert_eq!(
            capture_required("git", &["show", "-s", "--format=%s", "HEAD"], &dir).unwrap(),
            "fold in my edits"
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publish_branch_is_one_replaceable_snapshot_without_checkout_changes() {
        let dir = temp_repo("publish-test");
        let remote = dir.with_extension("remote.git");
        let remote_path = remote.to_string_lossy().to_string();
        capture_required("git", &["init", "--bare", "-q", &remote_path], &dir).unwrap();
        capture_required("git", &["remote", "add", "origin", &remote_path], &dir).unwrap();
        let base = commit_file(&dir, "base\n", "base");
        std::fs::write(dir.join("main.txt"), "new on main\n").unwrap();
        capture_required("git", &["add", "main.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "main advances"], &dir).unwrap();
        let main_tip = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();
        let git_dir = format!("--git-dir={remote_path}");
        capture_required(
            "git",
            &[&git_dir, "symbolic-ref", "HEAD", "refs/heads/main"],
            &dir,
        )
        .unwrap();
        let default_push = format!("{main_tip}:refs/heads/main");
        capture_required("git", &["push", "--quiet", "origin", &default_push], &dir).unwrap();
        assert_eq!(remote_default_branch(&dir).unwrap(), "main");
        assert_eq!(fetch_remote_branch_tip("main", &dir).unwrap(), main_tip);

        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        std::fs::write(dir.join("key.txt"), "temporary secret\n").unwrap();
        std::fs::write(dir.join("inherited.txt"), "from conversation A\n").unwrap();
        capture_required("git", &["add", "key.txt", "inherited.txt"], &dir).unwrap();
        let first_head = commit_file(&dir, "first result\n", "internal turn with key");
        let before = std::fs::read_to_string(dir.join("file.txt")).unwrap();
        let first_diff = WorkspaceDiff {
            base_commit: base.clone(),
            head: first_head.clone(),
            patch: "changed".to_string(),
        };

        let branch =
            prepare_publish_branch("publish-test", &first_diff, &main_tip, None, &dir).unwrap();
        assert_eq!(branch, "caos/publish-test");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            before
        );
        let first_publish =
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap();
        push_publish_branch(&branch, &dir).unwrap();
        let branch_ref = "refs/heads/caos/publish-test";
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(first_publish.as_str())
        );

        capture_required("git", &["rm", "-q", "key.txt"], &dir).unwrap();
        let final_head = commit_file(&dir, "final result\n", "internal turn without key");
        let final_diff = WorkspaceDiff {
            // This conversation was started from the previous conversation's
            // head. Publishing must not make that internal history reachable.
            base_commit: first_head.clone(),
            head: final_head.clone(),
            patch: "changed again".to_string(),
        };
        prepare_publish_branch("publish-test", &final_diff, &main_tip, None, &dir).unwrap();
        let final_publish =
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap();
        assert_ne!(final_publish, first_publish);
        assert_eq!(
            capture_required(
                "git",
                &["show", "-s", "--format=%s", "caos/publish-test"],
                &dir
            )
            .unwrap(),
            format!(
                "CAOS conversation {} at {}",
                short_hash("publish-test"),
                short_hash(&final_head)
            )
        );
        assert_eq!(
            capture_required("git", &["show", "caos/publish-test:file.txt"], &dir).unwrap(),
            "final result"
        );
        assert_eq!(
            capture_required("git", &["show", "caos/publish-test:main.txt"], &dir).unwrap(),
            "new on main"
        );
        assert_eq!(
            capture_required("git", &["show", "caos/publish-test:inherited.txt"], &dir).unwrap(),
            "from conversation A"
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test^"], &dir).unwrap(),
            main_tip
        );
        assert_eq!(
            capture_required(
                "git",
                &[
                    "rev-list",
                    "--count",
                    &format!("{main_tip}..caos/publish-test")
                ],
                &dir
            )
            .unwrap(),
            "1"
        );
        assert!(
            capture_optional("git", &["show", "caos/publish-test:key.txt"], &dir)
                .unwrap()
                .is_none()
        );
        assert!(!command_output(
            "git",
            &[
                "merge-base",
                "--is-ancestor",
                &first_publish,
                &final_publish
            ],
            &dir
        )
        .unwrap()
        .status
        .success());
        assert!(!command_output(
            "git",
            &["merge-base", "--is-ancestor", &first_head, &final_publish],
            &dir
        )
        .unwrap()
        .status
        .success());

        // A failed earlier publish can leave the local snapshot ahead of a
        // different remote tip. Lease against the freshly queried remote tip,
        // not the stale local branch that this call is about to push.
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(first_publish.as_str())
        );
        push_publish_branch(&branch, &dir).unwrap();
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(final_publish.as_str())
        );

        // An auto-deleted merged branch is recreated only if it is still
        // absent when the force-with-lease push reaches the remote.
        capture_required("git", &[&git_dir, "update-ref", "-d", branch_ref], &dir).unwrap();
        assert_eq!(remote_branch_tip(branch_ref, &dir).unwrap(), None);
        push_publish_branch(&branch, &dir).unwrap();
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(final_publish.as_str())
        );

        prepare_publish_branch("publish-test", &final_diff, &main_tip, None, &dir).unwrap();
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap(),
            final_publish
        );

        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn publish_refuses_conflicts_without_changing_the_checkout_or_branch() {
        let dir = temp_repo("publish-conflict-test");
        let base = commit_file(&dir, "base\n", "base");
        let conversation_head = commit_file(&dir, "conversation\n", "conversation turn");
        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        let main_tip = commit_file(&dir, "main\n", "main advances");

        let diff = WorkspaceDiff {
            base_commit: base,
            head: conversation_head,
            patch: "changed".to_string(),
        };
        let error =
            prepare_publish_branch("conflict-test", &diff, &main_tip, None, &dir).unwrap_err();

        assert!(error.contains("conflict with the selected PR base branch"));
        assert!(error.contains("file.txt"));
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            main_tip
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "main\n"
        );
        assert!(capture_optional(
            "git",
            &["rev-parse", "--verify", "refs/heads/caos/conflict-test"],
            &dir
        )
        .unwrap()
        .is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn non_default_publish_replays_only_the_child_conversation_delta() {
        let dir = temp_repo("publish-stacked-test");
        let base = commit_file(&dir, "base\n", "base");
        let parent_head = commit_file(&dir, "parent conversation\n", "parent turn");
        let child_head = commit_file(&dir, "child conversation\n", "child turn");

        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        std::fs::write(dir.join("upstream.txt"), "upstream\n").unwrap();
        capture_required("git", &["add", "upstream.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "base advances"], &dir).unwrap();
        let selected_base = commit_file(
            &dir,
            "parent conversation\n",
            "clean parent conversation snapshot",
        );

        // Merging both complete histories repeats the parent's edit and
        // conflicts with the child's edit to the same line.
        assert!(merge_publish_tree(&child_head, &selected_base, None, &dir).is_err());

        let diff = WorkspaceDiff {
            base_commit: parent_head.clone(),
            head: child_head,
            patch: "changed".to_string(),
        };
        prepare_publish_branch(
            "stacked-test",
            &diff,
            &selected_base,
            Some(&parent_head),
            &dir,
        )
        .unwrap();

        assert_eq!(
            capture_required("git", &["show", "caos/stacked-test:file.txt"], &dir).unwrap(),
            "child conversation"
        );
        assert_eq!(
            capture_required("git", &["show", "caos/stacked-test:upstream.txt"], &dir).unwrap(),
            "upstream"
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/stacked-test^"], &dir).unwrap(),
            selected_base
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remote_default_branch_comes_from_the_origin_head_symref() {
        assert_eq!(
            parse_remote_default_branch(
                "ref: refs/heads/release/next\tHEAD\n0123456789abcdef\tHEAD\n"
            )
            .unwrap(),
            "release/next"
        );
        assert!(parse_remote_default_branch("0123456789abcdef\tHEAD\n")
            .unwrap_err()
            .contains("did not advertise"));
        assert!(parse_remote_default_branch("ref: refs/tags/v1\tHEAD\n")
            .unwrap_err()
            .contains("outside refs/heads"));
    }

    #[test]
    fn github_repository_is_derived_from_origin_without_gh_repo_discovery() {
        let dir = temp_repo("github-origin-test");
        capture_required(
            "git",
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:Metta-AI/caos.git",
            ],
            &dir,
        )
        .unwrap();
        let repo = open_repo(&dir).unwrap();
        assert_eq!(github_repo_name(&repo).unwrap(), "Metta-AI/caos");
        assert_eq!(
            query_component("Metta-AI:caos/topic branch"),
            "Metta-AI%3Acaos%2Ftopic%20branch"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
