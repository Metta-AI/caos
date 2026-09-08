//! Workspace relationships and publication settings, stored with the conversation.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::canonical::{canonical_bytes, parse_canonical};
use super::{paths, Oid};

/// Source, integration progress, and publication policy are independent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<WorkspaceBase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<PublicationDestination>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationDestination {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<PublicationBase>,
}

/// A PR base stays typed until an execution plan resolves its destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "name",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PublicationBase {
    Default,
    Branch(String),
    Workspace(String),
}

impl PublicationBase {
    pub fn parent(&self) -> Option<&str> {
        match self {
            Self::Workspace(name) => Some(name),
            _ => None,
        }
    }
}

impl std::fmt::Display for PublicationBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str("repository default"),
            Self::Branch(name) => f.write_str(name),
            Self::Workspace(name) => write!(f, "@{name}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceBase {
    Branch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository: Option<String>,
        name: String,
        commit: Oid,
    },
    Workspace {
        name: String,
        commit: Oid,
    },
}

impl WorkspaceBase {
    pub fn name(&self) -> &str {
        match self {
            Self::Branch { name, .. } | Self::Workspace { name, .. } => name,
        }
    }
    pub fn commit(&self) -> &Oid {
        match self {
            Self::Branch { commit, .. } | Self::Workspace { commit, .. } => commit,
        }
    }
    pub fn with_commit(&self, commit: Oid) -> Self {
        match self {
            Self::Branch {
                repository, name, ..
            } => Self::Branch {
                repository: repository.clone(),
                name: name.clone(),
                commit,
            },
            Self::Workspace { name, .. } => Self::Workspace {
                name: name.clone(),
                commit,
            },
        }
    }
}

/// Make a pinned locator from Git's transport spelling. Parsing is shared with :@@=.
pub fn source_locator(repository: &str, commit: &Oid) -> Result<String, String> {
    validate_repository(repository)?;
    let url = if repository.starts_with("git+") || repository.starts_with("github:") {
        repository.to_string()
    } else if repository.contains("://") {
        format!("git+{repository}")
    } else if let Some((host, path)) = repository.split_once(':').filter(|(h, _)| !h.contains('/'))
    {
        format!("git+ssh://{host}/{path}")
    } else {
        format!("git+file://{repository}")
    };
    let source = format!("{url}?rev={commit}");
    validate_source(&source)?;
    Ok(source)
}

pub fn validate_source(source: &str) -> Result<git_locator::GitRef, String> {
    let parsed = git_locator::parse_git_ref(source)?;
    if parsed.rev.is_none() || parsed.dir.is_some() {
        return Err("workspace source must pin a whole Git commit (no path: or dir=)".into());
    }
    validate_repository(&parsed.fetch_url())?;
    Ok(parsed)
}

pub fn validate_repository(repository: &str) -> Result<(), String> {
    if repository.is_empty()
        || repository.starts_with('-')
        || repository.chars().any(char::is_control)
    {
        return Err("invalid workspace repository".into());
    }
    if repository.split_once("://").is_some_and(|(scheme, rest)| {
        rest.split('/')
            .next()
            .and_then(|host| host.rsplit_once('@'))
            .is_some_and(|(user, _)| scheme != "ssh" || user.contains(':'))
    }) {
        return Err("repository URLs must not contain credentials".into());
    }
    Ok(())
}

