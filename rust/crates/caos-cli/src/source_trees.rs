//! Host conveniences for ordinary code references and stack directories.
use super::*;
pub use conversation_protocol::v3::source_trees::PublicationBase;
use conversation_protocol::v3::source_trees::{validate_repository, validate_source};
use conversation_protocol::v3::Mode;
use conversation_protocol::v3::SourceTreeConfig;

/// Resolve checkout defaults once, when code is attached to a conversation.
/// All later operations use the recorded repository and integration checkpoint.
pub fn checkout_config(t: &GitTransport, commit: &str) -> Result<SourceTreeConfig, String> {
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
    let mut config = SourceTreeConfig::default();
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
            config.upstream = Some(conversation_protocol::v3::SourceTreeBase::Branch {
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
    mut config: SourceTreeConfig,
    commit: &str,
) -> Result<SourceTreeConfig, String> {
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

pub fn create_from_source_tree(
    t: &GitTransport,
    id: &str,
    name: &str,
    source: &str,
) -> Result<String, String> {
    let store = open_store(t)?;
    let (refname, head) =
        fetch_validated_head(t, &store, id)?.ok_or_else(|| format!("no conversation {id:?}"))?;
    // Resolve once. A retry must not silently start from a newer source snapshot.
    let transitions = conversation_protocol::v3::source_trees::create_from_source_tree(
        &Conversation::open(&store, &head)?,
        name,
        source,
    )?;
    append_transition(t, id, &refname, "creating a source tree", |store, head| {
        let view = Conversation::open(store, head)?;
        if view.source_tree(name)?.is_some() {
            return Err(format!("source tree {name:?} already exists"));
        }
        Ok(Step::MintMany(transitions.clone()))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationTarget {
    pub source_tree: String,
    pub head: String,
    pub repository: String,
    pub branch: String,
    pub base: PublicationBase,
    pub previous_config: SourceTreeConfig,
    pub diagnostic: Option<String>,
}

pub fn repository_url(_t: &GitTransport, config: &SourceTreeConfig) -> Result<String, String> {
    let repository = match config
        .publication
        .as_ref()
        .and_then(|p| p.repository.clone())
        .or_else(|| config.repository())
    {
        Some(repository) => repository.clone(),
        None => return Err("add a .base-url file beside this reference before publishing".into()),
    };
    validate_repository(&repository)?;
    Ok(repository)
}

pub(super) fn publication_branch(
    _view: &Conversation<'_>,
    _id: &str,
    name: &str,
    _repository: &str,
) -> Result<String, String> {
    conversation_protocol::v3::source_trees::validate_branch(name)?;
    Ok(name.to_string())
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
    conversation_protocol::v3::source_trees::validate_branch(branch)?;
    let remote = GitStore::open(t.work_dir(), Some(repository))?;
    let commit = remote
        .read_ref(&format!("refs/heads/{branch}"))?
        .ok_or_else(|| format!("branch {branch:?} does not exist in {repository}"))?;
    remote.ensure_local(&commit)?;
    Ok(commit.to_string())
}

/// Read destinations without changing the conversation or any remote branch.
pub fn publication_plan(t: &GitTransport, id: &str) -> Result<Vec<PublicationTarget>, String> {
    use conversation_protocol::v3::SourceTreeBase;
    let store = open_store(t)?;
    let (_, head) =
        fetch_validated_head(t, &store, id)?.ok_or_else(|| format!("no conversation {id:?}"))?;
    let view = Conversation::open(&store, &head)?;
    view.source_tree_configs()?
        .into_iter()
        .filter(|(name, _)| {
            conversation_protocol::v3::source_trees::is_boundary(name.rsplit('/').next().unwrap())
        })
        .map(|(name, config)| {
            // Invalid directory conventions are diagnostics, never stored settings.
            let mut diagnostic = None;
            let repository = repository_url(t, &config).unwrap_or_else(|error| {
                diagnostic = Some(error);
                String::new()
            });
            let branch = normalize_repository_identity(&repository)
                .and_then(|identity| publication_branch(&view, id, &name, &identity))
                .unwrap_or_else(|error| {
                    diagnostic.get_or_insert(error);
                    String::new()
                });
            let base = config
                .publication
                .as_ref()
                .and_then(|p| p.base.clone())
                .unwrap_or_else(|| match &config.upstream {
                    Some(SourceTreeBase::Branch { name, .. }) => {
                        PublicationBase::Branch(name.clone())
                    }
                    Some(SourceTreeBase::SourceTree { name, .. }) => {
                        PublicationBase::SourceTree(name.clone())
                    }
                    None => PublicationBase::Default,
                });
            Ok(PublicationTarget {
                head: view
                    .source_tree(&name)?
                    .ok_or("source tree disappeared")?
                    .commit
                    .to_string(),
                source_tree: name,
                repository,
                branch,
                base,
                previous_config: config,
                diagnostic,
            })
        })
        .collect()
}

/// A reviewed destination with its PR base resolved once. Code tips are fetched
/// at execution, after any included parent has finished publishing.
#[derive(Clone, Debug)]
pub struct ResolvedPublicationTarget {
    pub target: PublicationTarget,
    pub base_branch: String,
}

/// Order only the selected source_trees. An excluded parent may already be
/// published; execution verifies that its published tip matches its code head.
pub fn publication_order(plan: &[PublicationTarget]) -> Result<Vec<String>, String> {
    let mut destinations = HashSet::new();
    let mut targets = BTreeMap::new();
    for target in plan {
        if target.repository.is_empty() || target.branch.is_empty() {
            return Err(format!(
                "source tree {:?}: {}",
                target.source_tree,
                target
                    .diagnostic
                    .as_deref()
                    .unwrap_or("choose a repository and publication branch")
            ));
        }
        conversation_protocol::v3::source_trees::validate_branch(&target.branch)?;
        let identity = normalize_repository_identity(&target.repository)?;
        if !destinations.insert((identity, &target.branch)) {
            return Err(format!(
                "several source trees target branch {:?}; give each source tree its own branch",
                target.branch
            ));
        }
        if targets
            .insert(target.source_tree.as_str(), target)
            .is_some()
        {
            return Err(format!("duplicate source tree {:?}", target.source_tree));
        }
    }
    // Paths in a directory already define the only supported stack order.
    Ok(targets.keys().map(|name| name.to_string()).collect())
}

pub fn resolve_publication_plan(
    t: &GitTransport,
    all: &[PublicationTarget],
    selected: &[PublicationTarget],
) -> Result<Vec<ResolvedPublicationTarget>, String> {
    let mut defaults = BTreeMap::new();
    publication_order(selected)?
        .into_iter()
        .map(|name| {
            let target = selected
                .iter()
                .find(|target| target.source_tree == name)
                .unwrap()
                .clone();
            let base_branch = match &target.base {
                PublicationBase::Branch(name) => name.clone(),
                PublicationBase::SourceTree(parent) => {
                    let parent = all
                        .iter()
                        .find(|target| &target.source_tree == parent)
                        .ok_or_else(|| format!("unknown base source tree {parent:?}"))?;
                    if normalize_repository_identity(&parent.repository)?
                        != normalize_repository_identity(&target.repository)?
                    {
                        return Err(format!(
                            "source tree {name:?} and its base belong to different repositories"
                        ));
                    }
                    parent.branch.clone()
                }
                PublicationBase::Default => {
                    if !defaults.contains_key(&target.repository) {
                        defaults.insert(
                            target.repository.clone(),
                            default_branch(t, &target.repository)?,
                        );
                    }
                    defaults[&target.repository].clone()
                }
            };
            conversation_protocol::v3::source_trees::validate_branch(&base_branch)?;
            if base_branch == target.branch {
                return Err(format!(
                    "source tree {name:?} cannot publish onto its PR base"
                ));
            }
            Ok(ResolvedPublicationTarget {
                target,
                base_branch,
            })
        })
        .collect()
}

/// Verify the reviewed directory entries, then publish the prepared code.
/// Later UI focus or metadata changes cannot retarget this publication.
pub fn publish_prepared_target(
    t: &GitTransport,
    id: &str,
    resolved: &ResolvedPublicationTarget,
    prepared_head: &str,
    base_commit: &str,
) -> Result<PublishedBranch, String> {
    let target = &resolved.target;
    let _ = oid(base_commit, "publication base")?;
    let store = open_store(t)?;
    let (_, head) = fetch_validated_head(t, &store, id)?.ok_or("conversation disappeared")?;
    let view = Conversation::open(&store, &head)?;
    if view
        .source_tree(&target.source_tree)?
        .is_none_or(|ws| ws.commit.as_str() != prepared_head)
        || view.source_tree_config(&target.source_tree)?.publication
            != target.previous_config.publication
    {
        return Err("stack changed after preparation; review it again".into());
    }
    if target.base.parent().is_some_and(|parent| {
        view.source_tree(parent)
            .ok()
            .flatten()
            .is_none_or(|ws| ws.commit.as_str() != base_commit)
    }) {
        return Err("preceding boundary changed during preparation; review it again".into());
    }
    if target.branch != target.source_tree {
        return Err("publication branch is the entry path; rename the entry to change it".into());
    }
    publish_source_tree_branch_inner(
        t,
        id,
        Some(&target.source_tree),
        Some(prepared_head),
        Some((&target.repository, &target.branch)),
    )
}

/// Incorporate a fetched base into every boundary atomically. Conflicts leave
/// the selected directory unchanged, so it never describes half an update.
pub fn update_stack(
    t: &GitTransport,
    id: &str,
    selection: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut store = open_store(t)?;
    let (refname, head) = fetch_validated_head(t, &store, id)?.ok_or("conversation disappeared")?;
    let view = Conversation::open(&store, &head)?;
    let names = view.source_tree_names()?;
    let directory = |name: &str| {
        name.rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default()
    };
    let selected = selection.map(|name| {
        if names.iter().any(|entry| entry == name) {
            directory(name)
        } else {
            name.trim_end_matches('/').to_string()
        }
    });
    let dirs: std::collections::BTreeSet<_> = names
        .iter()
        .map(|name| directory(name))
        .filter(|dir| selected.as_ref().is_none_or(|selected| selected == dir))
        .collect();
    let mut changed = Vec::new();
    let mut files = Vec::new();
    for dir in dirs {
        let join = |leaf: &str| {
            if dir.is_empty() {
                leaf.to_string()
            } else {
                format!("{dir}/{leaf}")
            }
        };
        let view = Conversation::open(&store, &head)?;
        let Some(bytes) = view.snapshot().read(&join(".base-url"))? else {
            continue;
        };
        let base = conversation_protocol::v3::source_trees::BaseUrl::parse(&bytes)?;
        let old = view
            .source_tree(&join("00-base"))?
            .ok_or("stack has no 00-base")?
            .commit;
        let next = oid(
            &branch_snapshot(t, &base.repository, &base.branch)?,
            "new base",
        )?;
        if old == next {
            continue;
        }
        ensure_code_commit(t, &mut store, &next)?;
        let mut previous_old = old;
        let mut previous_new = next.clone();
        for name in names.iter().filter(|name| {
            directory(name) == dir
                && (name.rsplit('/').next() == Some("dirty")
                    || conversation_protocol::v3::source_trees::is_boundary(
                        name.rsplit('/').next().unwrap(),
                    ))
        }) {
            let current = Conversation::open(&store, &head)?
                .source_tree(name)?
                .unwrap()
                .commit;
            ensure_code_commit(t, &mut store, &current)?;
            let signature = inherited_signature(&store, &head)?;
            let resolution = reconcile(
                &mut store,
                &previous_old,
                &previous_new,
                Some(&current),
                &signature,
            )?;
            if matches!(resolution, SourceTreeResolution::Conflict { .. }) {
                return Err(format!(
                    "{name} conflicts with the updated base; resolve it before updating the stack"
                ));
            }
            let output = resolution
                .new_pointer()
                .cloned()
                .unwrap_or_else(|| current.clone());
            ensure_code_commit(t, &mut store, &output)?;
            if output != current {
                files.push((name.clone(), Some((Mode::Commit, output.encode_line()))));
                changed.push(name.clone());
            }
            previous_old = current;
            previous_new = output;
        }
        files.push((join("00-base"), Some((Mode::Commit, next.encode_line()))));
    }
    if files.is_empty() {
        return Ok(changed);
    }
    append_transition(t, id, &refname, "updating code stack", |_store, current| {
        if current != &head {
            return Err("conversation changed; update the stack again".into());
        }
        Ok(Step::Mint(Transition::FilesApply {
            files: files.clone(),
        }))
    })?;
    Ok(changed)
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
    use conversation_protocol::v3::SourceTreeBase;
    paths::validate_source_tree_name(name)?;
    let name = format!("{name}/dirty");
    let name = name.as_str();
    validate_repository(repository)?;
    let mut config = SourceTreeConfig::default();
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
        config.upstream = Some(SourceTreeBase::Branch {
            repository: Some(repository.to_string()),
            name: branch.to_string(),
            commit: commit.clone(),
        });
        commit
    };
    if config.upstream.is_none() {
        config.upstream = Some(SourceTreeBase::Branch {
            repository: Some(repository.to_string()),
            name: default_branch(t, repository)?,
            commit: commit.clone(),
        });
    }
    ensure_code_commit(t, &mut open_store(t)?, &commit)?;
    reject_reserved_caos(t, commit.as_str(), "attached source tree")?;
    append_transition(
        t,
        id,
        &refs::head_ref(id)?,
        "attaching a repository",
        |store, head| {
            let view = Conversation::open(store, head)?;
            if let Some(existing) = view.source_tree(name)? {
                if existing.commit == commit && {
                    let mut existing_config = view.source_tree_config(name)?;
                    existing_config.publication = None;
                    existing_config == config
                } {
                    return Ok(Step::Done(head.to_string()));
                }
                return Err(format!("source tree {name:?} already exists"));
            }
            Ok(Step::MintMany(vec![
                Transition::reference(name.to_string(), Some(commit.clone())),
                Transition::stack_base(name.to_string(), config.clone()),
            ]))
        },
    )
}

/// Attach the pinned commit named by the same locator grammar as :@@=.
/// Do not evaluate or select a subtree: source trees preserve code ancestry.
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
/// Agent-created source trees in an attached repo inherit this request snapshot.
pub(super) fn snapshot_repository_refs(t: &GitTransport, head: &Oid) -> Result<String, String> {
    let store = open_store(t)?;
    let mut repositories: BTreeMap<String, (String, HashSet<String>)> = BTreeMap::new();
    for config in Conversation::open(&store, head)?
        .source_tree_configs()?
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
        if let Some(conversation_protocol::v3::SourceTreeBase::Branch { name, .. }) =
            config.upstream
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

/// Rename a commit-valued entry; all prior names/values remain in C history.
pub fn rename_reference(
    t: &GitTransport,
    id: &str,
    source: &str,
    destination: &str,
) -> Result<String, String> {
    paths::validate_source_tree_name(destination)?;
    let store = open_store(t)?;
    let (_, captured) = fetch_validated_head(t, &store, id)?.ok_or("conversation disappeared")?;
    let original = Conversation::open(&store, &captured)?
        .source_tree(source)?
        .ok_or("source is not a code reference")?
        .commit;
    append_transition(
        t,
        id,
        &refs::head_ref(id)?,
        "renaming code reference",
        |store, head| {
            let view = Conversation::open(store, head)?;
            if view
                .source_tree(source)?
                .is_none_or(|ws| ws.commit != original)
            {
                return Err("source changed; retry".into());
            }
            if view.snapshot().exists(destination)? {
                return Err("destination already exists".into());
            }
            Ok(Step::Mint(Transition::FilesApply {
                files: vec![
                    (source.to_string(), None),
                    (
                        destination.to_string(),
                        Some((Mode::Commit, original.encode_line())),
                    ),
                ],
            }))
        },
    )
}
