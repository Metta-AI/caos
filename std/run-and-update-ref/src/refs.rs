//! Append terminal async records or child checkpoints to a v3 conversation.

use std::collections::{BTreeMap, HashSet};

use conversation_protocol::v3::apply::{apply, inherited_signature, mint, Transition};
use conversation_protocol::v3::refs as conversation_refs;
use conversation_protocol::v3::view::Conversation;
use conversation_protocol::v3::{
    validate_spine, AsyncStatus, ChildStatus, ChildWorkspace, CodeOps, GitStore, ObjectStore, Oid,
    RefUpdate, RequestOutcome, RequestStatus,
};

const MAX_CAS_ATTEMPTS: usize = 32;

pub fn validate_target_ref(refname: &str) -> Result<(), String> {
    conversation_refs::parse_head_ref(refname).map(|_| ())
}

pub fn validate_child(child: &str) -> Result<(), String> {
    conversation_refs::head_ref(child).map(|_| ())
}

pub fn append_status(refname: &str, task: &str, status: &str, result: &str) -> Result<(), String> {
    validate_target_ref(refname)?;
    let task = Oid::parse(task, "task")?;
    let result = Oid::parse(result, "result")?;
    let status = match status {
        "complete" => AsyncStatus::Complete,
        "failed" => AsyncStatus::Failed,
        _ => return Err(format!("invalid async status {status:?}")),
    };
    let mut store = scratch_store()?;
    append_status_with(&mut store, refname, &task, status, &result)
}

pub fn append_child_terminal(
    refname: &str,
    child: &str,
    subrequest: &str,
    relay: &str,
) -> Result<(), String> {
    validate_target_ref(refname)?;
    validate_child(child)?;
    let subrequest = Oid::parse(subrequest, "subrequest")?;
    let relay = Oid::parse(relay, "relay")?;
    let mut store = scratch_store()?;
    let child_ref = conversation_refs::head_ref(child)?;
    let terminal_head = store
        .fetch_ref(&child_ref)?
        .ok_or_else(|| format!("child conversation ref {child_ref} does not exist"))?;
    validate_spine(&store, &terminal_head, &mut HashSet::new()).map_err(String::from)?;
    let (status, child_workspaces) = terminal_facts(&store, &terminal_head, &subrequest)?;
    append_child_terminal_with(
        &mut store,
        refname,
        child,
        &subrequest,
        &relay,
        &terminal_head,
        status,
        &child_workspaces,
    )
}

fn terminal_facts(
    store: &dyn ObjectStore,
    terminal_head: &Oid,
    subrequest: &Oid,
) -> Result<(ChildStatus, BTreeMap<String, ChildWorkspace>), String> {
    let conversation = Conversation::open(store, terminal_head)?;
    let request = conversation
        .request(subrequest)?
        .ok_or_else(|| format!("child request {subrequest} does not exist at {terminal_head}"))?;
    let status = match (request.status, request.outcome) {
        (
            RequestStatus::Idle,
            Some(RequestOutcome::Idle {
                interrupted: false, ..
            }),
        ) => ChildStatus::Completed,
        (
            RequestStatus::Idle,
            Some(RequestOutcome::Idle {
                interrupted: true, ..
            }),
        ) => ChildStatus::Cancelled,
        (RequestStatus::Failed, Some(RequestOutcome::Failed { .. })) => ChildStatus::Failed,
        _ => return Err("child request not terminal".to_string()),
    };
    let child_workspaces = conversation
        .workspaces()?
        .into_iter()
        .map(|(name, workspace)| {
            (
                name,
                ChildWorkspace {
                    commit: workspace.commit,
                    initial: workspace.initial,
                },
            )
        })
        .collect();
    Ok((status, child_workspaces))
}

