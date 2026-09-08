use super::events::Event;
use std::cell::OnceCell;
use std::collections::BTreeMap;

use super::kinds::Kind;
use super::oid::Oid;
use super::paths;
use super::records::{
    parse_title, AsyncRecord, CallRecord, ChildRecord, Identity, PublicationRecord,
    TranscriptEntry, TurnRecord, WorkspaceRecord,
};
use super::tree::{Mode, ObjectStore, Snapshot, TreeEntry};

pub struct Conversation<'s> {
    store: &'s dyn ObjectStore,
    events: OnceCell<Vec<Event>>,
    context_len: OnceCell<usize>,
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
            events: OnceCell::new(),
            context_len: OnceCell::new(),
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
            events: OnceCell::new(),
            context_len: OnceCell::new(),
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
        let mut current = self.commit.clone();
        while let Some(head) = current {
            let info = self.store.read_commit(&head).map_err(String::from)?;
            match Kind::parse_message(&info.message)? {
                Kind::ConversationFork => {
                    value["kind"] = serde_json::json!("fork");
                    value["source"] =
                        serde_json::json!(info.parents.first().ok_or("fork has no source")?);
                    break;
                }
                Kind::ConversationRoot => break,
                _ => current = info.parents.first().cloned(),
            }
        }
        Identity::from_value(&value)
    }

    pub fn title(&self) -> Result<String, String> {
        parse_title(&self.required_blob(paths::TITLE)?)
    }

    pub fn workspace_names(&self) -> Result<Vec<String>, String> {
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

    pub fn workspace(&self, name: &str) -> Result<Option<WorkspaceRecord>, String> {
        paths::validate_workspace_name(name)?;
        let Some(entry) = self.snapshot.entry(name)? else {
            return Ok(None);
        };
        if entry.mode != Mode::Commit {
            return Ok(None);
        }
        Ok(Some(WorkspaceRecord { commit: entry.oid }))
    }

    /// The first value at this path in the current conversation context.
    /// Read it from history only when a comparison or harvest needs a base.
    pub fn reference_start(&self, name: &str) -> Result<Oid, String> {
        paths::validate_workspace_name(name)?;
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

    pub fn stack_predecessor(&self, name: &str) -> Result<Option<(String, Oid)>, String> {
        let (dir, leaf) = name.rsplit_once('/').unwrap_or(("", name));
        let mut previous = None;
        for entry in self.snapshot.list(dir)? {
            if entry.mode != Mode::Commit {
                continue;
            }
            if entry.name == leaf {
                return Ok(previous);
            }
            if super::workspaces::is_boundary(&entry.name) || entry.name == "00-base" {
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

    /// Derived from ordinary neighboring entries, never a stored workspace record.
    pub fn workspace_config(&self, name: &str) -> Result<super::WorkspaceConfig, String> {
        let dir = name.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        let base_path = if dir.is_empty() {
            ".base-url".into()
        } else {
            format!("{dir}/.base-url")
        };
        let Some(bytes) = self.snapshot.read(&base_path)? else {
            return Ok(Default::default());
        };
        // A malformed optional convention must not make conversation files
        // unreadable: the user needs to be able to edit .base-url to repair it.
        let Ok(base) = super::workspaces::BaseUrl::parse(&bytes) else {
            return Ok(Default::default());
        };
        let mut config = super::WorkspaceConfig::default();
        if let Some((path, commit)) = self.stack_predecessor(name)? {
            config.upstream = Some(if path.ends_with("/00-base") || path == "00-base" {
                super::WorkspaceBase::Branch {
                    repository: Some(base.repository.clone()),
                    name: base.branch.clone(),
                    commit,
                }
            } else {
                super::WorkspaceBase::Workspace { name: path, commit }
            });
        }
        config.publication = Some(super::PublicationDestination {
            repository: Some(base.repository),
            branch: name.to_string(),
            base: Some(match &config.upstream {
                Some(super::WorkspaceBase::Workspace { name, .. }) => {
                    super::workspaces::PublicationBase::Workspace(name.clone())
                }
                _ => super::workspaces::PublicationBase::Branch(base.branch),
            }),
        });
        Ok(config)
    }

    pub fn workspace_configs(&self) -> Result<BTreeMap<String, super::WorkspaceConfig>, String> {
        self.workspace_names()?
            .into_iter()
            .map(|name| self.workspace_config(&name).map(|config| (name, config)))
            .collect()
    }

    pub fn workspaces(&self) -> Result<BTreeMap<String, WorkspaceRecord>, String> {
        self.workspace_names()?
            .into_iter()
            .map(|name| {
                self.workspace(&name)?
                    .map(|record| (name.clone(), record))
                    .ok_or_else(|| format!("workspace {name:?} disappeared"))
            })
            .collect()
    }

    /// Retain ancestral results for the canonical transcript. A fork starts a
    /// new execution context without erasing the results its transcript refers to.
    fn events(&self) -> Result<&[Event], String> {
        if self.events.get().is_none() {
            let mut result = Vec::new();
            let mut current = self.commit.clone();
            while let Some(head) = current {
                let info = self.store.read_commit(&head).map_err(String::from)?;
                let (kind, events) = super::events::decode(&info.message)?;
                result.extend(events.into_iter().rev());
                if kind == Kind::ConversationFork {
                    let _ = self.context_len.set(result.len());
                }
                if kind == Kind::ConversationRoot {
                    break;
                }
                current = info.parents.first().cloned();
            }
            let _ = self.context_len.set(result.len());
            let _ = self.events.set(result);
        }
        Ok(self.events.get().expect("loaded"))
    }

    fn context_events(&self) -> Result<&[Event], String> {
        let events = self.events()?;
        Ok(&events[..*self.context_len.get().expect("loaded")])
    }

    pub fn active_turn(&self) -> Result<Option<TurnRecord>, String> {
        let latest = self.context_events()?.iter().find_map(|event| match event {
            Event::Request(r) => Some(r.clone()),
            _ => None,
        });
        Ok(latest.filter(|r| {
            matches!(
                r.status,
                super::TurnStatus::Queued
                    | super::TurnStatus::Running
                    | super::TurnStatus::Cancelling
            )
        }))
    }

    pub fn turn(&self, id: &Oid) -> Result<Option<TurnRecord>, String> {
        Ok(self.events()?.iter().find_map(|event| match event {
            Event::Request(r) if &r.id == id => Some(r.clone()),
            _ => None,
        }))
    }

    pub fn turn_ids(&self) -> Result<Vec<Oid>, String> {
        Ok(self
            .context_events()?
            .iter()
            .filter_map(|event| match event {
                Event::Request(r) => Some(r.id.clone()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    pub fn tool(&self, request: &Oid, round: u64, id: &str) -> Result<Option<CallRecord>, String> {
        Ok(self.tools(request, round)?.into_iter().find(|r| r.id == id))
    }

    pub fn tools(&self, request: &Oid, round: u64) -> Result<Vec<CallRecord>, String> {
        let mut records = BTreeMap::new();
        for event in self.events()? {
            if let Event::Tool(r) = event {
                if &r.request == request && r.round == round {
                    records.entry(r.id.clone()).or_insert_with(|| r.clone());
                }
            }
        }
        Ok(records.into_values().collect())
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
        for event in self.events()? {
            if let Event::Payload {
                path: stored,
                bytes,
            } = event
            {
                if stored == path {
                    return Ok(bytes.clone());
                }
            }
        }
        self.snapshot
            .read(path)?
            .ok_or_else(|| format!("required path {path} is absent"))
    }

    pub fn async_task(&self, task: &Oid) -> Result<Option<AsyncRecord>, String> {
        Ok(self.async_tasks()?.into_iter().find(|r| &r.task == task))
    }

    pub fn async_tasks(&self) -> Result<Vec<AsyncRecord>, String> {
        let mut records = BTreeMap::new();
        for event in self.context_events()? {
            if let Event::Async(r) = event {
                records.entry(r.task.clone()).or_insert_with(|| r.clone());
            }
        }
        Ok(records.into_values().collect())
    }

    pub fn child(&self, id: &str) -> Result<Option<ChildRecord>, String> {
        Ok(self.events()?.iter().find_map(|event| match event {
            Event::Child(r) if r.id == id => Some(r.clone()),
            _ => None,
        }))
    }

    pub fn children(&self) -> Result<Vec<ChildRecord>, String> {
        let mut records = BTreeMap::new();
        for event in self.context_events()? {
            if let Event::Child(r) = event {
                records.entry(r.id.clone()).or_insert_with(|| r.clone());
            }
        }
        Ok(records.into_values().collect())
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
        Ok(self.publications()?.into_iter().find(|r| r.id == id))
    }

    pub fn publications(&self) -> Result<Vec<PublicationRecord>, String> {
        let mut records = BTreeMap::new();
        for event in self.context_events()? {
            if let Event::Publication(r) = event {
                records.entry(r.id.clone()).or_insert_with(|| r.clone());
            }
        }
        Ok(records.into_values().collect())
    }

    pub fn file(&self, relative: &str) -> Result<Option<Vec<u8>>, String> {
        paths::validate_tree_path(relative)?;
        self.snapshot.read(&paths::files_path(relative))
    }

    fn require_format(&self) -> Result<(), String> {
        let bytes = self.required_blob(paths::FORMAT)?;
        if bytes != paths::FORMAT_BYTES.as_bytes() {
            return Err("unsupported conversation format; use the preserved build to open earlier conversations".to_string());
        }
        Ok(())
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
