use super::events::Event;
use std::cell::OnceCell;
use std::collections::BTreeMap;

use super::kinds::Kind;
use super::oid::Oid;
use super::paths;
use super::records::{
    parse_title, AsyncRecord, CallRecord, ChildRecord, Identity, PublicationRecord,
    SourceTreeRecord, TranscriptEntry, TurnRecord,
};
use super::tree::{Mode, ObjectStore, Snapshot, TreeEntry};

/// Latest records, reconstructed newest-first once for this immutable commit.
/// The context flag distinguishes this conversation's execution from inherited
/// results that remain readable in a fork's transcript.
#[derive(Default)]
struct Execution {
    turns: BTreeMap<Oid, (bool, TurnRecord)>,
    latest_turn: Option<Oid>,
    tools: BTreeMap<(Oid, u64), BTreeMap<String, CallRecord>>,
    children: BTreeMap<String, (bool, ChildRecord)>,
    tasks: BTreeMap<Oid, AsyncRecord>,
    publications: BTreeMap<String, PublicationRecord>,
    publication_order: Vec<String>,
    payloads: BTreeMap<String, Vec<u8>>,
    fork_source: Option<Oid>,
}

pub struct Conversation<'s> {
    store: &'s dyn ObjectStore,
    execution: OnceCell<Execution>,
    commit: Option<Oid>,
    parent: Option<Oid>,
    kind: Option<Kind>,
    tree: Oid,
    snapshot: Snapshot<'s>,
}

impl<'s> Conversation<'s> {
    pub fn open(store: &'s dyn ObjectStore, commit: &Oid) -> Result<Conversation<'s>, String> {
        let info = store.read_commit(commit).map_err(String::from)?;
        if info.parents.len() != 1 {
            return Err(format!(
                "conversation commit {commit} must have exactly one parent"
            ));
        }
        let kind = Kind::parse_message(&info.message)?;
        let conversation = Conversation {
            store,
            execution: OnceCell::new(),
            commit: Some(commit.clone()),
            parent: info.parents.first().cloned(),
            kind: Some(kind),
            tree: info.tree.clone(),
            snapshot: Snapshot::new(store, info.tree),
        };
        conversation.require_format()?;
        Ok(conversation)
    }

    pub fn open_tree(store: &'s dyn ObjectStore, tree: &Oid) -> Result<Conversation<'s>, String> {
        let conversation = Conversation {
            store,
            execution: OnceCell::new(),
            commit: None,
            parent: None,
            kind: None,
            tree: tree.clone(),
            snapshot: Snapshot::new(store, tree.clone()),
        };
        conversation.require_format()?;
        Ok(conversation)
    }

    pub(crate) fn current_events(&self) -> Result<Vec<Event>, String> {
        let head = self
            .commit
            .as_ref()
            .ok_or("execution history requires a commit")?;
        let info = self.store.read_commit(head).map_err(String::from)?;
        super::events::decode(&info.message).map(|(_, events)| events)
    }

    pub fn commit(&self) -> Option<&Oid> {
        self.commit.as_ref()
    }

    pub fn parent(&self) -> Option<&Oid> {
        self.parent.as_ref()
    }

    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }

    pub fn tree(&self) -> &Oid {
        &self.tree
    }

    pub fn snapshot(&self) -> &Snapshot<'s> {
        &self.snapshot
    }

    pub fn identity(&self) -> Result<Identity, String> {
        let mut value = super::canonical::parse_canonical(&self.required_blob(paths::IDENTITY)?)?;
        value["kind"] = serde_json::json!("root");
        if let Some(source) = &self.execution()?.fork_source {
            value["kind"] = serde_json::json!("fork");
            value["source"] = serde_json::json!(source);
        }
        Identity::from_value(&value)
    }

    pub fn title(&self) -> Result<String, String> {
        parse_title(&self.required_blob(paths::TITLE)?)
    }

