//! V3 conversation storage and exact-head append/retry.

use std::collections::HashSet;
use std::fs;
use std::process::{Command, Stdio};

use conversation_protocol::v3::apply::{apply, inherited_signature, mint, Applied, Transition};
use conversation_protocol::v3::paths;
use conversation_protocol::v3::tree::encode_commit_bytes;
use conversation_protocol::v3::view::Conversation;
use conversation_protocol::v3::{
    validate_spine, Block, ChildRecord, GitStore, ObjectStore, Oid, RefUpdate, TranscriptEntry,
};
use worker_common::{path, scratch};

const MAX_APPEND_ATTEMPTS: usize = 32;
const MAX_RECOVERY_WALK: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Appended {
    /// The commit produced by this logical transition. `head` can be newer
    /// when the push response was lost and another writer appended after it.
    pub commit: Oid,
    pub head: Oid,
    pub ordinal: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TryAppend {
    Appended(Appended),
    HeadChanged(Oid),
}

pub trait RefStore: ObjectStore {
    fn fetch_ref_value(&mut self, refname: &str) -> Result<Option<Oid>, String>;
    fn push_ref_value(
        &mut self,
        refname: &str,
        expected: Option<&Oid>,
        new: Option<&Oid>,
    ) -> Result<(), String>;
}

impl RefStore for GitStore {
    fn fetch_ref_value(&mut self, refname: &str) -> Result<Option<Oid>, String> {
        self.fetch_ref(refname)
    }

    fn push_ref_value(
        &mut self,
        refname: &str,
        expected: Option<&Oid>,
        new: Option<&Oid>,
    ) -> Result<(), String> {
        self.push(&[RefUpdate {
            refname: refname.to_string(),
            expected: expected.cloned(),
            new: new.cloned(),
        }])
    }
}

pub struct State<S: RefStore = GitStore> {
    store: S,
    refname: String,
    head: Oid,
    known_valid: HashSet<Oid>,
    fresh_after_append: bool,
}

impl State<GitStore> {
    pub fn open(conversation: &str) -> Result<Self, String> {
        let refname = conversation_protocol::v3::refs::head_ref(conversation)?;
        let server =
            std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set".to_string())?;
        let store = GitStore::scratch("llm-step-git", server.trim_end_matches('/'))?;
        let head = store
            .fetch_ref(&refname)?
            .ok_or_else(|| format!("conversation ref {refname} does not exist"))?;
        Self::from_store(store, refname, head)
    }

    /// Publish a commit object after its referenced tree and parents are known
    /// to be present on the server. Unlike a content-addressed ref push, this
    /// uses the in-process object endpoint and creates no advertised ref.
    pub fn publish_commit(&mut self, commit: &Oid) -> Result<(), String> {
        let info = self.store.read_commit(commit).map_err(String::from)?;
        for object in std::iter::once(&info.tree).chain(&info.parents) {
            require_server_object(commit, object)?;
        }
        let directory = scratch(&crate::fresh_name("publish-commit"))?;
        let source = directory.join("commit");
        fs::write(&source, encode_commit_bytes(&info))
            .map_err(|error| format!("writing {}: {error}", source.display()))?;
        let target = crate::fresh("published-commit");
        let output = Command::new("caos")
            .args(["put-commit", path(&source), &target])
            .stderr(Stdio::inherit())
            .output()
            .map_err(|error| format!("running caos put-commit: {error}"))?;
        if !output.status.success() {
            return Err(format!("caos put-commit exited with {}", output.status));
        }
        let published = String::from_utf8(output.stdout)
            .map_err(|error| format!("caos put-commit stdout is not UTF-8: {error}"))?;
        let published = Oid::parse(published.trim(), "published commit oid")?;
        if published != *commit {
            return Err(format!(
                "caos put-commit published {published}, expected {commit}"
            ));
        }
        crate::timing::phase("publish commit");
        Ok(())
    }

    /// Publish a code commit before a conversation record names it. The ref is
    /// content-addressed, so an existing equal value is the same publication.
    pub fn push_code(&mut self, commit: &Oid) -> Result<(), String> {
        self.store.ensure_local(commit)?;
        self.store.read_commit(commit).map_err(String::from)?;
        let refname = format!("refs/caos/req/{commit}");
        let pushed = self.store.push(&[RefUpdate {
            refname: refname.clone(),
            expected: None,
            new: Some(commit.clone()),
        }]);
        crate::timing::phase("push code");
        match pushed {
            Ok(()) => Ok(()),
            Err(push_error) => match self.store.fetch_ref(&refname)? {
                Some(current) if current == *commit => Ok(()),
                Some(current) => Err(format!(
                    "content-addressed code ref {refname} names {current}, not {commit}"
                )),
                None => Err(push_error),
            },
        }
    }

    pub fn fetch_object(&mut self, oid: &Oid) -> Result<(), String> {
        self.store.ensure_local(oid)
    }

    /// Atomically close a spawn call and create its child ref. A failed push
    /// is classified by rereading both refs: the immutable spawn intent and
    /// initial child spine distinguish a joined transaction from corruption.
    pub fn atomic_spawn(
        &mut self,
        expected: &Oid,
        transition: Transition,
        child: &ChildRecord,
    ) -> Result<TryAppend, String> {
        if &self.head != expected {
            return Ok(TryAppend::HeadChanged(self.head.clone()));
        }
        let parent_info = self.store.read_commit(expected).map_err(String::from)?;
        let kind = transition.kind();
        let Applied { tree, ordinal } =
            apply(&mut self.store, Some(&parent_info.tree), &transition)?;
        if ordinal.is_some() {
            return Err("subagent.spawn unexpectedly appended a transcript entry".to_string());
        }
        let signature = inherited_signature(&self.store, expected)?;
        let candidate = mint(
            &mut self.store,
            expected,
            &tree,
            transition.kind(),
            &signature,
        )?;
        let child_ref = conversation_protocol::v3::refs::head_ref(&child.id)?;
        let updates = [
            RefUpdate {
                refname: self.refname.clone(),
                expected: Some(expected.clone()),
                new: Some(candidate.clone()),
            },
            RefUpdate {
                refname: child_ref.clone(),
                expected: None,
                new: Some(child.initial_head.clone()),
            },
        ];
        let pushed = self.store.push(&updates);
        crate::timing::phase(&format!("push {}", kind.as_str()));
        if pushed.is_ok() {
            self.head = candidate.clone();
            self.known_valid.insert(candidate.clone());
            self.fresh_after_append = true;
            return Ok(TryAppend::Appended(Appended {
                commit: candidate.clone(),
                head: candidate,
                ordinal: None,
            }));
        }

        let push_error = pushed.unwrap_err();
        let observed_parent = self.store.fetch_ref(&self.refname).map_err(|read_error| {
            format!(
                "atomic spawn push failed ({push_error}); rereading {} also failed: {read_error}",
                self.refname
            )
        })?;
        let observed_child = self.store.fetch_ref(&child_ref).map_err(|read_error| {
            format!(
                "atomic spawn push failed ({push_error}); rereading {child_ref} also failed: {read_error}"
            )
        })?;
        let Some(observed_parent) = observed_parent else {
            return Err(format!(
                "atomic spawn push failed ({push_error}); parent ref {} disappeared",
                self.refname
            ));
        };
        self.head = observed_parent.clone();
        self.fresh_after_append = false;
        self.validate()?;
        let observed_record = self.conversation()?.child(&child.id)?;
        match (observed_record, observed_child) {
            (Some(record), Some(child_head)) => {
                if !same_spawn(&record, child) {
                    return Err(format!(
                        "corrupt subagent spawn {}: parent record disagrees with the durable spawn intent",
                        child.id
                    ));
                }
                if !parent_chain_contains(&self.store, &observed_parent, &candidate)? {
                    return Err(format!(
                        "corrupt subagent spawn {}: parent record is not descended from the reconstructed spawn commit {candidate}",
                        child.id
                    ));
                }
                validate_spine(&self.store, &child_head, &mut HashSet::new())
                    .map_err(|error| format!("corrupt subagent spawn {}: {error}", child.id))?;
                if !parent_chain_contains(&self.store, &child_head, &child.initial_head)? {
                    return Err(format!(
                        "corrupt subagent spawn {}: child ref {child_ref} at {child_head} does not contain initial head {}",
                        child.id, child.initial_head
                    ));
                }
                Ok(TryAppend::Appended(Appended {
                    commit: candidate,
                    head: observed_parent,
                    ordinal: None,
                }))
            }
            (Some(_), None) => Err(format!(
                "corrupt subagent spawn {}: parent record exists but child ref {child_ref} is absent",
                child.id
            )),
            (None, Some(child_head)) => Err(format!(
                "corrupt subagent spawn {}: child ref {child_ref} exists at {child_head} without its parent record",
                child.id
            )),
            (None, None) => Ok(TryAppend::HeadChanged(observed_parent)),
        }
    }
}

fn require_server_object(commit: &Oid, object: &Oid) -> Result<(), String> {
    let base =
        std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set".to_string())?;
    let url = format!("{}/object/{object}", base.trim_end_matches('/'));
    let response = minreq::head(&url)
        .with_timeout(30)
        .send()
        .map_err(|error| format!("HEAD {url}: {error}"))?;
    match response.status_code {
        200..=299 => Ok(()),
        404 => Err(format!(
            "cannot publish commit {commit}: server is missing object {object}"
        )),
        status => Err(format!("HEAD {url}: {status} {}", response.reason_phrase)),
    }
}

impl<S: RefStore> State<S> {
    pub fn from_store(store: S, refname: String, head: Oid) -> Result<Self, String> {
        let mut state = State {
            store,
            refname,
            head,
            known_valid: HashSet::new(),
            fresh_after_append: false,
        };
        state.validate()?;
        Ok(state)
    }

    pub fn head(&self) -> &Oid {
        &self.head
    }

    pub fn refname(&self) -> &str {
        &self.refname
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    pub fn conversation(&self) -> Result<Conversation<'_>, String> {
        Conversation::open(&self.store, &self.head)
    }

    pub fn conversation_at(&self, head: &Oid) -> Result<Conversation<'_>, String> {
        Conversation::open(&self.store, head)
    }

    pub fn reload(&mut self) -> Result<Oid, String> {
        let head = self
            .store
            .fetch_ref_value(&self.refname)?
            .ok_or_else(|| format!("conversation ref {} disappeared", self.refname))?;
        self.head = head.clone();
        self.fresh_after_append = false;
        self.validate()?;
        Ok(head)
    }

    pub fn take_fresh_after_append(&mut self) -> bool {
        std::mem::take(&mut self.fresh_after_append)
    }

    pub fn append(&mut self, transition: Transition) -> Result<Appended, String> {
        for _ in 0..MAX_APPEND_ATTEMPTS {
            let expected = self.head.clone();
            match self.try_append_at(&expected, transition.clone())? {
                TryAppend::Appended(appended) => return Ok(appended),
                TryAppend::HeadChanged(_) => {
                    if let Some(appended) = self.joined(&transition)? {
                        return Ok(appended);
                    }
                }
            }
        }
        Err("conversation kept changing while appending a transition".to_string())
    }

    pub fn try_append_at(
        &mut self,
        expected: &Oid,
        transition: Transition,
    ) -> Result<TryAppend, String> {
        if &self.head != expected {
            return Ok(TryAppend::HeadChanged(self.head.clone()));
        }
        let parent_info = self.store.read_commit(expected).map_err(String::from)?;
        let transition =
            retarget_transcript_transition(&self.store, &parent_info.tree, transition)?;
        let Applied { tree, ordinal } =
            apply(&mut self.store, Some(&parent_info.tree), &transition)?;
        let signature = inherited_signature(&self.store, expected)?;
        let candidate = mint(
            &mut self.store,
            expected,
            &tree,
            transition.kind(),
            &signature,
        )?;
        let pushed = self
            .store
            .push_ref_value(&self.refname, Some(expected), Some(&candidate));
        crate::timing::phase(&format!("push {}", transition.kind().as_str()));
        if pushed.is_ok() {
            self.head = candidate.clone();
            self.known_valid.insert(candidate.clone());
            self.fresh_after_append = true;
            return Ok(TryAppend::Appended(Appended {
                commit: candidate.clone(),
                head: candidate,
                ordinal,
            }));
        }

        let push_error = pushed.unwrap_err();
        let observed = self
            .store
            .fetch_ref_value(&self.refname)
            .map_err(|read_error| {
                format!(
                    "pushing {} failed ({push_error}); rereading it also failed: {read_error}",
                    self.refname
                )
            })?
            .ok_or_else(|| format!("conversation ref {} disappeared", self.refname))?;
        self.head = observed.clone();
        self.fresh_after_append = false;
        self.validate()?;
        if observed == candidate || parent_chain_contains(&self.store, &observed, &candidate)? {
            return Ok(TryAppend::Appended(Appended {
                commit: candidate,
                head: observed,
                ordinal,
            }));
        }
        if &observed != expected {
            Ok(TryAppend::HeadChanged(observed))
        } else {
            Err(push_error)
        }
    }

    pub fn try_append_pair_at(
        &mut self,
        expected: &Oid,
        first: Transition,
        second: Transition,
    ) -> Result<TryAppend, String> {
        if &self.head != expected {
            return Ok(TryAppend::HeadChanged(self.head.clone()));
        }
        let parent_info = self.store.read_commit(expected).map_err(String::from)?;
        let first = retarget_transcript_transition(&self.store, &parent_info.tree, first)?;
        let Applied {
            tree: first_tree,
            ordinal,
        } = apply(&mut self.store, Some(&parent_info.tree), &first)?;
        let signature = inherited_signature(&self.store, expected)?;
        let first_candidate = mint(
            &mut self.store,
            expected,
            &first_tree,
            first.kind(),
            &signature,
        )?;

        let second = retarget_transcript_transition(&self.store, &first_tree, second)?;
        let Applied {
            tree: second_tree,
            ordinal: second_ordinal,
        } = apply(&mut self.store, Some(&first_tree), &second)?;
        let second_signature = inherited_signature(&self.store, &first_candidate)?;
        let candidate = mint(
            &mut self.store,
            &first_candidate,
            &second_tree,
            second.kind(),
            &second_signature,
        )?;
        let pushed = self
            .store
            .push_ref_value(&self.refname, Some(expected), Some(&candidate));
        crate::timing::phase(&format!(
            "push {}+{}",
            first.kind().as_str(),
            second.kind().as_str()
        ));
        if pushed.is_ok() {
            self.head = candidate.clone();
            self.known_valid.insert(first_candidate);
            self.known_valid.insert(candidate.clone());
            self.fresh_after_append = true;
            return Ok(TryAppend::Appended(Appended {
                commit: candidate.clone(),
                head: candidate,
                ordinal: ordinal.or(second_ordinal),
            }));
        }

        let push_error = pushed.unwrap_err();
        let observed = self
            .store
            .fetch_ref_value(&self.refname)
            .map_err(|read_error| {
                format!(
                    "pushing {} failed ({push_error}); rereading it also failed: {read_error}",
                    self.refname
                )
            })?
            .ok_or_else(|| format!("conversation ref {} disappeared", self.refname))?;
        self.head = observed.clone();
        self.fresh_after_append = false;
        self.validate()?;
        if observed == candidate || parent_chain_contains(&self.store, &observed, &candidate)? {
            return Ok(TryAppend::Appended(Appended {
                commit: candidate,
                head: observed,
                ordinal: ordinal.or(second_ordinal),
            }));
        }
        if &observed != expected {
            Ok(TryAppend::HeadChanged(observed))
        } else {
            Err(push_error)
        }
    }

    fn validate(&mut self) -> Result<(), String> {
        validate_spine(&self.store, &self.head, &mut self.known_valid)
            .map(|_| ())
            .map_err(String::from)
    }

    fn joined(&self, transition: &Transition) -> Result<Option<Appended>, String> {
        let view = self.conversation()?;
        let ordinal = match transition_entry(transition) {
            Some(entry) => crate::message_ordinal(&view, &entry.message_id)?,
            None => None,
        };
        if let Some(ordinal) = ordinal {
            return Ok(Some(Appended {
                commit: self.head.clone(),
                head: self.head.clone(),
                ordinal: Some(ordinal),
            }));
        }
        let joined = match transition {
            Transition::AsyncStart { record } => {
                view.async_task(&record.task)?.as_ref() == Some(record)
            }
            Transition::ToolStart { record } | Transition::ToolComplete { record, .. } => {
                view.tool(&record.request, record.round, &record.id)?
                    .as_ref()
                    == Some(record)
            }
            Transition::SubagentApply { child, application } => view
                .child(child)?
                .is_some_and(|record| record.applications.contains(application)),
            _ => false,
        };
        Ok(joined.then(|| Appended {
            commit: self.head.clone(),
            head: self.head.clone(),
            ordinal: None,
        }))
    }
}

