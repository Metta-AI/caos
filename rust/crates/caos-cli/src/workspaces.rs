//! Host operations on named workspaces. Settings and code move through the same lease.
use super::*;
use conversation_protocol::v3::workspaces::{source_locator, validate_repository, validate_source};
pub use conversation_protocol::v3::workspaces::{Creation, PublicationBase};
use conversation_protocol::v3::WorkspaceConfig;

/// Resolve checkout defaults once, when code is attached to a conversation.
/// All later operations use the recorded repository and integration checkpoint.
pub fn checkout_config(t: &GitTransport, commit: &str) -> Result<WorkspaceConfig, String> {
    let origin = t.git_capture(&["remote", "get-url", "origin"], None).ok();
    let repository = origin
        .as_deref()
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(|| t.work_dir().to_string_lossy().into_owned());
    let path = t.work_dir().join(&repository);
    let repository = if path.exists() {
        path.canonicalize()
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .into_owned()
    } else {
        repository
    };
    let mut config = WorkspaceConfig {
        source: Some(source_locator(&repository, &oid(commit, "source commit")?)?),
        ..Default::default()
    };
    let mut candidates = Vec::new();
    if let Ok(reference) = t.git_capture(
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        None,
    ) {
        candidates.push(reference.trim().to_string());
    }
    if let Ok(reference) =
        t.git_capture(&["rev-parse", "--symbolic-full-name", "@{upstream}"], None)
    {
        candidates.push(reference.trim().to_string());
    }
    candidates.extend([
        "refs/remotes/origin/main".into(),
        "refs/remotes/origin/master".into(),
    ]);
    if origin.is_none() {
        if let Ok(reference) = t.git_capture(&["symbolic-ref", "--quiet", "HEAD"], None) {
            candidates.push(reference.trim().to_string());
        }
    }
    for reference in candidates {
        let Some(name) = reference
            .strip_prefix("refs/remotes/origin/")
            .or_else(|| reference.strip_prefix("refs/heads/"))
        else {
            continue;
        };
        if let Ok(base) = t.git_capture(&["merge-base", commit, &reference], None) {
            config.upstream = Some(conversation_protocol::v3::WorkspaceBase::Branch {
                repository: Some(repository.clone()),
                name: name.into(),
                commit: oid(base.trim(), "checkout base")?,
            });
            break;
        }
    }
    config.validate()?;
    Ok(config)
}

/// Keep the chosen upstream, but pin only the ancestry present in this snapshot.
pub fn config_at_commit(
    t: &GitTransport,
    mut config: WorkspaceConfig,
    commit: &str,
) -> Result<WorkspaceConfig, String> {
    if let Some(upstream) = &config.upstream {
        let base = t
            .git_capture(
                &["merge-base", "--all", commit, upstream.commit().as_str()],
                None,
            )
            .map_err(|error| {
                format!("revision has no shared history with the selected upstream: {error}")
            })?;
        if base.lines().count() != 1 {
            return Err("revision has multiple merge bases with the selected upstream".into());
        }
        config.upstream = Some(upstream.with_commit(oid(base.trim(), "snapshot checkpoint")?));
    }
    config.publication = None;
    Ok(config)
}

pub fn configure(
    t: &GitTransport,
    id: &str,
    name: &str,
    config: WorkspaceConfig,
) -> Result<String, String> {
    let refname = refs::head_ref(id)?;
    let mut preimage = None;
    append_transition(t, id, &refname, "configuring a workspace", |store, head| {
        let current = Conversation::open(store, head)?.workspace_config(name)?;
        if current == config {
            return Ok(Step::Done(head.to_string()));
        }
        if preimage.as_ref().is_some_and(|before| before != &current) {
            return Err(format!(
                "workspace {name:?} settings changed; reload before editing them"
            ));
        }
        preimage = Some(current);
        Ok(Step::Mint(Transition::WorkspaceConfigure {
            name: name.to_string(),
            config: config.clone(),
        }))
    })
}