    pub fn source_tree_names(&self) -> Result<Vec<String>, String> {
        fn walk(view: &Conversation<'_>, dir: &str, names: &mut Vec<String>) -> Result<(), String> {
            for entry in view.snapshot.list(dir)? {
                if dir.is_empty() && entry.name == ".caos" {
                    continue;
                }
                let path = if dir.is_empty() {
                    entry.name
                } else {
                    format!("{dir}/{}", entry.name)
                };
                match entry.mode {
                    Mode::Commit => names.push(path),
                    Mode::Tree => walk(view, &path, names)?,
                    _ => {}
                }
            }
            Ok(())
        }
        let mut names = Vec::new();
        walk(self, "", &mut names)?;
        names.sort();
        Ok(names)
    }

    pub fn source_tree(&self, name: &str) -> Result<Option<SourceTreeRecord>, String> {
        paths::validate_source_tree_name(name)?;
        let Some(entry) = self.snapshot.entry(name)? else {
            return Ok(None);
        };
        if entry.mode != Mode::Commit {
            return Ok(None);
        }
        Ok(Some(SourceTreeRecord { commit: entry.oid }))
    }

    /// The first value at this path in the current conversation context.
    /// Read it from history only when a comparison or harvest needs a base.
    pub fn reference_start(&self, name: &str) -> Result<Oid, String> {
        paths::validate_source_tree_name(name)?;
        let mut initial = self
            .snapshot
            .entry(name)?
            .filter(|entry| entry.mode == Mode::Commit)
            .ok_or_else(|| format!("no code reference {name:?}"))?
            .oid;
        let mut current = self.commit.clone();
        while let Some(head) = current {
            let info = self.store.read_commit(&head).map_err(String::from)?;
            let entry = Snapshot::new(self.store, info.tree).entry(name)?;
            match entry {
                Some(entry) if entry.mode == Mode::Commit => initial = entry.oid,
                _ => break,
            }
            if matches!(
                Kind::parse_message(&info.message)?,
                Kind::ConversationRoot | Kind::ConversationFork
            ) {
                break;
            }
            current = info.parents.first().cloned();
        }
        Ok(initial)
    }

    pub fn previous_reference(&self, name: &str) -> Result<Option<(String, Oid)>, String> {
        let (dir, leaf) = name.rsplit_once('/').unwrap_or(("", name));
        let mut previous = None;
        for entry in self.snapshot.list(dir)? {
            if entry.mode != Mode::Commit {
                continue;
            }
            if entry.name == leaf {
                return Ok(previous);
            }
            {
                let path = if dir.is_empty() {
                    entry.name
                } else {
                    format!("{dir}/{}", entry.name)
                };
                previous = Some((path, entry.oid));
            }
        }
        Ok(None)
    }

    pub fn source_trees(&self) -> Result<BTreeMap<String, SourceTreeRecord>, String> {
        self.source_tree_names()?
            .into_iter()
            .map(|name| {
                self.source_tree(&name)?
                    .map(|record| (name.clone(), record))
                    .ok_or_else(|| format!("source tree {name:?} disappeared"))
            })
            .collect()
    }

    fn execution(&self) -> Result<&Execution, String> {
        if self.execution.get().is_none() {
            let mut result = Execution::default();
            let mut current = self.commit.clone();
            let mut in_context = true;
            while let Some(head) = current {
                let info = self.store.read_commit(&head).map_err(String::from)?;
                let (kind, events) = super::events::decode(&info.message)?;
                for event in events.into_iter().rev() {
                    match event {
                        Event::Request(record) => {
                            if in_context && result.latest_turn.is_none() {
                                result.latest_turn = Some(record.id.clone());
                            }
                            result
                                .turns
                                .entry(record.id.clone())
                                .or_insert((in_context, record));
                        }
                        Event::Tool(record) => {
                            result
                                .tools
                                .entry((record.request.clone(), record.round))
                                .or_default()
                                .entry(record.id.clone())
                                .or_insert(record);
                        }
                        Event::Child(record) => {
                            result
                                .children
                                .entry(record.id.clone())
                                .or_insert((in_context, record));
                        }
                        Event::Async(record) if in_context => {
                            result.tasks.entry(record.task.clone()).or_insert(record);
                        }
                        Event::Publication(record) if in_context => {
                            if kind == Kind::PublicationPending {
                                result.publication_order.push(record.id.clone());
                            }
                            result
                                .publications
                                .entry(record.id.clone())
                                .or_insert(record);
                        }
                        Event::Payload { path, bytes } => {
                            result.payloads.entry(path).or_insert(bytes);
                        }
                        _ => {}
                    }
                }
                if kind == Kind::ConversationFork && in_context {
                    result.fork_source =
                        Some(info.parents.first().ok_or("fork has no source")?.clone());
                    in_context = false;
                }
                if kind == Kind::ConversationRoot {
                    break;
                }
                current = info.parents.first().cloned();
            }
            let _ = self.execution.set(result);
        }
        Ok(self.execution.get().expect("loaded"))
    }

