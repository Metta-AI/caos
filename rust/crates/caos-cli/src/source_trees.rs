//! Client imports and publication plans derived from directory contents.
use super::*;
use conversation_protocol::v3::source_trees::{is_boundary, validate_repository, validate_source};
use conversation_protocol::v3::BaseUrl;
use std::collections::BTreeSet;

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
    let reference = revision
        .map(str::to_string)
        .or_else(|| parsed.as_ref().and_then(|p| p.rev.clone()))
        .map(Ok)
        .unwrap_or_else(|| default_branch(t, &repository))?;
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
    ensure_code_commit(t, &mut open_store(t)?, &commit)?;
    append_transition(
        t,
        id,
        &refs::head_ref(id)?,
        "importing repository",
        |store, head| {
            if Conversation::open(store, head)?.snapshot().exists(name)? {
                return Err(format!("path {name:?} already exists"));
            }
            Ok(Step::MintMany(vec![Transition::reference(
                name.to_string(),
                Some(commit.clone()),
            )]))
        },
    )?;
    Ok(commit.to_string())
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
