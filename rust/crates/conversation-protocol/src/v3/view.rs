use std::collections::BTreeMap;

use super::kinds::Kind;
use super::oid::Oid;
use super::paths;
use super::records::{
    parse_active_request, parse_title, AsyncRecord, ChildRecord, Identity, PublicationRecord,
    Record, RequestRecord, ToolRecord, TranscriptEntry, WorkspaceOrigin, WorkspaceRecord,
};
use super::tree::{Mode, ObjectStore, Snapshot, TreeEntry};

pub struct Conversation<'s> {
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
            commit: None,
            parent: None,
            kind: None,
            tree: tree.clone(),
            snapshot: Snapshot::new(store, tree.clone()),
        };
        conversation.require_format()?;
        Ok(conversation)
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
        Identity::parse(&self.required_blob(paths::IDENTITY)?)
    }

    pub fn title(&self) -> Result<String, String> {
        parse_title(&self.required_blob(paths::TITLE)?)
    }

    pub fn workspace_names(&self) -> Result<Vec<String>, String> {
        let entries = self.list_optional(paths::WORKSPACES_DIR)?;
        let mut names = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.mode != Mode::Tree {
                return Err(format!(
                    "workspace entry {:?} is not a directory",
                    entry.name
                ));
            }
            paths::validate_workspace_name(&entry.name)?;
            names.push(entry.name);
        }
        names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        Ok(names)
    }

    pub fn workspace(&self, name: &str) -> Result<Option<WorkspaceRecord>, String> {
        paths::validate_workspace_name(name)?;
        let commit_path = paths::workspace_commit_path(name);
        let Some(commit) = self.optional_blob(&commit_path)? else {
            if self.snapshot.exists(&paths::workspace_dir(name))? {
                return Err(format!("workspace {name:?} is missing commit"));
            }
            return Ok(None);
        };
        let initial = self.required_blob(&paths::workspace_initial_path(name))?;
        let origin = self
            .optional_blob(&paths::workspace_origin_path(name))?
            .map(|bytes| WorkspaceOrigin::parse(&bytes))
            .transpose()?;
        Ok(Some(WorkspaceRecord {
            commit: Oid::parse_line(&commit, "workspace sha")?,
            initial: Oid::parse_line(&initial, "workspace sha")?,
            origin,
        }))
    }

    pub fn workspace_config(&self, name: &str) -> Result<super::WorkspaceConfig, String> {
        if self.workspace(name)?.is_none() {
            return Err(format!("workspace {name:?} does not exist"));
        }
        self.optional_blob(&paths::workspace_config_path(name))?
            .map(|bytes| super::WorkspaceConfig::parse(&bytes))
            .transpose()
            .map(Option::unwrap_or_default)
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

    pub fn workspaces_tree(&self) -> Result<Option<Oid>, String> {
        match self.snapshot.entry(paths::WORKSPACES_DIR)? {
            None => Ok(None),
            Some(entry) if entry.mode == Mode::Tree => Ok(Some(entry.oid)),
            Some(_) => Err(".caos/workspaces is not a directory".to_string()),
        }
    }

    pub fn active_request(&self) -> Result<Option<RequestRecord>, String> {
        let Some(bytes) = self.optional_blob(paths::ACTIVE_REQUEST)? else {
            return Ok(None);
        };
        let id = parse_active_request(&bytes)?;
        self.request(&id)?
            .map(Some)
            .ok_or_else(|| format!("active request {id} has no request record"))
    }

    pub fn request(&self, id: &Oid) -> Result<Option<RequestRecord>, String> {
        self.keyed::<RequestRecord>(
            &paths::request_record_path(id.as_str()),
            |record| record.id == *id,
            |record| format!("id {}", record.id),
        )
    }

    pub fn request_ids(&self) -> Result<Vec<Oid>, String> {
        let mut ids = Vec::new();
        for entry in self.list_optional(paths::REQUESTS_DIR)? {
            if entry.name == "active" {
                continue;
            }
            if entry.mode != Mode::Blob {
                return Err(format!("request entry {:?} is not a blob", entry.name));
            }
            let stem = entry
                .name
                .strip_suffix(".json")
                .ok_or_else(|| format!("invalid request record name {:?}", entry.name))?;
            ids.push(Oid::parse(stem, "request record id")?);
        }
        ids.sort();
        Ok(ids)
    }

    pub fn tool(&self, request: &Oid, round: u64, id: &str) -> Result<Option<ToolRecord>, String> {
        self.keyed::<ToolRecord>(
            &paths::tool_record_path(request.as_str(), round, id),
            |record| record.request == *request && record.round == round && record.id == id,
            |record| {
                format!(
                    "request {}, round {}, id {:?}",
                    record.request, record.round, record.id
                )
            },
        )
    }

    pub fn tools(&self, request: &Oid, round: u64) -> Result<Vec<ToolRecord>, String> {
        let dir = format!("{}/{}/{round:04}", paths::TOOLS_DIR, request.as_str());
        let mut records = Vec::new();
        for entry in self.list_optional(&dir)? {
            if entry.mode == Mode::Tree {
                continue;
            }
            let path = format!("{dir}/{}", entry.name);
            if !entry.name.ends_with(".json") {
                return Err(format!("invalid tool record path {path:?}"));
            }
            let record = ToolRecord::parse(&self.required_blob(&path)?)?;
            if record.request != *request
                || record.round != round
                || paths::tool_record_path(request.as_str(), round, &record.id) != path
            {
                return Err(format!("tool record {path:?} has mismatched identity"));
            }
            records.push(record);
        }
        Ok(records)
    }

    pub fn transcript_len(&self) -> Result<u64, String> {
        let shards = self.list_optional(paths::TRANSCRIPT_DIR)?;
        let Some(last_shard) = shards.last() else {
            return Ok(0);
        };
        if last_shard.mode != Mode::Tree
            || last_shard.name.len() != 9
            || !last_shard.name.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(format!("invalid transcript shard {:?}", last_shard.name));
        }
        let dir = format!("{}/{}", paths::TRANSCRIPT_DIR, last_shard.name);
        let entry = self
            .snapshot
            .list(&dir)?
            .into_iter()
            .rev()
            .find(|entry| entry.mode != Mode::Tree)
            .ok_or_else(|| format!("transcript shard {:?} has no entries", last_shard.name))?;
        let path = format!("{dir}/{}", entry.name);
        let (ordinal, _) = paths::parse_transcript_entry_path(&path)?;
        ordinal
            .checked_add(1)
            .ok_or_else(|| "transcript ordinal overflow".to_string())
    }

    pub fn transcript_entry(
        &self,
        ordinal: u64,
    ) -> Result<Option<(String, TranscriptEntry)>, String> {
        let dir = format!(
            "{}/{}",
            paths::TRANSCRIPT_DIR,
            paths::transcript_shard(ordinal)
        );
        let entries = self.list_optional(&dir)?;
        let index = self.transcript_index(&dir, &entries)?;
        index
            .get(&ordinal)
            .map(|(message_id, path)| self.read_transcript_entry(path, message_id))
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
            return Err(format!("invalid transcript range {from}..{to}"));
        }
        let mut entries = Vec::new();
        let mut ordinal = from;
        while ordinal < to {
            let dir = format!(
                "{}/{}",
                paths::TRANSCRIPT_DIR,
                paths::transcript_shard(ordinal)
            );
            let listing = self.list_optional(&dir)?;
            let index = self.transcript_index(&dir, &listing)?;
            let shard_end = ordinal.saturating_add(1000 - ordinal % 1000).min(to);
            while ordinal < shard_end {
                let (message_id, path) = index
                    .get(&ordinal)
                    .ok_or_else(|| format!("missing transcript ordinal {ordinal}"))?;
                let (message_id, entry) = self.read_transcript_entry(path, message_id)?;
                entries.push((ordinal, message_id, entry));
                ordinal += 1;
            }
        }
        Ok(entries)
    }

    pub fn payload(&self, path: &str) -> Result<Vec<u8>, String> {
        paths::validate_tree_path(path)?;
        self.snapshot
            .read(path)?
            .ok_or_else(|| format!("required path {path} is absent"))
    }

    pub fn async_task(&self, task: &Oid) -> Result<Option<AsyncRecord>, String> {
        self.keyed::<AsyncRecord>(
            &paths::async_record_path(task.as_str()),
            |record| record.task == *task,
            |record| format!("task {}", record.task),
        )
    }

    pub fn async_tasks(&self) -> Result<Vec<AsyncRecord>, String> {
        self.keyed_all::<AsyncRecord, _>(
            paths::ASYNC_DIR,
            |stem| Oid::parse(stem, "async record task"),
            |record, task| record.task == *task,
            |record| format!("task {}", record.task),
        )
    }

    pub fn child(&self, id: &str) -> Result<Option<ChildRecord>, String> {
        paths::validate_protocol_id_component(id)?;
        self.keyed::<ChildRecord>(
            &paths::subagent_record_path(id),
            |record| record.id == id,
            |record| format!("id {:?}", record.id),
        )
    }

    pub fn children(&self) -> Result<Vec<ChildRecord>, String> {
        self.keyed_all::<ChildRecord, _>(
            paths::SUBAGENTS_DIR,
            |stem| {
                paths::validate_protocol_id_component(stem)?;
                Ok(stem.to_string())
            },
            |record, id| record.id == *id,
            |record| format!("id {:?}", record.id),
        )
    }

    pub fn publication(&self, id: &str) -> Result<Option<PublicationRecord>, String> {
        paths::validate_protocol_id_component(id)?;
        self.keyed::<PublicationRecord>(
            &paths::publication_record_path(id),
            |record| record.id == id,
            |record| format!("id {:?}", record.id),
        )
    }

    pub fn publications(&self) -> Result<Vec<PublicationRecord>, String> {
        self.keyed_all::<PublicationRecord, _>(
            paths::PUBLICATIONS_DIR,
            |stem| {
                paths::validate_protocol_id_component(stem)?;
                Ok(stem.to_string())
            },
            |record, id| record.id == *id,
            |record| format!("id {:?}", record.id),
        )
    }

    pub fn file(&self, relative: &str) -> Result<Option<Vec<u8>>, String> {
        paths::validate_tree_path(relative)?;
        self.snapshot.read(&paths::files_path(relative))
    }

    fn require_format(&self) -> Result<(), String> {
        let bytes = self.required_blob(paths::FORMAT)?;
        if bytes != paths::FORMAT_BYTES.as_bytes() {
            return Err("unsupported conversation format".to_string());
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

    fn keyed<T: Record>(
        &self,
        path: &str,
        matches_path: impl FnOnce(&T) -> bool,
        identity: impl FnOnce(&T) -> String,
    ) -> Result<Option<T>, String> {
        let record = self
            .optional_blob(path)?
            .map(|bytes| T::parse_record(&bytes))
            .transpose()?;
        if let Some(record) = record.as_ref() {
            if !matches_path(record) {
                return Err(format!(
                    "record at {path:?} has identity {}, which does not match its path",
                    identity(record)
                ));
            }
        }
        Ok(record)
    }

    fn keyed_all<T, K>(
        &self,
        dir: &str,
        parse_key: impl Fn(&str) -> Result<K, String>,
        matches_path: impl Fn(&T, &K) -> bool,
        identity: impl Fn(&T) -> String,
    ) -> Result<Vec<T>, String>
    where
        T: Record,
    {
        self.list_optional(dir)?
            .into_iter()
            .map(|entry| {
                let stem = record_stem(dir, &entry)?;
                let key = parse_key(stem)?;
                let path = format!("{dir}/{}", entry.name);
                self.keyed(
                    &path,
                    |record| matches_path(record, &key),
                    |record| identity(record),
                )?
                .ok_or_else(|| format!("record {path:?} disappeared"))
            })
            .collect()
    }

    fn list_optional(&self, dir: &str) -> Result<Vec<TreeEntry>, String> {
        match self.snapshot.entry(dir)? {
            None => Ok(Vec::new()),
            Some(entry) if entry.mode == Mode::Tree => self.snapshot.list(dir),
            Some(_) => Err(format!("path {dir} is not a directory")),
        }
    }
}

fn record_stem<'a>(dir: &str, entry: &'a TreeEntry) -> Result<&'a str, String> {
    if entry.mode != Mode::Blob {
        return Err(format!("invalid record entry {dir}/{:?}", entry.name));
    }
    entry
        .name
        .strip_suffix(".json")
        .ok_or_else(|| format!("invalid record entry {dir}/{:?}", entry.name))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;
    use crate::v3::apply::{apply, client_signature, mint, Transition};
    use crate::v3::oid::ensure_genesis;
    use crate::v3::records::{IdentityKind, Role};
    use crate::v3::tree::{MemoryStore, ObjectStore, TreeBuilder};

    fn oid(character: char) -> Oid {
        Oid::parse(&character.to_string().repeat(40), "test oid").unwrap()
    }

    fn conversation() -> (MemoryStore, Oid) {
        let mut store = MemoryStore::new();
        let genesis = ensure_genesis(&mut store).unwrap();
        let transition = Transition::ConversationRoot {
            identity: Identity {
                id: "conversation".to_string(),
                kind: IdentityKind::Root,
                owner: None,
            },
            title: "Title".to_string(),
            workspaces: BTreeMap::from([("main".to_string(), (oid('a'), None))]),
            files_seed: None,
        };
        let root = apply(&mut store, None, &transition).unwrap();
        let signature = client_signature("Tester", "tester@example.com", 1);
        let root_head = mint(
            &mut store,
            &genesis,
            &root.tree,
            transition.kind(),
            &signature,
        )
        .unwrap();
        let message_id = "message-0";
        let payload_path = format!("{}/body", paths::transcript_payload_dir(0, message_id));
        let message = Transition::MessageAppend {
            entry: TranscriptEntry {
                message_id: message_id.to_string(),
                conversation: "conversation".to_string(),
                role: Role::User,
                actor: "user".to_string(),
                request: None,
                round: None,
                model: None,
                blocks: vec![super::super::records::Block::Payload { path: payload_path }],
                proposal: None,
                workspace_resolution: None,
            },
            payloads: vec![("body".to_string(), b"body".to_vec())],
        };
        let appended = apply(&mut store, Some(&root.tree), &message).unwrap();
        let head = mint(
            &mut store,
            &root_head,
            &appended.tree,
            message.kind(),
            &signature,
        )
        .unwrap();
        (store, head)
    }

    struct CountingStore {
        inner: MemoryStore,
        tree_reads: RefCell<BTreeMap<Oid, usize>>,
    }

    impl ObjectStore for CountingStore {
        fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>, super::super::tree::StoreError> {
            self.inner.read_blob(oid)
        }

        fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>, super::super::tree::StoreError> {
            *self.tree_reads.borrow_mut().entry(oid.clone()).or_default() += 1;
            self.inner.read_tree(oid)
        }

        fn read_commit(
            &self,
            oid: &Oid,
        ) -> Result<super::super::tree::CommitInfo, super::super::tree::StoreError> {
            self.inner.read_commit(oid)
        }

        fn write_blob(&mut self, bytes: &[u8]) -> Result<Oid, super::super::tree::StoreError> {
            self.inner.write_blob(bytes)
        }

        fn write_tree(
            &mut self,
            entries: &[TreeEntry],
        ) -> Result<Oid, super::super::tree::StoreError> {
            self.inner.write_tree(entries)
        }

        fn write_commit(
            &mut self,
            commit: &super::super::tree::CommitInfo,
        ) -> Result<Oid, super::super::tree::StoreError> {
            self.inner.write_commit(commit)
        }
    }

    #[test]
    fn opens_commits_and_trees_and_reads_core_state() {
        let (store, head) = conversation();
        let expected_parent = store.read_commit(&head).unwrap().parents[0].clone();
        let view = Conversation::open(&store, &head).unwrap();
        assert_eq!(view.commit(), Some(&head));
        assert_eq!(view.parent(), Some(&expected_parent));
        assert_eq!(view.kind(), Some(Kind::MessageAppend));
        assert_eq!(view.identity().unwrap().id, "conversation");
        assert_eq!(view.title().unwrap(), "Title");
        assert_eq!(view.workspace_names().unwrap(), vec!["main"]);
        assert_eq!(view.transcript_len().unwrap(), 1);
        assert_eq!(view.transcript_entry(0).unwrap().unwrap().0, "message-0");
        assert!(view.transcript_entry(1).unwrap().is_none());
        assert!(view.active_request().unwrap().is_none());
        assert!(view.async_tasks().unwrap().is_empty());
        assert!(view.children().unwrap().is_empty());
        assert!(view.publications().unwrap().is_empty());
        let tree = view.tree().clone();
        let tree_view = Conversation::open_tree(&store, &tree).unwrap();
        assert!(tree_view.commit().is_none());
        assert!(tree_view.parent().is_none());
        assert!(tree_view.kind().is_none());
    }

    #[test]
    fn executable_files_are_readable_and_missing_payloads_error() {
        let (mut store, head) = conversation();
        let tree = store.read_commit(&head).unwrap().tree;
        let mut builder = TreeBuilder::from(Some(tree));
        builder.put("files/run", Mode::Executable, b"#!/bin/sh\n".to_vec());
        let tree = builder.build(&mut store).unwrap();
        let view = Conversation::open_tree(&store, &tree).unwrap();
        assert_eq!(view.file("run").unwrap().unwrap(), b"#!/bin/sh\n");
        assert!(view.payload(".caos/missing").is_err());
    }

    #[test]
    fn keyed_identity_error_names_path_and_record_identity() {
        let (mut store, head) = conversation();
        let tree = store.read_commit(&head).unwrap().tree;
        let requested = oid('1');
        let actual = oid('2');
        let mut builder = TreeBuilder::from(Some(tree));
        builder.put(
            &paths::request_record_path(requested.as_str()),
            Mode::Blob,
            RequestRecord {
                id: actual.clone(),
                request_head: oid('3'),
                request_workspaces: None,
                model: "model".to_string(),
                configuration: "configuration".to_string(),
                round: 0,
                calls: Vec::new(),
                interjections: Vec::new(),
                status: super::super::records::RequestStatus::Queued,
                latest_message: None,
                escape_reason: None,
                outcome: None,
            }
            .encode(),
        );
        let tree = builder.build(&mut store).unwrap();
        let error = Conversation::open_tree(&store, &tree)
            .unwrap()
            .request(&requested)
            .unwrap_err();
        assert!(error.contains(&paths::request_record_path(requested.as_str())));
        assert!(error.contains(actual.as_str()));
    }

    #[test]
    fn transcript_reads_each_shard_tree_once() {
        let (mut store, head) = conversation();
        let base = store.read_commit(&head).unwrap().tree;
        let mut builder = TreeBuilder::from(Some(base));
        for ordinal in [999, 1000] {
            let message_id = format!("message-{ordinal}");
            builder.put(
                &paths::transcript_entry_path(ordinal, &message_id),
                Mode::Blob,
                TranscriptEntry {
                    message_id,
                    conversation: "conversation".to_string(),
                    role: Role::User,
                    actor: "user".to_string(),
                    request: None,
                    round: None,
                    model: None,
                    blocks: Vec::new(),
                    proposal: None,
                    workspace_resolution: None,
                }
                .encode(),
            );
        }
        let tree = builder.build(&mut store).unwrap();
        let snapshot = Snapshot::new(&store, tree.clone());
        let shards: Vec<Oid> = [999, 1000]
            .into_iter()
            .map(|ordinal| {
                snapshot
                    .entry(&format!(
                        "{}/{}",
                        paths::TRANSCRIPT_DIR,
                        paths::transcript_shard(ordinal)
                    ))
                    .unwrap()
                    .unwrap()
                    .oid
            })
            .collect();
        let store = CountingStore {
            inner: store,
            tree_reads: RefCell::new(BTreeMap::new()),
        };
        let view = Conversation::open_tree(&store, &tree).unwrap();
        store.tree_reads.borrow_mut().clear();

        assert_eq!(view.transcript(999, 1001).unwrap().len(), 2);
        for shard in shards {
            assert_eq!(store.tree_reads.borrow().get(&shard), Some(&1));
        }
    }
}