pub fn create_from_workspace(
    t: &GitTransport,
    id: &str,
    name: &str,
    source: &str,
    creation: Creation,
) -> Result<String, String> {
    let store = open_store(t)?;
    let (refname, head) =
        fetch_validated_head(t, &store, id)?.ok_or_else(|| format!("no conversation {id:?}"))?;
    // Resolve once. A retry must not silently start from a newer source snapshot.
    let transitions = conversation_protocol::v3::workspaces::create_from_workspace(
        &Conversation::open(&store, &head)?,
        name,
        source,
        creation,
    )?;
    append_transition(t, id, &refname, "creating a workspace", |store, head| {
        let view = Conversation::open(store, head)?;
        if view.workspace(name)?.is_some() {
            return Err(format!("workspace {name:?} already exists"));
        }
        Ok(Step::MintMany(transitions.clone()))
    })
}

pub fn repository_url(t: &GitTransport, config: &WorkspaceConfig) -> Result<String, String> {
    let repository = match config
        .publication
        .as_ref()
        .and_then(|p| p.repository.clone())
        .or_else(|| config.repository())
    {
        Some(repository) => repository.clone(),
        None => default_repository(t)?,
    };
    validate_repository(&repository)?;
    Ok(repository)
}

fn default_repository(t: &GitTransport) -> Result<String, String> {
    let checkout = git_config_value(t, "caos.checkout");
    let source = checkout.as_ref().map(GitTransport::discover).transpose()?;
    Ok(source
        .as_ref()
        .unwrap_or(t)
        .git_capture(&["remote", "get-url", "origin"], None)?
        .trim()
        .to_string())
}

pub(super) use conversation_protocol::v3::workspaces::default_publication_branch as generated_branch;

pub(super) fn publication_branch(
    view: &Conversation<'_>,
    id: &str,
    name: &str,
    repository: &str,
) -> Result<String, String> {
    if let Some(destination) = view.workspace_config(name)?.publication {
        return Ok(destination.branch);
    }
    if matches!(view.identity()?.kind, IdentityKind::Fork { .. }) {
        return Ok(generated_branch(id, name, view.workspace_names()?.len()));
    }
    let prior = view
        .publications()?
        .into_iter()
        .filter(|record| record.workspace_name == name && record.repository == repository)
        .map(|record| record.refname.trim_start_matches("refs/heads/").to_string())
        .collect::<std::collections::BTreeSet<_>>();
    match prior.len() {
        1 => Ok(prior.into_iter().next().unwrap()),
        0 => Ok(generated_branch(id, name, view.workspace_names()?.len())) ,
        _ => Err(format!("workspace {name:?} has several published branches; choose its publication branch explicitly")),
    }
}

pub fn default_branch(t: &GitTransport, repository: &str) -> Result<String, String> {
    let output = t.git_capture(&["ls-remote", "--symref", "--", repository, "HEAD"], None)?;
    output
        .lines()
        .find_map(|line| {
            let (reference, name) = line.strip_prefix("ref: ")?.split_once('\t')?;
            (name == "HEAD")
                .then(|| reference.strip_prefix("refs/heads/").map(str::to_string))
                .flatten()
        })
        .ok_or_else(|| format!("repository {repository:?} did not advertise a default branch"))
}

pub fn branch_snapshot(t: &GitTransport, repository: &str, branch: &str) -> Result<String, String> {
    conversation_protocol::v3::workspaces::validate_branch(branch)?;
    let remote = GitStore::open(t.work_dir(), Some(repository))?;
    let commit = remote
        .read_ref(&format!("refs/heads/{branch}"))?
        .ok_or_else(|| format!("branch {branch:?} does not exist in {repository}"))?;
    remote.ensure_local(&commit)?;
    Ok(commit.to_string())
}