#[allow(clippy::too_many_arguments)]
fn append_child_terminal_with<S: RefStore>(
    store: &mut S,
    refname: &str,
    child: &str,
    subrequest: &Oid,
    relay: &Oid,
    terminal_head: &Oid,
    status: ChildStatus,
    child_workspaces: &BTreeMap<String, ChildWorkspace>,
) -> Result<(), String> {
    cas_append(store, refname, |store, head| {
        let (head_tree, record) = {
            let conversation = Conversation::open(store, head)?;
            let record = conversation
                .child(child)?
                .ok_or_else(|| format!("subagent {child} was never recorded on {refname}"))?;
            (conversation.tree().clone(), record)
        };
        if record.request != *subrequest {
            return Err(format!(
                "subagent {child} records request {}, not {subrequest}",
                record.request
            ));
        }
        if record.relay != *relay {
            return Err(format!(
                "subagent {child} records relay {}, not {relay}",
                record.relay
            ));
        }
        if record.status != ChildStatus::Running {
            if record.terminal_head.as_ref() == Some(terminal_head) {
                return Ok(None);
            }
            return Err(format!(
                "subagent {child} already settled at {:?}, not {terminal_head}",
                record.terminal_head
            ));
        }

        Ok(Some((
            head_tree,
            Transition::SubagentTerminal {
                child: child.to_string(),
                terminal_head: terminal_head.clone(),
                status,
                child_workspaces: child_workspaces.clone(),
            },
        )))
    })
}

fn append_status_with<S: RefStore>(
    store: &mut S,
    refname: &str,
    task: &Oid,
    status: AsyncStatus,
    result: &Oid,
) -> Result<(), String> {
    cas_append(store, refname, |store, head| {
        let (head_tree, record) = {
            let conversation = Conversation::open(store, head)?;
            let record = conversation
                .async_task(task)?
                .ok_or_else(|| format!("task {task} was never recorded on {refname}"))?;
            (conversation.tree().clone(), record)
        };

        if record.status != AsyncStatus::Pending {
            if record.status == status && record.result.as_ref() == Some(result) {
                return Ok(None);
            }
            return Err(format!(
                "task {task} already settled with status {:?} and result {:?}",
                record.status, record.result
            ));
        }

        Ok(Some((
            head_tree,
            Transition::AsyncTerminal {
                task: task.clone(),
                status,
                result: Some(result.clone()),
                reason: None,
            },
        )))
    })
}

fn cas_append<S: RefStore>(
    store: &mut S,
    refname: &str,
    mut build: impl FnMut(&mut S, &Oid) -> Result<Option<(Oid, Transition)>, String>,
) -> Result<(), String> {
    for _ in 0..MAX_CAS_ATTEMPTS {
        let head = store
            .fetch_head(refname)?
            .ok_or_else(|| format!("target conversation ref {refname} does not exist"))?;
        let Some((head_tree, transition)) = build(store, &head)? else {
            return Ok(());
        };
        let applied = apply(store, Some(&head_tree), &transition)?;
        let signature = inherited_signature(store, &head)?;
        let candidate = mint(store, &head, &applied.tree, transition.kind(), &signature)?;
        let update = RefUpdate {
            refname: refname.to_string(),
            expected: Some(head.clone()),
            new: Some(candidate.clone()),
        };
        match store.push_head(update) {
            Ok(()) => return Ok(()),
            Err(push_error) => {
                let observed = store.fetch_head(refname).map_err(|read_error| {
                    format!(
                        "pushing {refname} failed ({push_error}); rereading it also failed: {read_error}"
                    )
                })?;
                let Some(observed) = observed else {
                    return Err(format!(
                        "pushing {refname} failed ({push_error}); the ref disappeared"
                    ));
                };
                if store.is_ancestor(&candidate, &observed)? {
                    return Ok(());
                }
                if observed == head {
                    return Err(push_error);
                }
            }
        }
    }
    Err(format!(
        "target ref {refname} kept changing after {MAX_CAS_ATTEMPTS} attempts"
    ))
}

trait RefStore: ObjectStore {
    fn fetch_head(&mut self, refname: &str) -> Result<Option<Oid>, String>;
    fn push_head(&mut self, update: RefUpdate) -> Result<(), String>;
    fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool, String>;
}

impl RefStore for GitStore {
    fn fetch_head(&mut self, refname: &str) -> Result<Option<Oid>, String> {
        self.fetch_ref(refname)
    }

    fn push_head(&mut self, update: RefUpdate) -> Result<(), String> {
        self.push(&[update])
    }

    fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool, String> {
        CodeOps::is_ancestor(self, ancestor, descendant)
    }
}

fn scratch_store() -> Result<GitStore, String> {
    let server =
        std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set".to_string())?;
    GitStore::scratch("run-and-update-ref-git", server.trim_end_matches('/'))
}

