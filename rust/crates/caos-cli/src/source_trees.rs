//! Client imports and publication plans derived from directory contents.
use super::*;
use conversation_protocol::v3::source_trees::{is_boundary, validate_repository, validate_source};
use conversation_protocol::v3::{BaseUrl, Mode};
use std::collections::BTreeSet;

/// Transfer history between the trusted local checkout and client repository.
/// upload-pack otherwise forbids fetching promised objects from a partial clone.
/// Keep the override on this local import, never on arbitrary repository fetches.
pub fn import_local_commit(
    source: &std::path::Path,
    destination: &std::path::Path,
    commit: &Oid,
) -> Result<(), String> {
    if GitStore::open(destination, None)?.has_local(commit)? {
        return Ok(());
    }
    let source = source.canonicalize().map_err(|e| e.to_string())?;
    let output = std::process::Command::new("git")
        .current_dir(destination)
        .env("GIT_NO_LAZY_FETCH", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
        ])
        .arg(&source)
        .arg(commit.as_str())
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("starting local history import: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "importing local history: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Portable provenance only: local paths and credentials stay on the client.
fn source_metadata(
    name: &str,
    repository: &str,
    default_branch: Option<String>,
) -> Option<(String, Vec<u8>)> {
    let portable = ["https://", "http://", "ssh://", "git://"]
        .iter()
        .any(|scheme| repository.starts_with(scheme))
        || repository
            .strip_prefix("git@")
            .is_some_and(|rest| rest.contains(':'));
    if !portable || repository.contains(['?', '#']) || validate_repository(repository).is_err() {
        return None;
    }
    let mut value = serde_json::json!({"repository": repository});
    if let Some(branch) = default_branch {
        value["default_branch"] = branch.into();
    }
    let parent = name
        .rsplit_once('/')
        .map(|(parent, _)| format!("{parent}/"))
        .unwrap_or_default();
    Some((
        format!("{parent}.source.json"),
        format!("{value}\n").into_bytes(),
    ))
}

/// Read optional local Git metadata without contacting the remote.
pub fn local_import_metadata(
    name: &str,
    checkout: &std::path::Path,
) -> Result<Option<(String, Vec<u8>)>, String> {
    let optional = |args: &[&str]| -> Result<Option<String>, String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(checkout)
            .output()
            .map_err(|e| format!("reading import provenance: {e}"))?;
        match output.status.code() {
            Some(0) => String::from_utf8(output.stdout)
                .map(|s| Some(s.trim_end().to_string()))
                .map_err(|e| e.to_string()),
            Some(1) => Ok(None),
            _ => Err(format!(
                "reading import provenance: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        }
    };
    let Some(repository) = optional(&["config", "--get", "remote.origin.url"])? else {
        return Ok(None);
    };
    let branch =
        optional(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?.and_then(|reference| {
            reference
                .strip_prefix("refs/remotes/origin/")
                .map(str::to_string)
        });
    Ok(source_metadata(name, &repository, branch))
}

fn advertised_default_branch(t: &GitTransport, repository: &str) -> Result<Option<String>, String> {
    let output = t.git_capture(&["ls-remote", "--symref", "--", repository, "HEAD"], None)?;
    Ok(output.lines().find_map(|line| {
        let (reference, name) = line.strip_prefix("ref: ")?.split_once('\t')?;
        (name == "HEAD")
            .then(|| reference.strip_prefix("refs/heads/").map(str::to_string))
            .flatten()
    }))
}

pub fn default_branch(t: &GitTransport, repository: &str) -> Result<String, String> {
    advertised_default_branch(t, repository)?
        .ok_or_else(|| format!("repository {repository:?} did not advertise a default branch"))
}

pub fn branch_snapshot(t: &GitTransport, repository: &str, branch: &str) -> Result<String, String> {
    conversation_protocol::v3::source_trees::validate_branch(branch)?;
    let remote = GitStore::open(t.work_dir(), Some(repository))?;
    let commit = remote
        .read_ref(&format!("refs/heads/{branch}"))?
        .ok_or_else(|| format!("branch {branch:?} does not exist in {repository}"))?;
    remote.ensure_local(&commit)?;
    Ok(commit.to_string())
}

pub fn import_local_path(
    client: &GitTransport,
    path: &std::path::Path,
) -> Result<(Mode, Oid), String> {
    import_path(&mut open_store(client)?, client, path)
}

/// Snapshot disk content, independently of the source repository's index or HEAD.
/// Git enumerates paths using its ignore rules; the index here is always empty.
fn import_path(
    store: &mut GitStore,
    client: &GitTransport,
    path: &std::path::Path,
) -> Result<(Mode, Oid), String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let meta =
        std::fs::symlink_metadata(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    if meta.is_dir() {
        let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
        // Reuse source-local ignore policy, but never its index or tracked files.
        let source_repo = GitTransport::discover(path).ok();
        let git_dir = source_repo
            .as_ref()
            .unwrap_or(client)
            .git_capture(&["rev-parse", "--absolute-git-dir"], None)?;
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir.trim())
            .arg("--work-tree")
            .arg(path)
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .env("GIT_INDEX_FILE", temp.path().join("index"))
            .current_dir(path)
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "listing import: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let mut tree = conversation_protocol::v3::tree::TreeBuilder::from(None);
        for name in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            let name = std::str::from_utf8(name)
                .map_err(|_| "import paths must be UTF-8")?
                .trim_end_matches('/');
            let (mode, oid) = import_path(store, client, &path.join(name))?;
            tree.put_oid(name, mode, oid);
        }
        Ok((Mode::Tree, tree.build(store)?))
    } else {
        let (mode, bytes) = if meta.file_type().is_symlink() {
            (
                Mode::Link,
                std::fs::read_link(path)
                    .map_err(|e| e.to_string())?
                    .as_os_str()
                    .as_bytes()
                    .to_vec(),
            )
        } else if meta.is_file() {
            (
                if meta.permissions().mode() & 0o111 != 0 {
                    Mode::Executable
                } else {
                    Mode::Blob
                },
                std::fs::read(path).map_err(|e| e.to_string())?,
            )
        } else {
            return Err(format!("cannot import special file {}", path.display()));
        };
        Ok((mode, store.write_blob(&bytes)?))
    }
}

pub fn import_source(
    t: &GitTransport,
    id: &str,
    name: &str,
    repository: &str,
    revision: Option<&str>,
) -> Result<String, String> {
    paths::validate_source_tree_name(name)?;
    let parsed = if repository.starts_with("git+") || repository.starts_with("github:") {
        Some(validate_source(repository)?)
    } else {
        None
    };
    let repository = parsed
        .as_ref()
        .map(|p| p.fetch_url())
        .unwrap_or_else(|| repository.to_string());
    validate_repository(&repository)?;
    let local = std::path::Path::new(&repository);
    let (mode, object, metadata) = if revision.is_none() && std::fs::symlink_metadata(local).is_ok()
    {
        let source = std::path::absolute(local).map_err(|e| e.to_string())?;
        let (mode, object) = import_local_path(t, &source)?;
        let metadata = if source.is_dir() && GitTransport::discover(&source).is_ok() {
            local_import_metadata(name, &source)?
        } else {
            None
        };
        (mode, object, metadata)
    } else if local.is_dir() {
        let source = std::path::Path::new(&repository)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let revision = revision.unwrap_or("HEAD");
        let resolved = crate::host_git::capture_required(
            "git",
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{revision}^{{commit}}"),
            ],
            &source,
        )?;
        let commit = oid(&resolved, "local import")?;
        import_local_commit(&source, t.work_dir(), &commit)?;
        (Mode::Commit, commit, local_import_metadata(name, &source)?)
    } else {
        let default = advertised_default_branch(t, &repository)?;
        let reference = revision
            .map(str::to_string)
            .or_else(|| parsed.as_ref().and_then(|p| p.rev.clone()))
            .or_else(|| default.clone())
            .ok_or_else(|| {
                format!("repository {repository:?} did not advertise a default branch")
            })?;
        let commit = if let Ok(commit) = oid(&reference, "imported commit") {
            GitStore::open(t.work_dir(), Some(&repository))?.ensure_local(&commit)?;
            commit
        } else {
            oid(
                &branch_snapshot(
                    t,
                    &repository,
                    reference.strip_prefix("refs/heads/").unwrap_or(&reference),
                )?,
                "imported commit",
            )?
        };
        (
            Mode::Commit,
            commit,
            source_metadata(name, &repository, default),
        )
    };
    if mode == Mode::Commit {
        ensure_code_commit(t, &mut open_store(t)?, &object)?;
    }
    let bytes = if matches!(mode, Mode::Commit | Mode::Tree) {
        object.encode_line()
    } else {
        open_store(t)?.read_blob(&object)?
    };
    append_transition(
        t,
        id,
        &refs::head_ref(id)?,
        "importing content",
        |store, head| {
            let view = Conversation::open(store, head)?;
            let snapshot = view.snapshot();
            if snapshot.exists(name)? {
                return Err(format!("path {name:?} already exists"));
            }
            let mut files = vec![(name.to_string(), Some((mode, bytes.clone())))];
            if let Some((path, bytes)) = &metadata {
                if path == name
                    || (snapshot.exists(path)? && snapshot.read(path)?.as_ref() != Some(bytes))
                {
                    return Err(format!("import provenance {path:?} already exists with different content; choose a separate import folder"));
                }
                files.push((path.clone(), Some((Mode::Blob, bytes.clone()))));
            }
            Ok(Step::MintMany(vec![Transition::FilesApply { files }]))
        },
    )?;
    Ok(object.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationTarget {
    pub source_tree: String,
    pub head: String,
    pub repository: String,
    pub branch: String,
    pub base_branch: String,
    pub base_commit: Option<String>,
    pub parent: Option<String>,
    pub base_url: Option<BaseUrl>,
    pub remote_head: Option<String>,
    pub diagnostic: Option<String>,
}

pub fn publication_plan(t: &GitTransport, id: &str) -> Result<Vec<PublicationTarget>, String> {
    let store = open_store(t)?;
    let (_, head) = fetch_validated_head(t, &store, id)?.ok_or("conversation disappeared")?;
    let view = Conversation::open(&store, &head)?;
    let mut targets = Vec::new();
    for name in view.source_tree_names()? {
        if !is_boundary(name.rsplit('/').next().unwrap()) {
            continue;
        }
        let base_url = view.base_url(&name)?;
        let mut diagnostic = None;
        let (repository, mut base_branch) = match &base_url {
            Some(base) => (base.repository.clone(), base.branch.clone()),
            None => {
                diagnostic =
                    Some("add a valid .base-url beside this entry before publishing".into());
                (String::new(), String::new())
            }
        };
        let parent = view
            .previous_reference(&name)?
            .map(|(path, _)| path)
            .filter(|path| path.rsplit('/').next() != Some("00-base"));
        if let Some(parent) = &parent {
            base_branch = parent.clone();
        }
        let base_commit = if let Some(parent) = &parent {
            view.source_tree(parent)?
                .map(|entry| entry.commit.to_string())
        } else if diagnostic.is_none() {
            match branch_snapshot(t, &repository, &base_branch) {
                Ok(commit) => Some(commit),
                Err(error) => {
                    diagnostic = Some(error);
                    None
                }
            }
        } else {
            None
        };
        let remote_head = if diagnostic.is_none() {
            match GitStore::open(t.work_dir(), Some(&repository))
                .and_then(|remote| remote.read_ref(&format!("refs/heads/{name}")))
            {
                Ok(head) => head.map(|oid| oid.to_string()),
                Err(error) => {
                    diagnostic = Some(error);
                    None
                }
            }
        } else {
            None
        };
        targets.push(PublicationTarget {
            head: view
                .source_tree(&name)?
                .ok_or("entry disappeared")?
                .commit
                .to_string(),
            branch: name.clone(),
            source_tree: name,
            repository,
            base_branch,
            parent,
            base_url,
            base_commit,
            remote_head,
            diagnostic,
        });
    }
    Ok(targets)
}

pub fn publication_order(plan: &[PublicationTarget]) -> Result<Vec<String>, String> {
    let mut destinations = HashSet::new();
    let mut names = BTreeSet::new();
    for target in plan {
        if let Some(error) = &target.diagnostic {
            return Err(error.clone());
        }
        validate_repository(&target.repository)?;
        conversation_protocol::v3::source_trees::validate_branch(&target.branch)?;
        if !destinations.insert((
            normalize_repository_identity(&target.repository)?,
            &target.branch,
        )) {
            return Err(format!("duplicate publication branch {:?}", target.branch));
        }
        if !names.insert(target.source_tree.clone()) {
            return Err(format!("duplicate entry {:?}", target.source_tree));
        }
    }
    Ok(names.into_iter().collect())
}

pub fn publish_target(
    t: &GitTransport,
    id: &str,
    target: &PublicationTarget,
    base_commit: &str,
) -> Result<PublishedBranch, String> {
    if target.base_commit.as_deref() != Some(base_commit) {
        return Err("PR base changed since the preview; review again".into());
    }
    let store = open_store(t)?;
    let (_, head) = fetch_validated_head(t, &store, id)?.ok_or("conversation disappeared")?;
    let view = Conversation::open(&store, &head)?;
    if view
        .source_tree(&target.source_tree)?
        .is_none_or(|entry| entry.commit.as_str() != target.head)
        || view.base_url(&target.source_tree)? != target.base_url
        || view
            .previous_reference(&target.source_tree)?
            .map(|(path, _)| path)
            .filter(|path| path.rsplit('/').next() != Some("00-base"))
            != target.parent
    {
        return Err("publication contents changed since the preview; review again".into());
    }
    if let Some(parent) = &target.parent {
        if view
            .source_tree(parent)?
            .is_none_or(|entry| entry.commit.as_str() != base_commit)
        {
            return Err("preceding entry changed since the preview; review again".into());
        }
    }
    let remote = GitStore::open(t.work_dir(), Some(&target.repository))?;
    if remote
        .read_ref(&format!("refs/heads/{}", target.branch))?
        .map(|oid| oid.to_string())
        != target.remote_head
    {
        return Err("remote branch changed since the preview; review again".into());
    }
    publish_source_tree_branch_inner(
        t,
        id,
        Some(&target.source_tree),
        Some(&target.head),
        Some(target),
    )
}