/// Read destinations without changing the conversation or any remote branch.
pub fn update_stack(
    t: &GitTransport,
    id: &str,
    selection: Option<&str>,
) -> Result<Vec<String>, String> {
    use conversation_protocol::v3::{workspace_order, WorkspaceBase};
    let store = open_store(t)?;
    let (refname, head) =
        fetch_validated_head(t, &store, id)?.ok_or_else(|| format!("no conversation {id:?}"))?;
    let configs = Conversation::open(&store, &head)?.workspace_configs()?;
    let order = workspace_order(&configs)?;
    fn root<'a>(mut name: &'a str, configs: &'a BTreeMap<String, WorkspaceConfig>) -> &'a str {
        while let Some(WorkspaceBase::Workspace { name: parent, .. }) = &configs[name].upstream {
            name = parent;
        }
        name
    }
    if selection.is_some_and(|name| !configs.contains_key(name)) {
        return Err("selected workspace no longer exists".into());
    }
    let selected_root = selection.map(|name| root(name, &configs));
    let mut updated = Vec::new();
    for name in order {
        if selected_root.is_some_and(|selected| root(&name, &configs) != selected) {
            continue;
        }
        let config = &configs[&name];
        let Some(base) = &config.upstream else {
            continue;
        };
        let result = (|| {
            let source = match base {
                WorkspaceBase::Branch {
                    repository,
                    name: branch,
                    ..
                } => oid(
                    &branch_snapshot(
                        t,
                        &repository
                            .clone()
                            .map(Ok)
                            .unwrap_or_else(|| default_repository(t))?,
                        branch,
                    )?,
                    "upstream",
                )?,
                WorkspaceBase::Workspace { name: parent, .. } => {
                    let (_, head) =
                        fetch_validated_head(t, &store, id)?.ok_or("conversation disappeared")?;
                    Conversation::open(&store, &head)?
                        .workspace(parent)?
                        .ok_or("base workspace disappeared")?
                        .commit
                }
            };
            let (_, latest) =
                fetch_validated_head(t, &store, id)?.ok_or("conversation disappeared")?;
            let current = Conversation::open(&store, &latest)?
                .workspace(&name)?
                .ok_or("workspace disappeared")?
                .commit;
            if !conversation_protocol::v3::CodeOps::is_ancestor(&store, base.commit(), &current)? {
                return Err(format!("workspace {name:?} predates its upstream checkpoint; roll back to this commit again to restore its checkpoint"));
            }
            if source == *base.commit() {
                return Ok(false);
            }
            let mut next_config = config.clone();
            next_config.upstream = Some(base.with_commit(source.clone()));
            append_transition(
                t,
                id,
                &refname,
                "updating the workspace stack",
                |store, head| {
                    let view = Conversation::open(store, head)?;
                    let current_config = view.workspace_config(&name)?;
                    if current_config == next_config {
                        return Ok(Step::Done(head.to_string()));
                    }
                    if current_config != *config {
                        return Err(format!(
                            "workspace {name:?} settings changed; update the stack again"
                        ));
                    }
                    let current = view
                        .workspace(&name)?
                        .ok_or("workspace disappeared")?
                        .commit;
                    ensure_code_commit(t, store, &source)?;
                    store.ensure_local(&current)?;
                    store.ensure_local(base.commit())?;
                    let signature = inherited_signature(store, head)?;
                    let resolution =
                        reconcile(store, base.commit(), &source, Some(&current), &signature)?;
                    if let WorkspaceResolution::Conflict { merge, .. } = &resolution {
                        let paths = merge
                            .as_ref()
                            .and_then(|m| m.conflict_paths.as_ref())
                            .map(|paths| paths.join(", "))
                            .unwrap_or_default();
                        return Err(format!("workspace {name:?} conflicts with {} at {source}: {paths}. Resolve using merge in this workspace, then run Update stack again; its head and base pin are unchanged.", base.name()));
                    }
                    let mut transitions = Vec::new();
                    if let Some(output) = resolution.new_pointer() {
                        ensure_code_commit(t, store, output)?;
                        transitions.push(Transition::WorkspaceAdvance {
                            name: name.clone(),
                            commit: output.clone(),
                        });
                    }
                    transitions.push(Transition::WorkspaceConfigure {
                        name: name.clone(),
                        config: next_config.clone(),
                    });
                    Ok(Step::MintMany(transitions))
                },
            )?;
            Ok(true)
        })();
        match result {
            Ok(true) => updated.push(name),
            Ok(false) => {}
            Err(error) => {
                return Err(if updated.is_empty() {
                    error
                } else {
                    format!("Updated {}. Stopped: {error}", updated.join(", "))
                })
            }
        }
    }
    Ok(updated)
}