impl WorkspaceConfig {
    pub fn repository(&self) -> Option<String> {
        self.source
            .as_ref()
            .and_then(|s| validate_source(s).ok())
            .map(|s| s.fetch_url())
            .or_else(|| match &self.upstream {
                Some(WorkspaceBase::Branch { repository, .. }) => repository.clone(),
                _ => None,
            })
    }
    pub fn validate(&self) -> Result<(), String> {
        if let Some(source) = &self.source {
            validate_source(source)?;
        }
        if let Some(destination) = &self.publication {
            if let Some(repository) = &destination.repository {
                validate_repository(repository)?;
            }
            validate_branch(&destination.branch)?;
            match &destination.base {
                Some(PublicationBase::Branch(name)) => validate_branch(name)?,
                Some(PublicationBase::Workspace(name)) => paths::validate_workspace_name(name)?,
                _ => {}
            }
        }
        match &self.upstream {
            Some(WorkspaceBase::Branch {
                repository, name, ..
            }) => {
                if let Some(repository) = repository {
                    validate_repository(repository)?;
                }
                validate_branch(name)?;
            }
            Some(WorkspaceBase::Workspace { name, .. }) => paths::validate_workspace_name(name)?,
            None => {}
        }
        Ok(())
    }
    pub fn encode(&self) -> Vec<u8> {
        canonical_bytes(&serde_json::to_value(self).expect("workspace settings serialize"))
            .expect("workspace settings are canonical")
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let config: Self = serde_json::from_value(parse_canonical(bytes)?)
            .map_err(|error| format!("workspace settings: {error}"))?;
        config.validate()?;
        if config.encode() != bytes {
            return Err("workspace settings must omit absent values".into());
        }
        Ok(config)
    }
}

// Only used to read and replay historical config.json records.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<LegacyBase>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyBase {
    Branch { name: String, commit: Oid },
    Workspace { name: String, commit: Oid },
}

pub(crate) fn legacy_config(bytes: &[u8], initial: &Oid) -> Result<WorkspaceConfig, String> {
    let old: LegacyConfig =
        serde_json::from_value(parse_canonical(bytes)?).map_err(|e| e.to_string())?;
    if canonical_bytes(&serde_json::to_value(&old).map_err(|e| e.to_string())?)? != bytes {
        return Err("legacy settings must omit absent values".into());
    }
    let source = old
        .repository
        .as_ref()
        .map(|repository| source_locator(repository, initial))
        .transpose()?;
    let upstream = old.base.map(|base| match base {
        LegacyBase::Branch { name, commit } => WorkspaceBase::Branch {
            repository: old.repository.clone(),
            name,
            commit,
        },
        LegacyBase::Workspace { name, commit } => WorkspaceBase::Workspace { name, commit },
    });
    let publication = old.branch.map(|branch| PublicationDestination {
        repository: old.repository,
        branch,
        base: None,
    });
    let config = WorkspaceConfig {
        source,
        upstream,
        publication,
    };
    config.validate()?;
    Ok(config)
}

pub(crate) fn is_legacy_config(bytes: &[u8]) -> Result<bool, String> {
    let value = parse_canonical(bytes)?;
    Ok(["repository", "branch", "base"]
        .iter()
        .any(|key| value.get(key).is_some()))
}

pub fn validate_branch(branch: &str) -> Result<(), String> {
    if branch.is_empty()
        || branch == "@"
        || branch.starts_with('-')
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || branch
            .bytes()
            .any(|byte| byte <= b' ' || byte == 127 || b"~^:?*[\\".contains(&byte))
        || branch
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(format!("invalid branch name {branch:?}"));
    }
    Ok(())
}

