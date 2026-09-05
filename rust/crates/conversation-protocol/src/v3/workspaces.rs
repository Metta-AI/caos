//! Workspace relationships and publication settings, stored with the conversation.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::canonical::{canonical_bytes, parse_canonical};
use super::{paths, Oid};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<WorkspaceBase>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceBase {
    Branch { name: String, commit: Oid },
    Workspace { name: String, commit: Oid },
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
            Self::Branch { name, .. } => Self::Branch {
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

impl WorkspaceConfig {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(repository) = &self.repository {
            if repository.is_empty()
                || repository.starts_with('-')
                || repository.chars().any(char::is_control)
            {
                return Err("invalid workspace repository".to_string());
            }
            // Credentials belong to the host's Git authentication, not the
            // conversation tree. SSH usernames (git@host:path) are fine.
            if repository.split_once("://").is_some_and(|(_, rest)| {
                rest.split('/')
                    .next()
                    .is_some_and(|host| host.contains('@'))
            }) {
                return Err("repository URLs must not contain credentials".to_string());
            }
        }
        if let Some(branch) = &self.branch {
            validate_branch(branch)?;
        }
        match &self.base {
            Some(WorkspaceBase::Branch { name, .. }) => validate_branch(name)?,
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
            return Err("workspace settings must omit absent values".to_string());
        }
        Ok(config)
    }
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
        if let Some(WorkspaceBase::Workspace { name: parent, .. }) = &config.base {
            let parent_config = configs
                .get(parent)
                .ok_or_else(|| format!("base workspace {parent:?} does not exist"))?;
            if let (Some(repository), Some(parent_repository)) =
                (&config.repository, &parent_config.repository)
            {
                if repository != parent_repository {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_settings_validate_dependencies_and_round_trip() {
        let commit = super::super::oid::g3();
        let base = WorkspaceConfig {
            repository: Some("https://example.com/code.git".into()),
            ..Default::default()
        };
        let mut child = base.clone();
        child.base = Some(WorkspaceBase::Workspace {
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
        configs.get_mut("refactor").unwrap().base = Some(WorkspaceBase::Workspace {
            name: "feature".into(),
            commit,
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
        child.repository = Some("https://user:password@example.com/repo".into());
        assert!(child.validate().is_err());
    }
}
