use std::collections::BTreeMap;

use super::apply::{apply, client_signature, mint, Applied, Transition};
use super::ids;
use super::oid::{ensure_genesis, Oid};
use super::paths;
use super::records::*;
use super::tree::{Mode, ObjectStore, TreeBuilder};

pub fn golden(store: &mut dyn ObjectStore) -> Oid {
    golden_with_applied(store).0
}

pub(crate) fn golden_with_applied(store: &mut dyn ObjectStore) -> (Oid, [Applied; 2]) {
    let genesis = ensure_genesis(store).expect("mint genesis");
    let signature = client_signature("Golden", "golden@example.com", 1_700_000_000);
    let mut seed = TreeBuilder::from(None);
    seed.put("notes.md", Mode::Blob, b"seeded notes\n".to_vec());
    let files_seed = seed.build(store).expect("build files seed");
    let root = Transition::ConversationRoot {
        identity: Identity {
            id: "golden-conversation".to_string(),
            kind: IdentityKind::Root,
            owner: None,
        },
        title: "Golden Conversation".to_string(),
        content: Some({
            let mut content = crate::v3::tree::TreeBuilder::from(Some(files_seed.clone()));
            for (name, commit) in BTreeMap::from([("main".to_string(), oid('a'))]) {
                content.put_oid(&name, crate::v3::Mode::Commit, commit);
            }
            content.build(store).unwrap()
        }),
    };
    let applied = apply(store, None, &root).expect("apply root");
    let mut head = mint(store, &genesis, &applied, root.kind(), &signature).expect("mint root");

    commit(
        store,
        &mut head,
        Transition::TitleSet {
            title: "Golden Conversation Updated".to_string(),
        },
        &signature,
    );
    let message_applied = commit(
        store,
        &mut head,
        Transition::MessageAppend {
            entry: TranscriptEntry {
                message_id: "user-0".to_string(),
                conversation: "golden-conversation".to_string(),
                role: Role::User,
                actor: "user".to_string(),
                request: None,
                round: None,
                model: None,
                blocks: vec![Block::Payload {
                    path: format!("{}/body", paths::transcript_payload_dir(0, "user-0")),
                }],
                proposal: None,
                source_tree_resolution: None,
            },
            payloads: vec![("body".to_string(), b"Build it".to_vec())],
        },
        &signature,
    );

    commit(
        store,
        &mut head,
        Transition::MessageAppend {
            entry: TranscriptEntry {
                message_id: "interjection-1".to_string(),
                conversation: "golden-conversation".to_string(),
                role: Role::User,
                actor: "user".to_string(),
                model: None,
                blocks: vec![Block::Text {
                    text: "Adopt this checkout".to_string(),
                }],
                proposal: Some(Proposal {
                    base: oid('a'),
                    commit: oid('d'),
                    source_tree_name: "main".to_string(),
                }),
                source_tree_resolution: Some(SourceTreeResolution::Direct {
                    current: oid('a'),
                    output: oid('d'),
                }),
            },
            payloads: Vec::new(),
        },
        &signature,
    );
    let model_applied = commit(
        store,
        &mut head,
        assistant_message(
            2,
            "assistant-1",
            &[("read-call", "read"), ("bash-call", "bash")],
            "golden-conversation",
        ),
        &signature,
    );
    let read_observation = format!(
        "{}/observation",
        paths::call_payload_dir("read-call")
    );
    commit(
        store,
        &mut head,
        Transition::ToolComplete {
            record: CallRecord {
                id: "read-call".to_string(),
                name: "read".to_string(),
                declaration_message: "assistant-1".to_string(),
                source_tree_name: None,
                input_commit: None,
                status: CallStatus::Complete,
                task: None,
                result: Some(ToolResult::Complete {
                    observation: read_observation,
                    proposal: None,
                }),
                source_tree_resolution: None,
                files: Vec::new(),
                files_outcome: None,
            },
            payloads: vec![("observation".to_string(), b"notes".to_vec())],
            files: Vec::new(),
        },
        &signature,
    );
    let started_bash = CallRecord {
        id: "bash-call".to_string(),
        name: "bash".to_string(),
        declaration_message: "assistant-1".to_string(),
        source_tree_name: Some("main".to_string()),
        input_commit: Some(oid('d')),
        status: CallStatus::Started,
        task: Some(oid('2')),
        result: None,
        source_tree_resolution: None,
        files: Vec::new(),
        files_outcome: None,
    };
    commit(
        store,
        &mut head,
        Transition::ToolStart {
            payloads: Vec::new(),
            record: started_bash.clone(),
        },
        &signature,
    );
    let bash_observation = format!(
        "{}/observation",
        paths::call_payload_dir("bash-call")
    );
    commit(
        store,
        &mut head,
        Transition::ToolComplete {
            record: CallRecord {
                status: CallStatus::Complete,
                result: Some(ToolResult::Complete {
                    observation: bash_observation,
                    proposal: Some(oid('b')),
                }),
                source_tree_resolution: Some(SourceTreeResolution::Direct {
                    current: oid('d'),
                    output: oid('b'),
                }),
                files: vec!["tool.txt".to_string()],
                files_outcome: Some(FilesOutcome {
                    applied: vec!["tool.txt".to_string()],
                    conflicted: Vec::new(),
                }),
                ..started_bash
            },
            payloads: vec![("observation".to_string(), b"changed".to_vec())],
            files: vec![(
                "tool.txt".to_string(),
                Some((Mode::Blob, b"tool output\n".to_vec())),
            )],
        },
        &signature,
    );

    let second_calls = vec![
        DeclaredCall {
            id: "spawn-call".to_string(),
            name: "spawn_agent".to_string(),
        },
        DeclaredCall {
            id: "async-call".to_string(),
            name: "run_async".to_string(),
        },
    ];
    commit(
        store,
        &mut head,
        model_complete(
            &request,
            1,
            3,
            "assistant-2",
            &second_calls,
            "golden-conversation",
        ),
        &signature,
    );
    let child_id =
        ids::child_id("golden-conversation", &request, 1, "spawn-call").expect("derive child id");
    let spawn_observation = format!(
        "{}/observation",
        paths::call_payload_dir("spawn-call")
    );
    let spawn_tool = CallRecord {
        id: "spawn-call".to_string(),
        name: "spawn_agent".to_string(),
        declaration_message: "assistant-2".to_string(),
        source_tree_name: None,
        input_commit: None,
        status: CallStatus::Complete,
        task: None,
        result: Some(ToolResult::Complete {
            observation: spawn_observation,
            proposal: None,
        }),
        source_tree_resolution: None,
        files: Vec::new(),
        files_outcome: None,
    };
    let child = ChildRecord {
        id: child_id.clone(),
        initial_head: head.clone(),
        relay: oid('3'),
        spawn_intent: SpawnIntent {
            tool: "spawn-call".to_string(),
            prompt: ".caos/tools/prompt".to_string(),
            model: "model".to_string(),
            configuration: configuration(),
            content: files_seed,
        },
        status: TaskStatus::Pending,
        terminal_head: None,
    };
    commit(
        store,
        &mut head,
        Transition::SubagentSpawn {
            tool: spawn_tool,
            payloads: vec![("observation".to_string(), b"spawned".to_vec())],
            child,
        },
        &signature,
    );
    let async_task = oid('4');
    commit(
        store,
        &mut head,
        Transition::AsyncStart {
            record: AsyncRecord {
                task: async_task.clone(),
                status: TaskStatus::Pending,
                target_ref: Some("refs/heads/main".to_string()),
                result: None,
                reason: None,
            },
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::TurnInterject {
            entry: TranscriptEntry {
                message_id: "interjection-3".to_string(),
                conversation: "golden-conversation".to_string(),
                role: Role::User,
                actor: "user".to_string(),
                request: Some(request.clone()),
                round: Some(2),
                model: None,
                blocks: vec![Block::Text {
                    text: "One more detail".to_string(),
                }],
                proposal: None,
                source_tree_resolution: None,
            },
            payloads: Vec::new(),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::AsyncTerminal {
            task: async_task,
            status: TaskStatus::Complete,
            result: Some(oid('5')),
            reason: None,
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::SubagentTerminal {
            child: child_id.clone(),
            terminal_head: oid('6'),
            status: TaskStatus::Complete,
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        assistant_message(5, "assistant-4", &[], "golden-conversation"),
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::TurnTerminal {
            outcome: TurnOutcome::Idle {
                result: None,
                interrupted: false,
            },
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::reference("side".to_string(), Some(oid('d'))),
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::reference("main".to_string(), Some(oid('f'))),
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::reference("side".to_string(), None),
        &signature,
    );

    let descriptor = Descriptor {
        source_base: oid('a'),
        source_head: oid('f'),
        target_base: oid('a'),
        policy: "squash".to_string(),
        implementation: "project-v1".to_string(),
        commit_policy: "single".to_string(),
    };
    let projection = ids::projection_id(&descriptor.to_value()).expect("derive projection id");
    let planned_head = oid('f');
    let expected_old = oid('a');
    let publication_id = ids::publication_id(
        "golden-conversation",
        "client-key",
        &projection,
        &planned_head,
        "repository",
        "refs/heads/main",
        Some(&expected_old),
    )
    .expect("derive publication id");
    commit(
        store,
        &mut head,
        Transition::PublicationPending {
            record: PublicationRecord {
                id: publication_id.clone(),
                key: "client-key".to_string(),
                descriptor,
                planned_head,
                repository: "repository".to_string(),
                refname: "refs/heads/main".to_string(),
                expected_old: Some(expected_old),
                source_tree_name: "main".to_string(),
                status: PublicationStatus::Pending,
                evidence: None,
                observed: None,
            },
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::PublicationTerminal {
            publication: publication_id,
            status: PublicationStatus::Complete,
            evidence: Evidence {
                kind: "push-success".to_string(),
                diagnostic: Some("published".to_string()),
            },
            observed: Some(oid('f')),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::FilesApply {
            files: vec![(
                "final.txt".to_string(),
                Some((Mode::Blob, b"final\n".to_vec())),
            )],
        },
        &signature,
    );

    commit(
        store,
        &mut head,
        Transition::MessageAppend {
            entry: TranscriptEntry {
                message_id: "interrupt-6".to_string(),
                conversation: "golden-conversation".to_string(),
                role: Role::System,
                actor: "user".to_string(),
                model: None,
                blocks: vec![Block::Text {
                    text: INTERRUPT_TEXT.to_string(),
                }],
                proposal: None,
                source_tree_resolution: None,
            },
            payloads: Vec::new(),
        },
        &signature,
    );

    let fork = Transition::ConversationFork {
        identity: Identity {
            id: "golden-fork".to_string(),
            kind: IdentityKind::Fork {
                source: head.clone(),
            },
            owner: None,
        },
        title: "Golden Fork".to_string(),
        content: None,
    };
    commit(store, &mut head, fork, &signature);
    (head, [message_applied, model_applied])
}

fn commit(
    store: &mut dyn ObjectStore,
    head: &mut Oid,
    transition: Transition,
    signature: &super::tree::Signature,
) -> Applied {
    let applied = apply(store, Some(head), &transition).expect("apply transition");
    *head = mint(store, head, &applied, transition.kind(), signature).expect("mint transition");
    applied
}

fn assistant_message(
    ordinal: u64,
    message_id: &str,
    calls: &[(&str, &str)],
    conversation: &str,
) -> Transition {
    let mut blocks = vec![Block::Text {
        text: "Working".to_string(),
    }];
    let mut payloads = Vec::new();
    for (id, tool) in calls {
        let name = format!("{id}-arguments");
        blocks.push(Block::ToolUse {
            id: id.to_string(),
            name: tool.to_string(),
            arguments: format!(
                "{}/{name}",
                paths::transcript_payload_dir(ordinal, message_id)
            ),
        });
        payloads.push((name, b"{}".to_vec()));
    }
    Transition::MessageAppend {
        entry: TranscriptEntry {
            message_id: message_id.to_string(),
            conversation: conversation.to_string(),
            role: Role::Assistant,
            actor: "model".to_string(),
            model: Some("model".to_string()),
            blocks,
            proposal: None,
            source_tree_resolution: None,
        },
        payloads,
    }
}

fn configuration() -> String {
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string()
}

fn oid(character: char) -> Oid {
    Oid::parse(&character.to_string().repeat(40), "fixture oid").expect("valid fixture oid")
}
