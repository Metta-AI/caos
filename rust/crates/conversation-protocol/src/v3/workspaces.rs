//! Views derived from stack directory entries; none of these views are persisted.

use serde::{Deserialize, Serialize};

use super::{paths, Oid};

/// Resolved neighbors and publication destination for a commit-valued path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
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
        self.publication
            .as_ref()
            .and_then(|p| p.repository.clone())
            .or_else(|| match &self.upstream {
                Some(WorkspaceBase::Branch { repository, .. }) => repository.clone(),
                _ => None,
            })
    }
    pub fn validate(&self) -> Result<(), String> {
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
}

/// The sole external locator in a stack. Two lines avoid ambiguous URL fragments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseUrl {
    pub repository: String,
    pub branch: String,
}
impl BaseUrl {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
        let lines: Vec<_> = text.lines().collect();
        if lines.len() != 2 {
            return Err(".base-url needs a repository URL and branch on separate lines".into());
        }
        validate_repository(lines[0])?;
        validate_branch(lines[1])?;
        Ok(Self {
            repository: lines[0].into(),
            branch: lines[1].into(),
        })
    }
    pub fn encode(&self) -> Vec<u8> {
        format!("{}\n{}\n", self.repository, self.branch).into_bytes()
    }
}

pub fn is_boundary(name: &str) -> bool {
    name.len() > 3
        && name.as_bytes()[..2].iter().all(u8::is_ascii_digit)
        && name.as_bytes()[2] == b'-'
        && !name.starts_with("00-")
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

/// Create a named line of work from one pinned workspace snapshot. The caller
/// publishes this sequence atomically with any associated operation receipt.
pub fn create_from_workspace(
    view: &super::view::Conversation<'_>,
    name: &str,
    source: &str,
) -> Result<Vec<super::apply::Transition>, String> {
    paths::validate_workspace_name(name)?;
    if view.snapshot().exists(name)? {
        return Err(format!("path {name:?} already exists"));
    }
    let ws = view
        .workspace(source)?
        .ok_or_else(|| format!("no code reference {source:?}"))?;
    let commit = ws.commit;
    Ok(vec![super::apply::Transition::reference(
        name.to_string(),
        Some(commit),
    )])
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