/// Parents precede dependents. A stack is just these workspace base edges.
pub fn workspace_order(configs: &BTreeMap<String, WorkspaceConfig>) -> Result<Vec<String>, String> {
    fn visit(
        name: &str,
        configs: &BTreeMap<String, WorkspaceConfig>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        ordered: &mut Vec<String>,
    ) -> Result<(), String> {
        if visited.contains(name) {
            return Ok(());
        }
        if !visiting.insert(name.to_string()) {
            return Err(format!("workspace base cycle at {name:?}"));
        }
        let config = configs
            .get(name)
            .ok_or_else(|| format!("base workspace {name:?} does not exist"))?;
        config.validate()?;
        if let Some(WorkspaceBase::Workspace { name: parent, .. }) = &config.upstream {
            let parent_config = configs
                .get(parent)
                .ok_or_else(|| format!("base workspace {parent:?} does not exist"))?;
            if let (Some(repository), Some(parent_repository)) =
                (config.repository(), parent_config.repository())
            {
                if normalize_repository_identity(&repository)?
                    != normalize_repository_identity(&parent_repository)?
                {
                    return Err(format!(
                        "workspace {name:?} and its base {parent:?} use different repositories"
                    ));
                }
            }
            visit(parent, configs, visiting, visited, ordered)?;
        }
        visiting.remove(name);
        visited.insert(name.to_string());
        ordered.push(name.to_string());
        Ok(())
    }

    let mut ordered = Vec::new();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for name in configs.keys() {
        paths::validate_workspace_name(name)?;
        visit(name, configs, &mut visiting, &mut visited, &mut ordered)?;
    }
    Ok(ordered)
}