    pub fn latest_turn(&self) -> Result<Option<TurnRecord>, String> {
        let execution = self.execution()?;
        Ok(execution
            .latest_turn
            .as_ref()
            .and_then(|id| execution.turns.get(id))
            .map(|(_, r)| r.clone()))
    }

    pub fn active_turn(&self) -> Result<Option<TurnRecord>, String> {
        Ok(self.latest_turn()?.filter(|r| {
            matches!(
                r.status,
                super::TurnStatus::Queued
                    | super::TurnStatus::Running
                    | super::TurnStatus::Cancelling
            )
        }))
    }

    pub fn turn(&self, id: &Oid) -> Result<Option<TurnRecord>, String> {
        Ok(self.execution()?.turns.get(id).map(|(_, r)| r.clone()))
    }

    pub fn turn_ids(&self) -> Result<Vec<Oid>, String> {
        Ok(self
            .execution()?
            .turns
            .iter()
            .filter(|(_, (context, _))| *context)
            .map(|(id, _)| id.clone())
            .collect())
    }

    pub fn tool(&self, request: &Oid, round: u64, id: &str) -> Result<Option<CallRecord>, String> {
        Ok(self
            .execution()?
            .tools
            .get(&(request.clone(), round))
            .and_then(|tools| tools.get(id))
            .cloned())
    }

    pub fn tools(&self, request: &Oid, round: u64) -> Result<Vec<CallRecord>, String> {
        Ok(self
            .execution()?
            .tools
            .get(&(request.clone(), round))
            .map(|tools| tools.values().cloned().collect())
            .unwrap_or_default())
    }

    pub fn transcript_len(&self) -> Result<u64, String> {
        let index = self.transcript_index(
            paths::TRANSCRIPT_DIR,
            &self.list_optional(paths::TRANSCRIPT_DIR)?,
        )?;
        Ok(index
            .last_key_value()
            .map(|(ordinal, _)| ordinal + 1)
            .unwrap_or(0))
    }

    pub fn transcript_entry(
        &self,
        ordinal: u64,
    ) -> Result<Option<(String, TranscriptEntry)>, String> {
        let index = self.transcript_index(
            paths::TRANSCRIPT_DIR,
            &self.list_optional(paths::TRANSCRIPT_DIR)?,
        )?;
        index
            .get(&ordinal)
            .map(|(id, path)| self.read_transcript_entry(path, id))
            .transpose()
    }

    fn transcript_index(
        &self,
        dir: &str,
        entries: &[TreeEntry],
    ) -> Result<BTreeMap<u64, (String, String)>, String> {
        let mut index = BTreeMap::new();
        for entry in entries {
            if entry.mode == Mode::Tree {
                continue;
            }
            let path = format!("{dir}/{}", entry.name);
            let (ordinal, message_id) = paths::parse_transcript_entry_path(&path)?;
            if index.insert(ordinal, (message_id, path)).is_some() {
                return Err(format!("multiple transcript entries for ordinal {ordinal}"));
            }
        }
        Ok(index)
    }

    fn read_transcript_entry(
        &self,
        path: &str,
        message_id: &str,
    ) -> Result<(String, TranscriptEntry), String> {
        let entry = TranscriptEntry::parse(&self.required_blob(path)?)?;
        if entry.message_id != message_id {
            return Err(format!(
                "transcript entry {path:?} has mismatched message_id"
            ));
        }
        Ok((message_id.to_string(), entry))
    }