/// Import a repository snapshot, preserving its transport URL for Git auth.
/// Full hashes are pinned attachments; branch names also establish an update base.
pub fn attach(
    t: &GitTransport,
    id: &str,
    name: &str,
    repository: &str,
    revision: Option<&str>,
) -> Result<String, String> {
    use conversation_protocol::v3::WorkspaceBase;
    paths::validate_workspace_name(name)?;
    validate_repository(repository)?;
    let mut config = WorkspaceConfig::default();
    let reference = match revision {
        Some(reference) => reference.to_string(),
        None => default_branch(t, repository)?,
    };
    let commit = if let Ok(commit) = oid(&reference, "attachment commit") {
        GitStore::open(t.work_dir(), Some(repository))?.ensure_local(&commit)?;
        commit
    } else {
        let branch = reference.strip_prefix("refs/heads/").unwrap_or(&reference);
        let commit = oid(
            &branch_snapshot(t, repository, branch)?,
            "attachment commit",
        )?;
        config.upstream = Some(WorkspaceBase::Branch {
            repository: Some(repository.to_string()),
            name: branch.to_string(),
            commit: commit.clone(),
        });
        commit
    };
    config.source = Some(source_locator(repository, &commit)?);
    ensure_code_commit(t, &mut open_store(t)?, &commit)?;
    reject_reserved_caos(t, commit.as_str(), "attached workspace")?;
    append_transition(
        t,
        id,
        &refs::head_ref(id)?,
        "attaching a repository",
        |store, head| {
            let view = Conversation::open(store, head)?;
            if let Some(existing) = view.workspace(name)? {
                if existing.commit == commit && existing.initial == commit && {
                    let mut existing_config = view.workspace_config(name)?;
                    existing_config.publication = None;
                    existing_config == config
                } {
                    return Ok(Step::Done(head.to_string()));
                }
                return Err(format!("workspace {name:?} already exists"));
            }
            Ok(Step::MintMany(vec![
                Transition::WorkspaceCreate {
                    name: name.to_string(),
                    commit: commit.clone(),
                    origin: None,
                },
                Transition::WorkspaceConfigure {
                    name: name.to_string(),
                    config: config.clone(),
                },
            ]))
        },
    )
}

/// Attach the pinned commit named by the same locator grammar as :@@=.
/// Do not evaluate or select a subtree: workspaces preserve code ancestry.
pub fn attach_source(
    t: &GitTransport,
    id: &str,
    name: &str,
    source: &str,
) -> Result<String, String> {
    let parsed = validate_source(source)?;
    attach(t, id, name, &parsed.fetch_url(), parsed.rev.as_deref())
}

/// Capture only relevant named branch tips, grouped by repository identity.
/// Agent-created workspaces in an attached repo inherit this request snapshot.
pub(super) fn snapshot_repository_refs(t: &GitTransport, head: &Oid) -> Result<String, String> {
    let store = open_store(t)?;
    let mut repositories: BTreeMap<String, (String, HashSet<String>)> = BTreeMap::new();
    for config in Conversation::open(&store, head)?
        .workspace_configs()?
        .into_values()
    {
        let Some(repository) = config.repository() else {
            continue;
        };
        let identity = normalize_repository_identity(&repository)?;
        let (_, branches) = repositories
            .entry(identity)
            .or_insert_with(|| (repository, HashSet::new()));
        branches.extend(MERGE_REF_CANDIDATES.iter().map(|name| name.to_string()));
        if let Some(conversation_protocol::v3::WorkspaceBase::Branch { name, .. }) = config.upstream
        {
            branches.insert(name);
        }
    }
    let mut snapshots = BTreeMap::new();
    for (identity, (repository, branches)) in repositories {
        let output = t.git_capture(&["ls-remote", "--heads", "--", &repository], None)?;
        let remote = GitStore::open(t.work_dir(), Some(&repository))?;
        let mut refs = String::new();
        for line in output.lines() {
            let Some((hash, reference)) = line.split_once('\t') else {
                continue;
            };
            let Some(name) = reference.strip_prefix("refs/heads/") else {
                continue;
            };
            if !branches.contains(name) {
                continue;
            }
            let commit = oid(hash, "repository ref")?;
            remote.ensure_local(&commit)?;
            t.ensure_pushed(hash)?;
            refs.push_str(&format!(
                "{name} {hash}
origin/{name} {hash}
"
            ));
        }
        snapshots.insert(identity, refs);
    }
    serde_json::to_string(&snapshots).map_err(|error| error.to_string())
}