pub fn default_publication_branch(id: &str, name: &str, count: usize) -> String {
    if count == 1 {
        format!("caos/{id}")
    } else {
        format!("caos-workspaces/{id}/{name}")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Creation {
    #[default]
    FromUpstream,
    Stack,
    Copy,
}

/// Create a named line of work from one pinned workspace snapshot. The caller
/// publishes this sequence atomically with any associated operation receipt.
pub fn create_from_workspace(
    view: &super::view::Conversation<'_>,
    name: &str,
    source: &str,
    creation: Creation,
) -> Result<Vec<super::apply::Transition>, String> {
    use super::apply::Transition;
    paths::validate_workspace_name(name)?;
    if view.workspace(name)?.is_some() {
        return Err(format!("workspace {name:?} already exists"));
    }
    let ws = view
        .workspace(source)?
        .ok_or_else(|| format!("no workspace {source:?}"))?;
    let mut config = view.workspace_config(source)?;
    config.publication = None;
    let commit = match creation {
        Creation::FromUpstream => config
            .upstream
            .as_ref()
            .map(|base| base.commit().clone())
            .unwrap_or(ws.initial),
        Creation::Stack => {
            config.upstream = Some(WorkspaceBase::Workspace {
                name: source.to_string(),
                commit: ws.commit.clone(),
            });
            ws.commit
        }
        Creation::Copy => ws.commit,
    };
    let mut transitions = vec![Transition::WorkspaceCreate {
        name: name.to_string(),
        commit,
        origin: None,
    }];
    if config != WorkspaceConfig::default() {
        transitions.push(Transition::WorkspaceConfigure {
            name: name.to_string(),
            config,
        });
    }
    Ok(transitions)
}

pub fn normalize_repository_identity(url: &str) -> Result<String, String> {
    let url = url.trim();
    let url = url.strip_prefix("file://").unwrap_or(url);
    if url.is_empty() {
        return Err("origin has an empty URL".to_string());
    }
    let ssh;
    let url = if let Some(rest) = url.strip_prefix("ssh://git@") {
        ssh = format!("https://{rest}");
        &ssh
    } else {
        url
    };
    let mut normalized = if let Some(scp) = url.strip_prefix("git@") {
        let (host, path) = scp
            .split_once(':')
            .ok_or_else(|| format!("invalid origin URL {url:?}"))?;
        if host.is_empty() || path.is_empty() {
            return Err(format!("invalid origin URL {url:?}"));
        }
        format!("https://{host}/{path}")
    } else {
        url.to_string()
    };
    while normalized.ends_with('/') {
        normalized.pop();
    }
    if let Some(without_suffix) = normalized.strip_suffix(".git") {
        normalized = without_suffix.to_string();
    }
    while normalized.ends_with('/') {
        normalized.pop();
    }
    if let Some((scheme, rest)) = normalized.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() {
            return Err(format!("invalid origin URL {url:?}"));
        }
        let authority = match authority.rsplit_once('@') {
            Some((user, host)) => format!("{user}@{}", host.to_ascii_lowercase()),
            None => authority.to_ascii_lowercase(),
        };
        normalized = if path.is_empty() {
            format!("{scheme}://{authority}")
        } else {
            format!("{scheme}://{authority}/{path}")
        };
    }
    if normalized.is_empty() {
        return Err(format!("invalid origin URL {url:?}"));
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_publication_do_not_retarget_upstream() {
        let commit = super::super::oid::g3();
        let source = source_locator("git@example.com:team/repo.git", &commit).unwrap();
        assert_eq!(
            validate_source(&source).unwrap().rev.as_deref(),
            Some(commit.as_str())
        );
        for source in ["path:/tmp/repo".to_string(), format!("{source}&dir=src")] {
            assert!(validate_source(&source).is_err());
        }
        let mut config = WorkspaceConfig {
            source: Some(source),
            upstream: Some(WorkspaceBase::Branch {
                repository: Some("git@example.com:team/repo.git".into()),
                name: "main".into(),
                commit,
            }),
            publication: None,
        };
        let upstream = config.upstream.clone();
        config.publication = Some(PublicationDestination {
            repository: Some("https://example.com/fork".into()),
            branch: "review/topic".into(),
            base: Some(PublicationBase::Branch("release".into())),
        });
        let decoded = WorkspaceConfig::parse(&config.encode()).unwrap();
        assert_eq!(decoded.upstream, upstream);
        assert_eq!(decoded.source, config.source);
    }

    #[test]
    fn legacy_defaults_and_explicit_repositories_survive_conversion() {
        let commit = super::super::oid::g3();
        for repository in [None, Some("https://example.com/repo")] {
            let old = LegacyConfig {
                repository: repository.map(str::to_string),
                branch: Some("review/topic".into()),
                base: Some(LegacyBase::Branch {
                    name: "main".into(),
                    commit: commit.clone(),
                }),
            };
            let bytes = canonical_bytes(&serde_json::to_value(old).unwrap()).unwrap();
            let config = legacy_config(&bytes, &commit).unwrap();
            let publication = config.publication.as_ref().unwrap();
            assert_eq!(publication.branch, "review/topic");
            assert_eq!(publication.repository.as_deref(), repository);
            assert_eq!(config.upstream.as_ref().unwrap().name(), "main");
            assert_eq!(WorkspaceConfig::parse(&config.encode()).unwrap(), config);
        }
    }

    #[test]
    fn workspace_settings_validate_dependencies_and_round_trip() {
        let commit = super::super::oid::g3();
        let base = WorkspaceConfig {
            source: Some(source_locator("https://example.com/code.git", &commit).unwrap()),
            ..Default::default()
        };
        let mut child = base.clone();
        child.upstream = Some(WorkspaceBase::Workspace {
            name: "refactor".into(),
            commit: commit.clone(),
        });
        assert_eq!(WorkspaceConfig::parse(&child.encode()).unwrap(), child);
        let mut configs =
            BTreeMap::from([("feature".into(), child.clone()), ("refactor".into(), base)]);
        assert_eq!(
            workspace_order(&configs).unwrap(),
            vec!["refactor", "feature"]
        );
        configs.get_mut("refactor").unwrap().upstream = Some(WorkspaceBase::Workspace {
            name: "feature".into(),
            commit: commit.clone(),
        });
        assert!(workspace_order(&configs).unwrap_err().contains("cycle"));
        configs.remove("refactor");
        assert!(workspace_order(&configs)
            .unwrap_err()
            .contains("does not exist"));
        for branch in ["../main", "a.lock", "a//b", "-main", "a@{b", "a b"] {
            assert!(validate_branch(branch).is_err());
        }
        for branch in ["main", "caos/feature", "fix-one"] {
            validate_branch(branch).unwrap();
        }
        child.source = Some(format!(
            "git+https://user:password@example.com/repo?rev={commit}"
        ));
        assert!(child.validate().is_err());
    }
}