    pub fn transcript(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, String, TranscriptEntry)>, String> {
        if from > to {
            return Err("invalid transcript range".into());
        }
        let index = self.transcript_index(
            paths::TRANSCRIPT_DIR,
            &self.list_optional(paths::TRANSCRIPT_DIR)?,
        )?;
        (from..to)
            .map(|ordinal| {
                let (id, path) = index
                    .get(&ordinal)
                    .ok_or_else(|| format!("missing transcript ordinal {ordinal}"))?;
                let (id, entry) = self.read_transcript_entry(path, id)?;
                Ok((ordinal, id, entry))
            })
            .collect()
    }

    pub fn payload(&self, path: &str) -> Result<Vec<u8>, String> {
        paths::validate_tree_path(path)?;
        if let Some(bytes) = self.execution()?.payloads.get(path) {
            return Ok(bytes.clone());
        }
        self.snapshot
            .read(path)?
            .ok_or_else(|| format!("required path {path} is absent"))
    }

    pub fn async_task(&self, task: &Oid) -> Result<Option<AsyncRecord>, String> {
        Ok(self.execution()?.tasks.get(task).cloned())
    }

    pub fn async_tasks(&self) -> Result<Vec<AsyncRecord>, String> {
        Ok(self.execution()?.tasks.values().cloned().collect())
    }

    pub fn child(&self, id: &str) -> Result<Option<ChildRecord>, String> {
        Ok(self.execution()?.children.get(id).map(|(_, r)| r.clone()))
    }

    pub fn children(&self) -> Result<Vec<ChildRecord>, String> {
        Ok(self
            .execution()?
            .children
            .values()
            .filter(|(context, _)| *context)
            .map(|(_, r)| r.clone())
            .collect())
    }

    pub fn tasks(&self) -> Result<Vec<super::TaskRecord>, String> {
        Ok(self
            .async_tasks()?
            .into_iter()
            .map(super::TaskRecord::Computation)
            .chain(
                self.children()?
                    .into_iter()
                    .map(super::TaskRecord::Conversation),
            )
            .collect())
    }

    pub fn task(&self, computation: &Oid) -> Result<Option<super::TaskRecord>, String> {
        if let Some(task) = self.async_task(computation)? {
            return Ok(Some(super::TaskRecord::Computation(task)));
        }
        Ok(self
            .children()?
            .into_iter()
            .find(|child| &child.relay == computation)
            .map(super::TaskRecord::Conversation))
    }

    pub fn publication(&self, id: &str) -> Result<Option<PublicationRecord>, String> {
        Ok(self.execution()?.publications.get(id).cloned())
    }

    pub fn publications(&self) -> Result<Vec<PublicationRecord>, String> {
        Ok(self.execution()?.publications.values().cloned().collect())
    }

    /// Latest publication states, newest creation first within this context.
    pub fn publications_by_creation(&self) -> Result<Vec<PublicationRecord>, String> {
        let execution = self.execution()?;
        if execution.publication_order.len() != execution.publications.len() {
            return Err(
                "publication records do not have unique publication.pending commits".into(),
            );
        }
        Ok(execution
            .publication_order
            .iter()
            .map(|id| execution.publications[id].clone())
            .collect())
    }

    pub fn file(&self, relative: &str) -> Result<Option<Vec<u8>>, String> {
        paths::validate_tree_path(relative)?;
        self.snapshot.read(&paths::files_path(relative))
    }

    fn require_format(&self) -> Result<(), String> {
        paths::validate_format(&self.required_blob(paths::FORMAT)?)
    }

    fn optional_blob(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        match self.snapshot.entry(path)? {
            None => Ok(None),
            Some(entry) if entry.mode == Mode::Blob => self
                .snapshot
                .blob(&entry.oid)
                .map(Some)
                .map_err(|error| format!("reading {path}: {error}")),
            Some(_) => Err(format!("path {path} is not a mode 100644 blob")),
        }
    }

    fn required_blob(&self, path: &str) -> Result<Vec<u8>, String> {
        self.optional_blob(path)?
            .ok_or_else(|| format!("required path {path} is absent"))
    }

    fn list_optional(&self, dir: &str) -> Result<Vec<TreeEntry>, String> {
        match self.snapshot.entry(dir)? {
            None => Ok(Vec::new()),
            Some(entry) if entry.mode == Mode::Tree => self.snapshot.list(dir),
            Some(_) => Err(format!("path {dir} is not a directory")),
        }
    }
}
