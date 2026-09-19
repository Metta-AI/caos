//! Stack updates are saved values. Git computes merges; this module sequences
//! them without moving the caller's source pointers or retaining a worktree.
#[cfg(feature = "git-cli")]
pub(crate) mod git;

use super::paths::validate_source_tree_name;
use super::{ObjectStore, Oid};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub path: String,
    /// The predecessor this layer was last based on. It remains pinned when
    /// the predecessor's gitlink advances independently.
    pub base: Oid,
    pub head: Oid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Rebase,
    Merge,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictStage {
    pub path: String,
    pub mode: String,
    pub oid: Oid,
    pub stage: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictMessage {
    pub paths: Vec<String>,
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeResult {
    pub tree: Oid,
    pub conflicted: bool,
    pub stages: Vec<ConflictStage>,
    pub messages: Vec<ConflictMessage>,
}

/// Keeps the complete conflict report, including conflicts without stage rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub original: Oid,
    pub result: MergeResult,
    pub draft: Oid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub method: Method,
    pub layers: Vec<Layer>,
    pub onto: Oid,
    pub todo: Vec<Vec<Oid>>,
    pub completed: Vec<Oid>,
    pub cursor: usize,
    pub current: Oid,
    pub pending: Option<Pending>,
}

pub trait StackStore: ObjectStore {
    fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool, String>;
    fn merge_objects(
        &mut self,
        base: Option<&Oid>,
        ours: &Oid,
        theirs: &Oid,
    ) -> Result<MergeResult, String>;
}

impl Operation {
    pub fn start(
        store: &dyn ObjectStore,
        layers: Vec<Layer>,
        onto: Oid,
        method: Method,
    ) -> Result<Self, String> {
        if layers.is_empty() {
            return Err("a stack needs at least one layer".into());
        }
        store.read_commit(&onto)?;
        let mut paths = std::collections::BTreeSet::new();
        let mut todo = Vec::new();
        for layer in &layers {
            validate_source_tree_name(&layer.path)?;
            if !paths.insert(layer.path.clone()) {
                return Err(format!("duplicate stack layer {}", layer.path));
            }
            store.read_commit(&layer.base)?;
            store.read_commit(&layer.head)?;
            let commits = match method {
                Method::Merge => vec![layer.head.clone()],
                Method::Rebase => {
                    let mut commits = Vec::new();
                    let mut head = layer.head.clone();
                    while head != layer.base {
                        let commit = store.read_commit(&head)?;
                        if commit.parents.len() != 1 {
                            return Err(format!(
                                "{} is not a linear history above {} (commit {}). Use merge updates for histories containing merges.",
                                layer.path, layer.base, head
                            ));
                        }
                        commits.push(head);
                        head = commit.parents[0].clone();
                    }
                    commits.reverse();
                    commits
                }
            };
            todo.push(commits);
        }
        Ok(Self {
            method,
            layers,
            onto: onto.clone(),
            todo,
            completed: Vec::new(),
            cursor: 0,
            current: onto,
            pending: None,
        })
    }

    pub fn is_complete(&self) -> bool {
        self.pending.is_none() && self.completed.len() == self.layers.len()
    }

    /// Runs until complete or until Git reports a conflict. All work can be
    /// repeated from this serialized value; inputs and commit metadata are pinned.
    pub fn advance(&mut self, store: &mut impl StackStore) -> Result<(), String> {
        self.validate()?;

        if self.pending.is_some() {
            return Err("resolve the pending draft before continuing".into());
        }
        while self.completed.len() < self.layers.len() {
            let layer = self.completed.len();
            if self.cursor == self.todo[layer].len() {
                self.completed.push(self.current.clone());
                self.cursor = 0;
                continue;
            }
            let original = self.todo[layer][self.cursor].clone();
            if self.method == Method::Merge {
                if store.is_ancestor(&self.current, &original)? {
                    self.current = original;
                    self.cursor += 1;
                    continue;
                }
                if store.is_ancestor(&original, &self.current)? {
                    self.cursor += 1;
                    continue;
                }
            }
            let commit = store.read_commit(&original)?;
            let base = match self.method {
                Method::Rebase => Some(
                    commit
                        .parents
                        .first()
                        .filter(|_| commit.parents.len() == 1)
                        .ok_or("saved rebase plan contains a non-linear commit")?,
                ),
                Method::Merge => None,
            };
            if base == Some(&self.current) || original == self.current {
                self.current = original;
                self.cursor += 1;
                continue;
            }
            let result = store.merge_objects(base, &self.current, &original)?;
            if result.conflicted {
                let mut draft = store.read_commit(&self.current)?;
                draft.tree = result.tree.clone();
                draft.parents = vec![self.current.clone()];
                draft.extra_headers.clear();
                draft.message =
                    format!("Resolve stack update for {}\n", self.layers[layer].path).into_bytes();
                let draft = store.write_commit(&draft)?;
                self.pending = Some(Pending {
                    original,
                    result,
                    draft,
                });
                return Ok(());
            }
            self.record(store, &original, &result.tree)?;
        }
        Ok(())
    }

    /// The caller supplies the resolved draft commit and explicitly acknowledges
    /// every conflict, including structural ones. Marker scanning is not proof.
    pub fn resume(&mut self, store: &mut impl StackStore, resolved: &Oid) -> Result<(), String> {
        self.validate()?;

        let pending = self.pending.as_ref().ok_or("no conflict is pending")?;
        let original = pending.original.clone();
        let tree = store.read_commit(resolved)?.tree;
        self.record(store, &original, &tree)?;
        self.pending = None;
        self.advance(store)
    }

    fn validate(&self) -> Result<(), String> {
        if self.todo.len() != self.layers.len() || self.completed.len() > self.layers.len() {
            return Err("invalid stack operation dimensions".into());
        }
        let layer = self.completed.len();
        if layer == self.layers.len() {
            if self.cursor != 0 || self.pending.is_some() {
                return Err("completed stack has pending work".into());
            }
        } else if self.cursor > self.todo[layer].len()
            || self
                .pending
                .as_ref()
                .is_some_and(|p| self.todo[layer].get(self.cursor) != Some(&p.original))
        {
            return Err("invalid stack operation cursor".into());
        }
        Ok(())
    }

    fn record(
        &mut self,
        store: &mut impl StackStore,
        original: &Oid,
        tree: &Oid,
    ) -> Result<(), String> {
        let mut commit = store.read_commit(original)?;
        commit.tree = tree.clone();
        commit.parents = match self.method {
            Method::Rebase => vec![self.current.clone()],
            Method::Merge => vec![original.clone(), self.current.clone()],
        };
        // A signature or mergetag authenticates the old commit, not this one.
        // Preserve non-signature metadata such as encoding.
        commit.extra_headers = unsigned_headers(&commit.extra_headers);
        if self.method == Method::Merge {
            commit.message = format!(
                "Merge updated stack base into {}\n",
                self.layers[self.completed.len()].path
            )
            .into_bytes();
        }
        self.current = store.write_commit(&commit)?;
        self.cursor += 1;
        Ok(())
    }
}

fn unsigned_headers(headers: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut dropping = false;
    for line in headers.split_inclusive(|b| *b == b'\n') {
        if !line.starts_with(b" ") {
            dropping = line.starts_with(b"gpgsig ")
                || line.starts_with(b"gpgsig-sha256 ")
                || line.starts_with(b"mergetag ");
        }
        if !dropping {
            output.extend_from_slice(line);
        }
    }
    output
}

#[cfg(test)]
mod tests;