pub(crate) fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    Oid::parse(hash, what).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use conversation_protocol::v3::apply::client_signature;
    use conversation_protocol::v3::oid::ensure_genesis;
    use conversation_protocol::v3::{
        AsyncRecord, Block, Identity, IdentityKind, MemoryStore, Mode, RequestRecord, Role,
        TranscriptEntry, TreeBuilder,
    };

    use super::*;

    enum FirstPush {
        Normal,
        MoveTo(Oid),
        AcceptThenAdvance,
    }

    struct FakeStore {
        objects: MemoryStore,
        head: Option<Oid>,
        first_push: FirstPush,
        pushes: Vec<Oid>,
    }

    impl FakeStore {
        fn new(objects: MemoryStore, head: Oid) -> Self {
            Self {
                objects,
                head: Some(head),
                first_push: FirstPush::Normal,
                pushes: Vec::new(),
            }
        }

        fn head(&self) -> &Oid {
            self.head.as_ref().expect("test ref exists")
        }
    }

    conversation_protocol::delegate_object_store!(FakeStore, objects);

    impl RefStore for FakeStore {
        fn fetch_head(&mut self, _refname: &str) -> Result<Option<Oid>, String> {
            Ok(self.head.clone())
        }

        fn push_head(&mut self, update: RefUpdate) -> Result<(), String> {
            let new = update
                .new
                .ok_or_else(|| "test update unexpectedly deleted the ref".to_string())?;
            self.pushes.push(new.clone());
            match std::mem::replace(&mut self.first_push, FirstPush::Normal) {
                FirstPush::MoveTo(winner) => {
                    self.head = Some(winner);
                    Err("injected CAS loss".to_string())
                }
                FirstPush::AcceptThenAdvance => {
                    if update.expected != self.head {
                        return Err("test lease mismatch".to_string());
                    }
                    self.head = Some(new.clone());
                    let tree = self.read_commit(&new).map_err(String::from)?.tree;
                    let transition = Transition::TitleSet {
                        title: "Advanced after accepted push".to_string(),
                    };
                    let applied = apply(self, Some(&tree), &transition)?;
                    let signature = inherited_signature(self, &new)?;
                    let advanced = mint(self, &new, &applied.tree, transition.kind(), &signature)?;
                    self.head = Some(advanced);
                    Err("injected lost response".to_string())
                }
                FirstPush::Normal => {
                    if update.expected != self.head {
                        return Err("test lease mismatch".to_string());
                    }
                    self.head = Some(new);
                    Ok(())
                }
            }
        }

        fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool, String> {
            let mut current = descendant.clone();
            loop {
                if &current == ancestor {
                    return Ok(true);
                }
                let commit = self.read_commit(&current).map_err(String::from)?;
                let Some(parent) = commit.parents.first() else {
                    return Ok(false);
                };
                current = parent.clone();
            }
        }
    }

    fn oid(character: char) -> Oid {
        Oid::parse(&character.to_string().repeat(40), "test oid").unwrap()
    }

    fn signature() -> conversation_protocol::v3::Signature {
        client_signature("Test", "test@example.com", 1_700_000_000)
    }

    fn commit_transition(store: &mut MemoryStore, parent: &Oid, transition: &Transition) -> Oid {
        let parent_tree = store.read_commit(parent).unwrap().tree;
        let applied = apply(store, Some(&parent_tree), transition).unwrap();
        mint(
            store,
            parent,
            &applied.tree,
            transition.kind(),
            &signature(),
        )
        .unwrap()
    }

    fn conversation(with_task: bool) -> (FakeStore, String, Oid) {
        let refname = conversation_refs::head_ref("conversation").unwrap();
        let task = oid('1');
        let mut objects = MemoryStore::new();
        let genesis = ensure_genesis(&mut objects).unwrap();
        let root = Transition::ConversationRoot {
            identity: Identity {
                id: "conversation".to_string(),
                kind: IdentityKind::Root,
                owner: None,
            },
            title: "Conversation".to_string(),
            workspaces: BTreeMap::new(),
            files_seed: None,
        };
        let applied = apply(&mut objects, None, &root).unwrap();
        let mut head = mint(
            &mut objects,
            &genesis,
            &applied.tree,
            root.kind(),
            &signature(),
        )
        .unwrap();
        if with_task {
            head = commit_transition(
                &mut objects,
                &head,
                &Transition::AsyncStart {
                    record: AsyncRecord {
                        task: task.clone(),
                        status: AsyncStatus::Pending,
                        target_ref: Some(refname.clone()),
                        result: None,
                        reason: None,
                    },
                },
            );
        }
        (FakeStore::new(objects, head), refname, task)
    }

    fn stored_result(store: &mut FakeStore, contents: &[u8]) -> Oid {
        store.write_blob(contents).unwrap()
    }

    fn stored_failure_tree(store: &mut FakeStore) -> Oid {
        let mut builder = TreeBuilder::from(None);
        builder.put("status", Mode::Blob, b"failed\n".to_vec());
        builder.put("error", Mode::Blob, b"subrequest failed\n".to_vec());
        builder.build(store).unwrap()
    }

    fn child_request_history(interrupted: Option<bool>) -> (MemoryStore, Oid, Oid) {
        let mut store = MemoryStore::new();
        let genesis = ensure_genesis(&mut store).unwrap();
        let root = Transition::ConversationRoot {
            identity: Identity {
                id: "subagent-test".to_string(),
                kind: IdentityKind::Root,
                owner: None,
            },
            title: "Child".to_string(),
            workspaces: BTreeMap::from([("main".to_string(), (oid('a'), None))]),
            files_seed: None,
        };
        let applied = apply(&mut store, None, &root).unwrap();
        let mut head = mint(
            &mut store,
            &genesis,
            &applied.tree,
            root.kind(),
            &signature(),
        )
        .unwrap();
        head = commit_transition(
            &mut store,
            &head,
            &Transition::MessageAppend {
                entry: TranscriptEntry {
                    message_id: "prompt".to_string(),
                    conversation: "subagent-test".to_string(),
                    role: Role::User,
                    actor: "owner".to_string(),
                    request: None,
                    round: None,
                    model: None,
                    blocks: vec![Block::Text {
                        text: "work".to_string(),
                    }],
                    proposal: None,
                    workspace_resolution: None,
                },
                payloads: Vec::new(),
            },
        );
        let request = oid('7');
        let request_workspaces = Conversation::open(&store, &head)
            .unwrap()
            .workspaces_tree()
            .unwrap();
        head = commit_transition(
            &mut store,
            &head,
            &Transition::RequestAdmit {
                record: RequestRecord {
                    id: request.clone(),
                    request_head: head.clone(),
                    request_workspaces,
                    model: "model".to_string(),
                    configuration: "configuration".to_string(),
                    round: 0,
                    calls: Vec::new(),
                    interjections: Vec::new(),
                    status: RequestStatus::Queued,
                    latest_message: None,
                    escape_reason: None,
                    outcome: None,
                },
            },
        );
        head = commit_transition(
            &mut store,
            &head,
            &Transition::RequestClaim {
                request: request.clone(),
                latest_message: "prompt".to_string(),
            },
        );
        if let Some(interrupted) = interrupted {
            head = commit_transition(
                &mut store,
                &head,
                &Transition::RequestTerminal {
                    request: request.clone(),
                    outcome: RequestOutcome::Idle {
                        result: None,
                        interrupted,
                    },
                },
            );
        }
        (store, head, request)
    }

    fn task_record(store: &FakeStore, task: &Oid) -> AsyncRecord {
        Conversation::open(store, store.head())
            .unwrap()
            .async_task(task)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn child_terminal_facts_derive_status_and_workspaces() {
        for (interrupted, expected) in [
            (false, ChildStatus::Completed),
            (true, ChildStatus::Cancelled),
        ] {
            let (store, head, request) = child_request_history(Some(interrupted));
            let (status, workspaces) = terminal_facts(&store, &head, &request).unwrap();
            assert_eq!(status, expected);
            assert_eq!(workspaces["main"].commit, oid('a'));
            assert_eq!(workspaces["main"].initial, oid('a'));
        }
    }

    #[test]
    fn child_checkpoint_refuses_a_nonterminal_request() {
        let (store, head, request) = child_request_history(None);
        assert_eq!(
            terminal_facts(&store, &head, &request).unwrap_err(),
            "child request not terminal"
        );
    }

    #[test]
    fn appends_complete() {
        let (mut store, refname, task) = conversation(true);
        let result = stored_result(&mut store, b"complete");

        append_status_with(&mut store, &refname, &task, AsyncStatus::Complete, &result).unwrap();

        let record = task_record(&store, &task);
        assert_eq!(record.status, AsyncStatus::Complete);
        assert_eq!(record.result, Some(result));
        assert_eq!(store.pushes.len(), 1);
    }

    #[test]
    fn appends_failed_with_the_failure_tree_result() {
        let (mut store, refname, task) = conversation(true);
        let result = stored_failure_tree(&mut store);

        append_status_with(&mut store, &refname, &task, AsyncStatus::Failed, &result).unwrap();

        let record = task_record(&store, &task);
        assert_eq!(record.status, AsyncStatus::Failed);
        assert_eq!(record.result, Some(result));
    }

    #[test]
    fn identical_terminal_replay_appends_nothing() {
        let (mut store, refname, task) = conversation(true);
        let result = stored_result(&mut store, b"complete");
        append_status_with(&mut store, &refname, &task, AsyncStatus::Complete, &result).unwrap();
        let settled_head = store.head().clone();
        let push_count = store.pushes.len();

        append_status_with(&mut store, &refname, &task, AsyncStatus::Complete, &result).unwrap();

        assert_eq!(store.head(), &settled_head);
        assert_eq!(store.pushes.len(), push_count);
    }

    #[test]
    fn differing_terminal_replay_is_an_error() {
        let (mut store, refname, task) = conversation(true);
        let complete = stored_result(&mut store, b"complete");
        let failed = stored_failure_tree(&mut store);
        append_status_with(
            &mut store,
            &refname,
            &task,
            AsyncStatus::Complete,
            &complete,
        )
        .unwrap();
        let settled_head = store.head().clone();
        let push_count = store.pushes.len();

        let error = append_status_with(&mut store, &refname, &task, AsyncStatus::Failed, &failed)
            .unwrap_err();

        assert!(error.contains("already settled"), "{error}");
        assert_eq!(store.head(), &settled_head);
        assert_eq!(store.pushes.len(), push_count);
    }

    #[test]
    fn absent_task_record_is_an_error() {
        let (mut store, refname, task) = conversation(false);
        let result = stored_result(&mut store, b"complete");

        assert_eq!(
            append_status_with(&mut store, &refname, &task, AsyncStatus::Complete, &result,),
            Err(format!("task {task} was never recorded on {refname}"))
        );
        assert!(store.pushes.is_empty());
    }

    #[test]
    fn cas_loss_reapplies_to_the_moved_head() {
        let (mut store, refname, task) = conversation(true);
        let original = store.head().clone();
        let concurrent = commit_transition(
            &mut store.objects,
            &original,
            &Transition::TitleSet {
                title: "Concurrent title".to_string(),
            },
        );
        store.first_push = FirstPush::MoveTo(concurrent.clone());
        let result = stored_result(&mut store, b"complete");

        append_status_with(&mut store, &refname, &task, AsyncStatus::Complete, &result).unwrap();

        assert_eq!(store.pushes.len(), 2);
        assert_ne!(store.pushes[0], store.pushes[1]);
        assert_eq!(
            store.read_commit(store.head()).unwrap().parents,
            vec![concurrent]
        );
        let conversation = Conversation::open(&store, store.head()).unwrap();
        assert_eq!(conversation.title().unwrap(), "Concurrent title");
        assert_eq!(
            conversation.async_task(&task).unwrap().unwrap().status,
            AsyncStatus::Complete
        );
    }

    #[test]
    fn lost_response_is_detected_when_candidate_is_on_the_parent_chain() {
        let (mut store, refname, task) = conversation(true);
        store.first_push = FirstPush::AcceptThenAdvance;
        let result = stored_result(&mut store, b"complete");

        append_status_with(&mut store, &refname, &task, AsyncStatus::Complete, &result).unwrap();

        assert_eq!(store.pushes.len(), 1);
        assert_eq!(
            Conversation::open(&store, store.head())
                .unwrap()
                .title()
                .unwrap(),
            "Advanced after accepted push"
        );
        assert_eq!(task_record(&store, &task).status, AsyncStatus::Complete);
    }

    #[test]
    fn target_ref_is_only_a_v3_conversation_head() {
        let valid = conversation_refs::head_ref("chat-1").unwrap();
        assert!(validate_target_ref(&valid).is_ok());
        assert!(validate_target_ref("refs/heads/main").is_err());
        assert!(validate_target_ref("refs/caos/v2/conversations/chat-1/head").is_err());
    }

    #[test]
    fn durable_hashes_are_canonical_lowercase() {
        assert!(validate_hash(&"a".repeat(40), "test hash").is_ok());
        assert!(validate_hash(&"A".repeat(40), "test hash")
            .unwrap_err()
            .contains("lowercase"));
    }
}
