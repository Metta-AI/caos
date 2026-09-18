//! Client imports of explicitly selected commits.
use super::*;
use conversation_protocol::v3::source_trees::{validate_repository, validate_source};
use conversation_protocol::v3::Mode;

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
    Some((
        format!("{name}.source.json"),
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

pub struct ImportedContent {
    pub commit: Oid,
    pub metadata: Option<(String, Vec<u8>)>,
}

/// Resolve the same import semantics for the launcher and interactive client.
pub fn prepare_import(
    t: &GitTransport,
    name: &str,
    repository: &str,
    revision: Option<&str>,
) -> Result<ImportedContent, String> {
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
    let (commit, metadata) = if local.exists() {
        if !local.is_dir() {
            return Err("local imports require a Git checkout root".into());
        }
        let source = local.canonicalize().map_err(|e| e.to_string())?;
        // Validate this directory itself; discovering Git from a plain
        // subdirectory would silently import its enclosing repository.
        let git_dir = if source.join(".git").exists() {
            ".git"
        } else {
            "."
        };
        crate::host_git::capture_required(
            "git",
            &["rev-parse", "--resolve-git-dir", git_dir],
            &source,
        )
        .map_err(|_| "local imports require a Git checkout root or bare repository")?;
        if revision.is_none() {
            // Include staged edits, untracked files and dirty submodules even
            // when the checkout's status configuration normally hides them.
            let status = crate::host_git::capture_required(
                "git",
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--ignore-submodules=none",
                ],
                &source,
            )?;
            if !status.is_empty() {
                return Err("local import has uncommitted changes; commit them or supply an explicit revision (hash or ref)".into());
            }
        }
        let resolved = crate::host_git::capture_required(
            "git",
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{}^{{commit}}", revision.unwrap_or("HEAD")),
            ],
            &source,
        )?;
        let commit = oid(&resolved, "local import")?;
        import_local_commit(&source, t.work_dir(), &commit)?;
        (commit, local_import_metadata(name, &source)?)
    } else {
        let reference = revision
            .or_else(|| parsed.as_ref().and_then(|p| p.rev.as_deref()))
            .ok_or("Git URI imports require a full commit hash")?;
        let commit = oid(reference, "Git URI import requires a full commit hash")?;
        GitStore::open(t.work_dir(), Some(&repository))?.ensure_local(&commit)?;
        let default = advertised_default_branch(t, &repository)?;
        (commit, source_metadata(name, &repository, default))
    };
    ensure_code_commit(t, &mut open_store(t)?, &commit)?;
    Ok(ImportedContent { commit, metadata })
}

pub fn import_source(
    t: &GitTransport,
    id: &str,
    name: &str,
    repository: &str,
    revision: Option<&str>,
) -> Result<String, String> {
    let ImportedContent { commit, metadata } = prepare_import(t, name, repository, revision)?;
    let bytes = commit.encode_line();
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
            let mut files = vec![(name.to_string(), Some((Mode::Commit, bytes.clone())))];
            if let Some((path, bytes)) = &metadata {
                if snapshot.exists(path)? && snapshot.read(path)?.as_ref() != Some(bytes) {
                    return Err(format!("import provenance {path:?} already exists with different content; choose another import path"));
                }
                files.push((path.clone(), Some((Mode::Blob, bytes.clone()))));
            }
            Ok(Step::MintMany(vec![
                Transition::FilesApply { files },
                Transition::MessageAppend {
                    entry: system_entry(
                        id,
                        format!("import-{head}"),
                        format!("Imported at {name}: {commit}"),
                    ),
                    payloads: Vec::new(),
                },
            ]))
        },
    )?;
    Ok(commit.to_string())
}
