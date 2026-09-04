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
        workspaces: BTreeMap::from([("main".to_string(), (oid('a'), None))]),
        files_seed: Some(files_seed),
    };
    let applied = apply(store, None, &root).expect("apply root");
    let mut head =
        mint(store, &genesis, &applied.tree, root.kind(), &signature).expect("mint root");

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
                workspace_resolution: None,
            },
            payloads: vec![("body".to_string(), b"Build it".to_vec())],
        },
        &signature,
    );

    let request = oid('1');
    let request_head = head.clone();
    commit(
        store,
        &mut head,
        Transition::RequestAdmit {
            record: request_record(request.clone(), request_head),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::RequestClaim {
            request: request.clone(),
            latest_message: "user-0".to_string(),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::RequestInterject {
            request: request.clone(),
            entry: TranscriptEntry {
                message_id: "interjection-1".to_string(),
                conversation: "golden-conversation".to_string(),
                role: Role::User,
                actor: "user".to_string(),
                request: Some(request.clone()),
                round: Some(0),
                model: None,
                blocks: vec![Block::Text {
                    text: "Adopt this checkout".to_string(),
                }],
                proposal: Some(Proposal {
                    base: oid('a'),
                    commit: oid('d'),
                    workspace_name: "main".to_string(),
                }),
                workspace_resolution: Some(WorkspaceResolution::Direct {
                    current: oid('a'),
                    output: oid('d'),
                }),
            },
            payloads: Vec::new(),
        },
        &signature,
    );
    let first_calls = vec![
        DeclaredCall {
            id: "read-call".to_string(),
            name: "read".to_string(),
        },
        DeclaredCall {
            id: "bash-call".to_string(),
            name: "bash".to_string(),
        },
    ];
    let model_applied = commit(
        store,
        &mut head,
        model_complete(
            &request,
            0,
            2,
            "assistant-1",
            &first_calls,
            "golden-conversation",
        ),
        &signature,
    );
    let read_observation = format!(
        "{}/observation",
        paths::tool_payload_dir(request.as_str(), 0, "read-call")
    );
    commit(
        store,
        &mut head,
        Transition::ToolComplete {
            record: ToolRecord {
                request: request.clone(),
                round: 0,
                id: "read-call".to_string(),
                name: "read".to_string(),
                declaration_message: "assistant-1".to_string(),
                workspace_name: None,
                input_workspace: None,
                status: ToolStatus::Complete,
                task: None,
                result: Some(ToolResult::Complete {
                    observation: read_observation,
                    proposal: None,
                }),
                workspace_resolution: None,
                files: Vec::new(),
                files_outcome: None,
            },
            payloads: vec![("observation".to_string(), b"notes".to_vec())],
            files: Vec::new(),
        },
        &signature,
    );
    let started_bash = ToolRecord {
        request: request.clone(),
        round: 0,
        id: "bash-call".to_string(),
        name: "bash".to_string(),
        declaration_message: "assistant-1".to_string(),
        workspace_name: Some("main".to_string()),
        input_workspace: Some(oid('d')),
        status: ToolStatus::Started,
        task: Some(oid('2')),
        result: None,
        workspace_resolution: None,
        files: Vec::new(),
        files_outcome: None,
    };
    commit(
        store,
        &mut head,
        Transition::ToolStart {
            record: started_bash.clone(),
        },
        &signature,
    );
    let bash_observation = format!(
        "{}/observation",
        paths::tool_payload_dir(request.as_str(), 0, "bash-call")
    );
    commit(
        store,
        &mut head,
        Transition::ToolComplete {
            record: ToolRecord {
                status: ToolStatus::Complete,
                result: Some(ToolResult::Complete {
                    observation: bash_observation,
                    proposal: Some(oid('b')),
                }),
                workspace_resolution: Some(WorkspaceResolution::Direct {
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
        paths::tool_payload_dir(request.as_str(), 1, "spawn-call")
    );
    let spawn_tool = ToolRecord {
        request: request.clone(),
        round: 1,
        id: "spawn-call".to_string(),
        name: "spawn_agent".to_string(),
        declaration_message: "assistant-2".to_string(),
        workspace_name: None,
        input_workspace: None,
        status: ToolStatus::Complete,
        task: None,
        result: Some(ToolResult::Complete {
            observation: spawn_observation,
            proposal: None,
        }),
        workspace_resolution: None,
        files: Vec::new(),
        files_outcome: None,
    };
    let child = ChildRecord {
        id: child_id.clone(),
        initial_head: head.clone(),
        initial_workspace: Some(oid('b')),
        request: request.clone(),
        relay: oid('3'),
        spawn_intent: SpawnIntent {
            request: request.clone(),
            round: 1,
            tool: "spawn-call".to_string(),
            workspace_name: Some("main".to_string()),
            input_workspace: Some(oid('b')),
            prompt: ".caos/tools/prompt".to_string(),
            model: "model".to_string(),
            configuration: configuration(),
            files_seed: None,
        },
        status: ChildStatus::Running,
        applications: Vec::new(),
        terminal_head: None,
        child_workspaces: None,
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
                status: AsyncStatus::Pending,
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
        Transition::RequestInterject {
            request: request.clone(),
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
                workspace_resolution: None,
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
            status: AsyncStatus::Complete,
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
            status: ChildStatus::Completed,
            child_workspaces: BTreeMap::from([(
                "main".to_string(),
                ChildWorkspace {
                    commit: oid('c'),
                    initial: oid('b'),
                },
            )]),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::SubagentApply {
            child: child_id,
            application: Application {
                parent_workspace_name: "main".to_string(),
                parent_workspace: Some(oid('b')),
                child_workspace: "main".to_string(),
                workspace_resolution: WorkspaceResolution::Merged {
                    current: oid('b'),
                    merge: MergeInfo {
                        base: oid('a'),
                        ours: oid('b'),
                        theirs: oid('c'),
                        implementation: "merge-v1".to_string(),
                        output: Some(oid('c')),
                        conflict_paths: None,
                    },
                    output: oid('c'),
                },
            },
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        model_complete(&request, 2, 5, "assistant-4", &[], "golden-conversation"),
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::RequestTerminal {
            request: request.clone(),
            outcome: RequestOutcome::Idle {
                result: None,
                interrupted: false,
            },
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::WorkspaceCreate {
            name: "side".to_string(),
            commit: oid('d'),
            origin: Some(WorkspaceOrigin {
                source: "seed".to_string(),
                source_tree: oid('e'),
            }),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::WorkspaceRollback {
            name: "main".to_string(),
            commit: oid('f'),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::WorkspaceRemove {
            name: "side".to_string(),
        },
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
                workspace_name: "main".to_string(),
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

    let queued_escape = oid('7');
    let queued_head = head.clone();
    commit(
        store,
        &mut head,
        Transition::RequestAdmit {
            record: request_record(queued_escape.clone(), queued_head),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::RequestEscape {
            request: queued_escape,
            reason: Some("cancel before claim".to_string()),
        },
        &signature,
    );

    let cancelling = oid('8');
    let cancelling_head = head.clone();
    commit(
        store,
        &mut head,
        Transition::RequestAdmit {
            record: request_record(cancelling.clone(), cancelling_head),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::RequestClaim {
            request: cancelling.clone(),
            latest_message: "assistant-4".to_string(),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::RequestEscape {
            request: cancelling.clone(),
            reason: Some("stop running".to_string()),
        },
        &signature,
    );
    commit(
        store,
        &mut head,
        Transition::RequestTerminal {
            request: cancelling,
            outcome: RequestOutcome::Idle {
                result: None,
                interrupted: true,
            },
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
    let parent_tree = store.read_commit(head).expect("read parent").tree;
    let applied = apply(store, Some(&parent_tree), &transition).expect("apply transition");
    *head =
        mint(store, head, &applied.tree, transition.kind(), signature).expect("mint transition");
    applied
}

fn request_record(id: Oid, request_head: Oid) -> RequestRecord {
    RequestRecord {
        id,
        request_head,
        request_workspaces: None,
        model: "model".to_string(),
        configuration: configuration(),
        round: 0,
        calls: Vec::new(),
        interjections: Vec::new(),
        status: RequestStatus::Queued,
        latest_message: None,
        escape_reason: None,
        outcome: None,
    }
}

fn model_complete(
    request: &Oid,
    round: u64,
    ordinal: u64,
    message_id: &str,
    calls: &[DeclaredCall],
    conversation: &str,
) -> Transition {
    let mut blocks = vec![Block::Text {
        text: "Working".to_string(),
    }];
    let mut payloads = Vec::new();
    for call in calls {
        let name = format!("{}-arguments", call.id);
        blocks.push(Block::ToolUse {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: format!(
                "{}/{name}",
                paths::transcript_payload_dir(ordinal, message_id)
            ),
        });
        payloads.push((name, b"{}".to_vec()));
    }
    Transition::ModelComplete {
        request: request.clone(),
        entry: TranscriptEntry {
            message_id: message_id.to_string(),
            conversation: conversation.to_string(),
            role: Role::Assistant,
            actor: "model".to_string(),
            request: Some(request.clone()),
            round: Some(round),
            model: Some("model".to_string()),
            blocks,
            proposal: None,
            workspace_resolution: None,
        },
        payloads,
        calls: calls.to_vec(),
    }
}

fn configuration() -> String {
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string()
}

fn oid(character: char) -> Oid {
    Oid::parse(&character.to_string().repeat(40), "fixture oid").expect("valid fixture oid")
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use crate::v3::tree::MemoryStore;

    const GOLDEN_HEAD: &str = "cc1df48f8601195e11f30072e684e06f60ed562b";
    const GOLDEN_TREES: [&str; 33] = [
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        "a398d70aa573940890a0877daddfcd1bd8dec0f8",
        "b10226eb938788dae4ca857e7333218843b57c20",
        "828429dd06a79479a9e0652f186d5f5b9534ab85",
        "30fd822540cf7caab3a9535df99e7fb46938f5e9",
        "fb62e9f83c59dd35c6a0b24ea3fef2d1a4f55ccd",
        "bb96722aebb12654a1957e90cf1ce6db8c676e96",
        "6cddae8c89b579277793c5021b191c0d1462df4a",
        "76374b5318f392c83d84212472251d37a1eb5a85",
        "e440db3d32a4de40afd32ead5a4aaf1850429d03",
        "56f766f8af868960f42bd80cc8767946ce183d55",
        "241fe3fe45263205123d93942098d00a5d780e94",
        "6eae194b77e008fca3db31db0500c3db3481c954",
        "4c15e9cf7a524fa3ce3b9380f494ed41d26a3b3e",
        "e5f686e5d604841640b49e761791f4e30e768356",
        "8ac550e6fb2aaddb5b6d95d8446b0d3a92237c94",
        "6fd133d180f51e36105f39ba67c98f196c96ef9b",
        "d2c119718013193f409e07664789dcc1258322f9",
        "acbba4378f5062a40cb5465923010a4682d3c34c",
        "60bba3b2c482c7d2954f0f2e4117ec01003b739e",
        "02d6de0bb3eb05afb066d97d68c07e16f9c5ea8e",
        "8209fee51b0400cbf65d97dd521bfc7dcc87f6e2",
        "b0854834cbc76008973375a1b7d170d727018100",
        "0b83c09973f68c2620dc33e37c8e248f96162107",
        "c752ff5ec3119971670fa1fcf05457e2f4a99b8f",
        "49717d887f9becfdf59f4edc9f471a3b78cf4b17",
        "6c3662fcb9e4ef2e267aadfaf7eb122f952b8b59",
        "0a8a7d38e77d50f27b1a5704ea93a6081137cfd0",
        "d8ec34900a7741cd2c6f8e0026f7efcc7bcbca6b",
        "dfde41a608e79d9e4c8b4ebfe381bf7420d37ac5",
        "b61549f48b3800ebf3c3d84b2a0aed90381b721f",
        "d8ebacbcd89e88512ad3a785e52f6cc879082a50",
        "3b9aaddc6b0d2d5f685434644ea2187ef25aea87",
    ];

    #[test]
    fn golden_spine_oids_are_stable() {
        let mut store = MemoryStore::new();
        let head = golden(&mut store);
        assert_eq!(head.as_str(), GOLDEN_HEAD);
        let mut trees = Vec::new();
        let mut cursor = head;
        loop {
            let commit = store.read_commit(&cursor).unwrap();
            trees.push(commit.tree);
            let Some(parent) = commit.parents.first() else {
                break;
            };
            cursor = parent.clone();
        }
        trees.reverse();
        assert_eq!(
            trees.iter().map(Oid::as_str).collect::<Vec<_>>(),
            GOLDEN_TREES
        );
    }
}