fn same_spawn(observed: &ChildRecord, expected: &ChildRecord) -> bool {
    observed.id == expected.id
        && observed.initial_head == expected.initial_head
        && observed.initial_workspace == expected.initial_workspace
        && observed.request == expected.request
        && observed.relay == expected.relay
        && observed.spawn_intent == expected.spawn_intent
}

fn transition_entry(transition: &Transition) -> Option<&TranscriptEntry> {
    match transition {
        Transition::MessageAppend { entry, .. } | Transition::ModelComplete { entry, .. } => {
            Some(entry)
        }
        _ => None,
    }
}

fn retarget_transcript_transition(
    store: &dyn ObjectStore,
    parent_tree: &Oid,
    mut transition: Transition,
) -> Result<Transition, String> {
    let ordinal = Conversation::open_tree(store, parent_tree)?.transcript_len()?;
    match &mut transition {
        Transition::MessageAppend { entry, payloads }
        | Transition::RequestInterject {
            entry, payloads, ..
        }
        | Transition::ModelComplete {
            entry, payloads, ..
        } => retarget_entry(entry, payloads, ordinal),
        _ => {}
    }
    Ok(transition)
}

fn retarget_entry(entry: &mut TranscriptEntry, payloads: &[(String, Vec<u8>)], ordinal: u64) {
    let dir = paths::transcript_payload_dir(ordinal, &entry.message_id);
    for block in &mut entry.blocks {
        let target = match block {
            Block::Payload { path } => Some(path),
            Block::ToolUse { arguments, .. } => Some(arguments),
            Block::Text { .. } => None,
        };
        if let Some(target) = target {
            if let Some(name) = payloads
                .iter()
                .map(|(name, _)| name)
                .find(|name| target.rsplit('/').next() == Some(name.as_str()))
            {
                *target = format!("{dir}/{name}");
            }
        }
    }
}

