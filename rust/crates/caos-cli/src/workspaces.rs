//! Host operations on named workspaces. Settings and code move through the same lease.
use super::*;
use conversation_protocol::v3::WorkspaceConfig;

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
    stacked: bool,
) -> Result<String, String> {
    let store = open_store(t)?;
    let (refname, head) =
        fetch_validated_head(t, &store, id)?.ok_or_else(|| format!("no conversation {id:?}"))?;
    // Resolve once. A retry must not silently start from a newer source snapshot.
    let transitions = conversation_protocol::v3::workspaces::create_from_workspace(
        &Conversation::open(&store, &head)?,
        name,
        source,
        stacked,
    )?;
    append_transition(t, id, &refname, "creating a workspace", |store, head| {
        let view = Conversation::open(store, head)?;
        if view.workspace(name)?.is_some() {
            return Err(format!("workspace {name:?} already exists"));
        }
        Ok(Step::MintMany(transitions.clone()))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationTarget {
    pub workspace: String,
    pub head: String,
    pub repository: String,
    pub branch: String,
    pub base: String,
    pub parent: Option<String>,
    pub previous_config: WorkspaceConfig,
}

pub fn repository_url(t: &GitTransport, config: &WorkspaceConfig) -> Result<String, String> {
    let repository = match &config.repository {
        Some(repository) => repository.clone(),
        None => t
            .git_capture(&["remote", "get-url", "origin"], None)?
            .trim()
            .to_string(),
    };
    WorkspaceConfig {
        repository: Some(repository.clone()),
        ..Default::default()
    }
    .validate()?;
    Ok(repository)
}

pub(super) fn publication_branch(
    view: &Conversation<'_>,
    id: &str,
    name: &str,
    repository: &str,
) -> Result<String, String> {
    if let Some(branch) = view.workspace_config(name)?.branch {
        return Ok(branch);
    }
    let prior = view
        .publications()?
        .into_iter()
        .filter(|record| record.workspace_name == name && record.repository == repository)
        .map(|record| record.refname.trim_start_matches("refs/heads/").to_string())
        .collect::<std::collections::BTreeSet<_>>();
    match prior.len() {
        1 => Ok(prior.into_iter().next().unwrap()),
        0 if view.workspace_names()?.len() == 1 => Ok(format!("caos/{id}")),
        0 => Ok(format!("caos-workspaces/{id}/{name}")),
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
pub fn publication_plan(t: &GitTransport, id: &str) -> Result<Vec<PublicationTarget>, String> {
    use conversation_protocol::v3::{workspace_order, WorkspaceBase};
    let store = open_store(t)?;
    let (_, head) =
        fetch_validated_head(t, &store, id)?.ok_or_else(|| format!("no conversation {id:?}"))?;
    let view = Conversation::open(&store, &head)?;
    let configs = view.workspace_configs()?;
    let mut destinations = BTreeMap::new();
    for (name, config) in &configs {
        let repository = repository_url(t, config)?;
        let identity = normalize_repository_identity(&repository)?;
        let branch = publication_branch(&view, id, name, &identity)?;
        destinations.insert(name.clone(), (repository, branch));
    }
    let mut defaults: BTreeMap<String, String> = BTreeMap::new();
    let mut plan = Vec::new();
    for name in workspace_order(&configs)? {
        let config = configs[&name].clone();
        let (repository, branch) = destinations[&name].clone();
        let (base, parent) = match &config.base {
            Some(WorkspaceBase::Branch { name, .. }) => (name.clone(), None),
            Some(WorkspaceBase::Workspace { name: parent, .. }) => {
                if normalize_repository_identity(&destinations[parent].0)?
                    != normalize_repository_identity(&repository)?
                {
                    return Err(format!("workspace {name:?} and its base {parent:?} belong to different repositories"));
                }
                (destinations[parent].1.clone(), Some(parent.clone()))
            }
            None => {
                let value = match defaults.get(&repository) {
                    Some(value) => value.clone(),
                    None => {
                        let value = default_branch(t, &repository)?;
                        defaults.insert(repository.clone(), value.clone());
                        value
                    }
                };
                (value, None)
            }
        };
        plan.push(PublicationTarget {
            workspace: name.clone(),
            head: view
                .workspace(&name)?
                .ok_or("workspace disappeared")?
                .commit
                .to_string(),
            repository,
            branch,
            base,
            parent,
            previous_config: config,
        });
    }
    Ok(plan)
}

pub fn publication_order(plan: &[PublicationTarget]) -> Result<Vec<String>, String> {
    let mut configs = BTreeMap::new();
    let mut destinations = std::collections::BTreeSet::new();
    for target in plan {
        conversation_protocol::v3::workspaces::validate_branch(&target.branch)?;
        conversation_protocol::v3::workspaces::validate_branch(&target.base)?;
        if target.branch == target.base {
            return Err(format!(
                "workspace {:?} cannot publish onto its PR base",
                target.workspace
            ));
        }
        let head = oid(&target.head, "workspace head")?;
        configs.insert(
            target.workspace.clone(),
            WorkspaceConfig {
                repository: Some(normalize_repository_identity(&target.repository)?),
                branch: Some(target.branch.clone()),
                base: target.parent.as_ref().map(|parent| {
                    conversation_protocol::v3::WorkspaceBase::Workspace {
                        name: parent.clone(),
                        commit: head.clone(),
                    }
                }),
            },
        );
        if !destinations.insert((
            normalize_repository_identity(&target.repository)?,
            &target.branch,
        )) {
            return Err(format!(
                "several workspaces target branch {:?}; give each workspace its own branch",
                target.branch
            ));
        }
    }
    conversation_protocol::v3::workspace_order(&configs)
}

/// Persist exactly the reviewed destination, then publish the prepared code to it.
/// Later UI focus or metadata changes cannot retarget this publication.
pub fn publish_prepared_target(
    t: &GitTransport,
    id: &str,
    target: &PublicationTarget,
    prepared_head: &str,
    base_commit: &str,
) -> Result<PublishedBranch, String> {
    use conversation_protocol::v3::WorkspaceBase;
    let base_commit = oid(base_commit, "publication base")?;
    let config = WorkspaceConfig {
        repository: Some(target.repository.clone()),
        branch: Some(target.branch.clone()),
        base: Some(match &target.parent {
            Some(parent) => WorkspaceBase::Workspace {
                name: parent.clone(),
                commit: base_commit,
            },
            None => WorkspaceBase::Branch {
                name: target.base.clone(),
                commit: base_commit,
            },
        }),
    };
    append_transition(
        t,
        id,
        &refs::head_ref(id)?,
        "saving the publication target",
        |store, head| {
            let view = Conversation::open(store, head)?;
            if view
                .workspace(&target.workspace)?
                .is_none_or(|workspace| workspace.commit.as_str() != prepared_head)
            {
                return Err(format!(
                    "workspace {:?} changed after preparation; review it again",
                    target.workspace
                ));
            }
            let current = view.workspace_config(&target.workspace)?;
            if current == config {
                return Ok(Step::Done(head.to_string()));
            }
            if current != target.previous_config {
                return Err(format!("workspace {:?} settings changed after the publication preview; review them again", target.workspace));
            }
            Ok(Step::Mint(Transition::WorkspaceConfigure {
                name: target.workspace.clone(),
                config: config.clone(),
            }))
        },
    )?;
    publish_workspace_branch_inner(
        t,
        id,
        Some(&target.workspace),
        Some(prepared_head),
        Some((&target.repository, &target.branch)),
    )
}

// Update a connected stack, or every stack when selection is None. Each step
// snapshots its upstream once; retries reconcile against that same code.
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
        while let Some(WorkspaceBase::Workspace { name: parent, .. }) = &configs[name].base {
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
        let Some(base) = &config.base else { continue };
        let result = (|| {
            let source = match base {
                WorkspaceBase::Branch { name: branch, .. } => oid(
                    &branch_snapshot(t, &repository_url(t, config)?, branch)?,
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
            if source == *base.commit() {
                return Ok(false);
            }
            let mut next_config = config.clone();
            next_config.base = Some(base.with_commit(source.clone()));
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
    let mut config = WorkspaceConfig {
        repository: Some(repository.to_string()),
        ..Default::default()
    };
    config.validate()?;
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
        config.base = Some(WorkspaceBase::Branch {
            name: branch.to_string(),
            commit: commit.clone(),
        });
        commit
    };
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
                if existing.commit == commit
                    && existing.initial == commit
                    && view.workspace_config(name)? == config
                {
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

/// Capture only relevant named branch tips, grouped by repository identity.
/// Agent-created workspaces in an attached repo inherit this request snapshot.
pub(super) fn snapshot_repository_refs(t: &GitTransport, head: &Oid) -> Result<String, String> {
    let store = open_store(t)?;
    let mut repositories: BTreeMap<String, (String, HashSet<String>)> = BTreeMap::new();
    for config in Conversation::open(&store, head)?
        .workspace_configs()?
        .into_values()
    {
        let Some(repository) = config.repository else {
            continue;
        };
        let identity = normalize_repository_identity(&repository)?;
        let (_, branches) = repositories
            .entry(identity)
            .or_insert_with(|| (repository, HashSet::new()));
        branches.extend(MERGE_REF_CANDIDATES.iter().map(|name| name.to_string()));
        if let Some(conversation_protocol::v3::WorkspaceBase::Branch { name, .. }) = config.base {
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