fn parent_chain_contains(store: &dyn ObjectStore, tip: &Oid, needle: &Oid) -> Result<bool, String> {
    let mut current = tip.clone();
    for _ in 0..MAX_RECOVERY_WALK {
        if &current == needle {
            return Ok(true);
        }
        let info = store.read_commit(&current).map_err(String::from)?;
        let Some(parent) = info.parents.first() else {
            return Ok(false);
        };
        current = parent.clone();
    }
    Err(format!(
        "conversation recovery walk from {tip} exceeded {MAX_RECOVERY_WALK} commits"
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use conversation_protocol::v3::apply::{client_signature, mint};
    use conversation_protocol::v3::oid::ensure_genesis;
    use conversation_protocol::v3::{Identity, IdentityKind, MemoryStore};

    use super::*;

    struct RacingStore {
        objects: MemoryStore,
        head: Oid,
        race_once: bool,
        pushes: usize,
    }

    conversation_protocol::delegate_object_store!(RacingStore, objects);

    impl RefStore for RacingStore {
        fn fetch_ref_value(&mut self, _refname: &str) -> Result<Option<Oid>, String> {
            Ok(Some(self.head.clone()))
        }

        fn push_ref_value(
            &mut self,
            _refname: &str,
            expected: Option<&Oid>,
            new: Option<&Oid>,
        ) -> Result<(), String> {
            self.pushes += 1;
            if self.race_once {
                self.race_once = false;
                let transition = Transition::MessageAppend {
                    entry: entry("22222222222222222222222222222222", "concurrent"),
                    payloads: Vec::new(),
                };
                self.head = append_object(&mut self.objects, &self.head, transition)?;
                return Err("simulated lost compare-and-swap".to_string());
            }
            if expected != Some(&self.head) {
                return Err("stale expected head".to_string());
            }
            self.head = new.cloned().ok_or("test ref deletion is unsupported")?;
            Ok(())
        }
    }

    fn entry(message_id: &str, text: &str) -> TranscriptEntry {
        TranscriptEntry {
            message_id: message_id.to_string(),
            conversation: "conversation".to_string(),
            role: conversation_protocol::v3::Role::System,
            actor: "test".to_string(),
            request: None,
            round: None,
            model: None,
            blocks: vec![Block::Text {
                text: text.to_string(),
            }],
            proposal: None,
            workspace_resolution: None,
        }
    }

    fn append_object(
        store: &mut dyn ObjectStore,
        parent: &Oid,
        transition: Transition,
    ) -> Result<Oid, String> {
        let parent_tree = store.read_commit(parent).map_err(String::from)?.tree;
        let applied = apply(store, Some(&parent_tree), &transition)?;
        let signature = inherited_signature(store, parent)?;
        mint(store, parent, &applied.tree, transition.kind(), &signature)
    }

    fn root(store: &mut MemoryStore) -> Result<Oid, String> {
        let genesis = ensure_genesis(store)?;
        let transition = Transition::ConversationRoot {
            identity: Identity {
                id: "conversation".to_string(),
                kind: IdentityKind::Root,
                owner: None,
            },
            title: "test".to_string(),
            workspaces: BTreeMap::new(),
            files_seed: None,
        };
        let applied = apply(store, None, &transition)?;
        mint(
            store,
            &genesis,
            &applied.tree,
            transition.kind(),
            &client_signature("test", "test@example.invalid", 1),
        )
    }

    #[test]
    fn append_reapplies_after_a_lost_compare_and_swap() {
        let mut objects = MemoryStore::new();
        let root = root(&mut objects).unwrap();
        let store = RacingStore {
            objects,
            head: root.clone(),
            race_once: true,
            pushes: 0,
        };
        let mut state = State::from_store(
            store,
            "refs/caos/v3/conversations/conversation/head".to_string(),
            root,
        )
        .unwrap();

        state
            .append(Transition::MessageAppend {
                entry: entry("11111111111111111111111111111111", "requested"),
                payloads: Vec::new(),
            })
            .unwrap();

        let view = state.conversation().unwrap();
        assert_eq!(view.transcript_len().unwrap(), 2);
        assert_eq!(
            view.transcript_entry(0).unwrap().unwrap().1.blocks,
            entry("22222222222222222222222222222222", "concurrent").blocks
        );
        assert_eq!(
            view.transcript_entry(1).unwrap().unwrap().1.blocks,
            entry("11111111111111111111111111111111", "requested").blocks
        );
    }

    #[test]
    fn append_pair_mints_two_commits_with_one_push() {
        let mut objects = MemoryStore::new();
        let root = root(&mut objects).unwrap();
        let store = RacingStore {
            objects,
            head: root.clone(),
            race_once: false,
            pushes: 0,
        };
        let mut state = State::from_store(
            store,
            "refs/caos/v3/conversations/conversation/head".to_string(),
            root.clone(),
        )
        .unwrap();

        let appended = state
            .try_append_pair_at(
                &root,
                Transition::MessageAppend {
                    entry: entry("11111111111111111111111111111111", "first"),
                    payloads: Vec::new(),
                },
                Transition::MessageAppend {
                    entry: entry("22222222222222222222222222222222", "second"),
                    payloads: Vec::new(),
                },
            )
            .unwrap();
        let TryAppend::Appended(appended) = appended else {
            panic!("pair append lost an uncontended race");
        };

        assert_eq!(state.store().pushes, 1);
        assert_eq!(appended.commit, *state.head());
        assert_eq!(appended.ordinal, Some(0));
        let info = state.store().read_commit(state.head()).unwrap();
        let first = info.parents.first().expect("terminal parent");
        assert_eq!(
            state.store().read_commit(first).unwrap().parents,
            vec![root]
        );
        assert_eq!(state.conversation().unwrap().transcript_len().unwrap(), 2);
        assert!(state.take_fresh_after_append());
        assert!(!state.take_fresh_after_append());
    }
}
