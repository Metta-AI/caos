use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use conversation_protocol::v3::apply::{apply, client_signature, mint, Transition};
use conversation_protocol::v3::oid::{ensure_genesis, g3, hex_lower};
use conversation_protocol::v3::paths;
use conversation_protocol::v3::records::{
    AsyncRecord, AsyncStatus, Block, Identity, IdentityKind, RequestOutcome, RequestRecord,
    RequestStatus, Role, ToolStatus, TranscriptEntry,
};
use conversation_protocol::v3::refs;
use conversation_protocol::v3::view::Conversation;
use conversation_protocol::v3::{
    validate_spine, GitStore, Kind, Mode, ObjectStore, Oid, RefUpdate, TreeEntry,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum ToolCommand {
    ChildId {
        parent: String,
        request: Oid,
        round: u64,
        tool: String,
    },
    Ref {
        id: String,
    },
    Root {
        repo: PathBuf,
        id: String,
        title: String,
        workspaces: Vec<(String, Oid)>,
    },
    Turn {
        repo: PathBuf,
        id: String,
        user: String,
        title: String,
        workspaces: Vec<(String, Oid)>,
        head: Option<Oid>,
        actor: String,
        text: String,
        secret_hash: Oid,
        model: String,
        configuration: Oid,
    },
    Interject {
        repo: PathBuf,
        head: Option<Oid>,
        request: Oid,
        actor: String,
        text: String,
        refname: Option<String>,
    },
    Escape {
        repo: PathBuf,
        head: Option<Oid>,
        request: Oid,
        refname: Option<String>,
    },
    Create {
        repo: PathBuf,
        refname: String,
        user: String,
        id: String,
        new: Oid,
    },
    Fetch {
        repo: PathBuf,
        refname: String,
    },
    Read {
        repo: PathBuf,
        head: Oid,
        path: String,
    },
    Request {
        repo: PathBuf,
        head: Oid,
        id: Option<Oid>,
    },
    WaitTerminal {
        repo: PathBuf,
        refname: String,
        request: Oid,
        timeout_secs: u64,
    },
    Workspace {
        repo: PathBuf,
        head: Oid,
        name: String,
    },
    Transcript {
        repo: PathBuf,
        head: Oid,
    },
    Tools {
        repo: PathBuf,
        head: Oid,
        request: Oid,
    },
    ToolObservation {
        repo: PathBuf,
        head: Oid,
        request: Oid,
        round: u64,
        id: String,
    },
    Async {
        repo: PathBuf,
        head: Oid,
        task: Option<Oid>,
    },
    AsyncStart {
        repo: PathBuf,
        head: Oid,
        task: Oid,
        target_ref: Option<String>,
        refname: Option<String>,
    },
    Children {
        repo: PathBuf,
        head: Oid,
    },
    Parents {
        repo: PathBuf,
        head: Oid,
        validate: bool,
        started_tool: Option<(Oid, String)>,
        present_path: Option<String>,
    },
}

#[derive(Debug)]
struct ToolError {
    code: u8,
    message: Option<String>,
}

impl ToolError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: Some(message.into()),
        }
    }

    fn code(code: u8) -> Self {
        Self {
            code,
            message: None,
        }
    }
}

impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1).collect()).and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(message) = error.message {
                eprintln!("llm-test-tool: {message}");
            }
            ExitCode::from(error.code)
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<ToolCommand, ToolError> {
    let mut args = Arguments::new(args);
    let command = args.next("subcommand")?;
    let parsed = match command.as_str() {
        "child-id" => ToolCommand::ChildId {
            parent: args.required("--parent")?,
            request: args.oid("--request", "parent request")?,
            round: parse_u64(&args.required("--round")?, "round")?,
            tool: args.required("--tool")?,
        },
        "ref" => ToolCommand::Ref {
            id: args.required("--id")?,
        },
        "root" => {
            let repo = args.repo()?;
            let id = args.required("--id")?;
            let title = args.required("--title")?;
            let workspaces = args.workspaces()?;
            ToolCommand::Root {
                repo,
                id,
                title,
                workspaces,
            }
        }
        "turn" => {
            let repo = args.repo()?;
            let id = args.required("--id")?;
            let user = args.required("--user")?;
            let title = args.required("--title")?;
            let head = args
                .take("--head")?
                .map(|value| parse_oid(&value, "conversation head"))
                .transpose()?;
            let actor = args.required("--actor")?;
            let text = args.required("--text")?;
            let secret_hash = args.oid("--secret-hash", "secret hash")?;
            let model = args.required("--model")?;
            let configuration = args.oid("--configuration", "model configuration")?;
            let workspaces = args.workspaces()?;
            ToolCommand::Turn {
                repo,
                id,
                user,
                title,
                workspaces,
                head,
                actor,
                text,
                secret_hash,
                model,
                configuration,
            }
        }
        "interject" => ToolCommand::Interject {
            repo: args.repo()?,
            head: args
                .take("--head")?
                .map(|value| parse_oid(&value, "conversation head"))
                .transpose()?,
            request: args.oid("--request", "request")?,
            actor: args.required("--actor")?,
            text: args.required("--text")?,
            refname: args.take("--ref")?,
        },
        "escape" => ToolCommand::Escape {
            repo: args.repo()?,
            head: args
                .take("--head")?
                .map(|value| parse_oid(&value, "conversation head"))
                .transpose()?,
            request: args.oid("--request", "request")?,
            refname: args.take("--ref")?,
        },
        "create" => ToolCommand::Create {
            repo: args.repo()?,
            refname: args.required("--ref")?,
            user: args.required("--user")?,
            id: args.required("--id")?,
            new: args.oid("--new", "new head")?,
        },
        "fetch" => ToolCommand::Fetch {
            repo: args.repo()?,
            refname: args.required("--ref")?,
        },
        "read" => ToolCommand::Read {
            repo: args.repo()?,
            head: args.oid("--head", "conversation head")?,
            path: args.required("--path")?,
        },
        "request" => ToolCommand::Request {
            repo: args.repo()?,
            head: args.oid("--head", "conversation head")?,
            id: args
                .take("--id")?
                .map(|value| parse_oid(&value, "request"))
                .transpose()?,
        },
        "wait-terminal" => ToolCommand::WaitTerminal {
            repo: args.repo()?,
            refname: args.required("--ref")?,
            request: args.oid("--request", "request")?,
            timeout_secs: args
                .take("--timeout-secs")?
                .map(|value| parse_u64(&value, "timeout"))
                .transpose()?
                .unwrap_or(120),
        },
        "workspace" => ToolCommand::Workspace {
            repo: args.repo()?,
            head: args.oid("--head", "conversation head")?,
            name: args.required("--name")?,
        },
        "transcript" => ToolCommand::Transcript {
            repo: args.repo()?,
            head: args.oid("--head", "conversation head")?,
        },
        "tools" => ToolCommand::Tools {
            repo: args.repo()?,
            head: args.oid("--head", "conversation head")?,
            request: args.oid("--request", "request")?,
        },
        "tool-observation" => ToolCommand::ToolObservation {
            repo: args.repo()?,
            head: args.oid("--head", "conversation head")?,
            request: args.oid("--request", "request")?,
            round: parse_u64(&args.required("--round")?, "round")?,
            id: args.required("--id")?,
        },
        "async" => ToolCommand::Async {
            repo: args.repo()?,
            head: args.oid("--head", "conversation head")?,
            task: args
                .take("--task")?
                .map(|value| parse_oid(&value, "async task"))
                .transpose()?,
        },
        "async-start" => ToolCommand::AsyncStart {
            repo: args.repo()?,
            head: args.oid("--head", "conversation head")?,
            task: args.oid("--task", "async task")?,
            target_ref: args.take("--target-ref")?,
            refname: args.take("--ref")?,
        },
        "children" => ToolCommand::Children {
            repo: args.repo()?,
            head: args.oid("--head", "conversation head")?,
        },
        "parents" => {
            let repo = args.repo()?;
            let head = args.oid("--head", "conversation head")?;
            let validate = args.flag("--validate");
            let request = args.take("--request")?;
            let started_tool = args.take("--started-tool")?;
            let present_path = args.take("--present-path")?;
            let started_tool = match (request, started_tool) {
                (None, None) => None,
                (Some(request), Some(tool)) => Some((parse_oid(&request, "request")?, tool)),
                _ => {
                    return Err(ToolError::new(
                        "parents needs both --request and --started-tool",
                    ))
                }
            };
            ToolCommand::Parents {
                repo,
                head,
                validate,
                started_tool,
                present_path,
            }
        }
        _ => return Err(ToolError::new(format!("unknown subcommand {command:?}"))),
    };
    args.finish()?;
    Ok(parsed)
}

struct Arguments {
    values: Vec<String>,
}

impl Arguments {
    fn new(values: Vec<String>) -> Self {
        Self { values }
    }

    fn next(&mut self, what: &str) -> Result<String, ToolError> {
        if self.values.is_empty() {
            return Err(ToolError::new(format!("missing {what}")));
        }
        Ok(self.values.remove(0))
    }

    fn take(&mut self, flag: &str) -> Result<Option<String>, ToolError> {
        let Some(index) = self.values.iter().position(|value| value == flag) else {
            return Ok(None);
        };
        self.values.remove(index);
        if index == self.values.len() {
            return Err(ToolError::new(format!("missing value for {flag}")));
        }
        Ok(Some(self.values.remove(index)))
    }

    fn required(&mut self, flag: &str) -> Result<String, ToolError> {
        self.take(flag)?
            .ok_or_else(|| ToolError::new(format!("missing {flag}")))
    }

    fn oid(&mut self, flag: &str, what: &str) -> Result<Oid, ToolError> {
        parse_oid(&self.required(flag)?, what)
    }

    fn repo(&mut self) -> Result<PathBuf, ToolError> {
        self.required("--repo").map(PathBuf::from)
    }

    fn flag(&mut self, flag: &str) -> bool {
        let Some(index) = self.values.iter().position(|value| value == flag) else {
            return false;
        };
        self.values.remove(index);
        true
    }

    fn workspaces(&mut self) -> Result<Vec<(String, Oid)>, ToolError> {
        let mut workspaces = Vec::new();
        while let Some(value) = self.take("--workspace")? {
            let (name, commit) = value
                .split_once('=')
                .ok_or_else(|| ToolError::new("--workspace must be <name>=<commit>"))?;
            paths::validate_workspace_name(name).map_err(ToolError::new)?;
            workspaces.push((name.to_string(), parse_oid(commit, "workspace commit")?));
        }
        Ok(workspaces)
    }

    fn finish(self) -> Result<(), ToolError> {
        if self.values.is_empty() {
            Ok(())
        } else {
            Err(ToolError::new(format!(
                "unexpected arguments: {}",
                self.values.join(" ")
            )))
        }
    }
}

fn parse_oid(value: &str, what: &str) -> Result<Oid, ToolError> {
    Oid::parse(value, what).map_err(ToolError::new)
}

fn parse_u64(value: &str, what: &str) -> Result<u64, ToolError> {
    value
        .parse()
        .map_err(|error| ToolError::new(format!("invalid {what} {value:?}: {error}")))
}

fn run(command: ToolCommand) -> Result<(), ToolError> {
    match command {
        ToolCommand::ChildId {
            parent,
            request,
            round,
            tool,
        } => println!(
            "child {}",
            conversation_protocol::v3::ids::child_id(&parent, &request, round, &tool)?
        ),
        ToolCommand::Ref { id } => println!("ref {}", refs::head_ref(&id)?),
        ToolCommand::Root {
            repo,
            id,
            title,
            workspaces,
        } => {
            let mut store = open(&repo)?;
            let head = conversation_root(&mut store, id, title, workspaces)?;
            println!("head {head}");
        }
        ToolCommand::Turn {
            repo,
            id,
            user,
            title,
            workspaces,
            head,
            actor,
            text,
            secret_hash,
            model,
            configuration,
        } => turn(TurnInput {
            repo,
            id,
            user,
            title,
            workspaces,
            head,
            actor,
            text,
            secret_hash,
            model,
            configuration,
        })?,
        ToolCommand::Interject {
            repo,
            head,
            request,
            actor,
            text,
            refname,
        } => {
            let mut store = open(&repo)?;
            let head = match head {
                Some(head) => {
                    store.ensure_local(&head)?;
                    head
                }
                None => {
                    let refname = refname
                        .as_ref()
                        .ok_or_else(|| ToolError::new("interject needs either --head or --ref"))?;
                    store
                        .fetch_ref(refname)?
                        .ok_or_else(|| ToolError::new(format!("ref {refname} is absent")))?
                }
            };
            let view = Conversation::open(&store, &head)?;
            let id = view.identity()?.id;
            let request_record = view
                .request(&request)?
                .ok_or_else(|| ToolError::new(format!("request {request} does not exist")))?;
            let round = request_record.round;
            let status = request_status_name(&request_record.status);
            drop(view);
            let message_id = client_key()?;
            let transition = Transition::RequestInterject {
                request: request.clone(),
                entry: TranscriptEntry {
                    message_id: message_id.clone(),
                    conversation: id,
                    role: Role::User,
                    actor,
                    request: Some(request),
                    round: Some(round),
                    model: None,
                    blocks: vec![Block::Text { text }],
                    proposal: None,
                    workspace_resolution: None,
                },
                payloads: Vec::new(),
            };
            let new = append(&mut store, &head, transition)?;
            if let Some(refname) = refname {
                push_with_lease(&store, refname, &head, &new)?;
            }
            println!("head {new}");
            println!("parent {head}");
            println!("status {status}");
            println!("message {message_id}");
        }
        ToolCommand::Escape {
            repo,
            head,
            request,
            refname,
        } => {
            let mut store = open(&repo)?;
            let head = match head {
                Some(head) => {
                    store.ensure_local(&head)?;
                    head
                }
                None => {
                    let refname = refname
                        .as_ref()
                        .ok_or_else(|| ToolError::new("escape needs either --head or --ref"))?;
                    store
                        .fetch_ref(refname)?
                        .ok_or_else(|| ToolError::new(format!("ref {refname} is absent")))?
                }
            };
            let new = append(
                &mut store,
                &head,
                Transition::RequestEscape {
                    request: request.clone(),
                    reason: None,
                },
            )?;
            let request_record = Conversation::open(&store, &new)?
                .request(&request)?
                .ok_or_else(|| ToolError::new(format!("request {request} does not exist")))?;
            let interrupted = matches!(
                request_record.outcome,
                Some(RequestOutcome::Idle {
                    interrupted: true,
                    ..
                })
            );
            if let Some(refname) = refname {
                push_with_lease(&store, refname, &head, &new)?;
            }
            println!("head {new}");
            println!("interrupted {interrupted}");
            println!("status {}", request_status_name(&request_record.status));
        }
        ToolCommand::Create {
            repo,
            refname,
            user,
            id,
            new,
        } => {
            let store = open(&repo)?;
            let active = refs::active_membership_ref(&user, &id)?;
            let archived = refs::archived_membership_ref(&user, &id)?;
            store.push(&[
                RefUpdate {
                    refname,
                    expected: None,
                    new: Some(new.clone()),
                },
                RefUpdate {
                    refname: active,
                    expected: None,
                    new: Some(new.clone()),
                },
                RefUpdate {
                    refname: archived,
                    expected: None,
                    new: None,
                },
            ])?;
            println!("pushed {new}");
        }
        ToolCommand::Fetch { repo, refname } => {
            let store = open(&repo)?;
            let Some(head) = store.fetch_ref(&refname)? else {
                return Err(ToolError::code(4));
            };
            println!("head {head}");
        }
        ToolCommand::Read { repo, head, path } => {
            let store = open(&repo)?;
            store.ensure_local(&head)?;
            let view = Conversation::open(&store, &head)?;
            let bytes = view
                .snapshot()
                .read(&path)?
                .ok_or_else(|| ToolError::new(format!("path {path:?} is absent")))?;
            std::io::stdout().write_all(&bytes).map_err(|error| {
                ToolError::new(format!("writing record to standard output: {error}"))
            })?;
        }
        ToolCommand::Request { repo, head, id } => {
            let store = open(&repo)?;
            store.ensure_local(&head)?;
            let view = Conversation::open(&store, &head)?;
            let record = match id {
                Some(id) => view.request(&id)?,
                None => view.active_request()?,
            };
            match record {
                Some(record) => write_record(&record.encode())?,
                None => println!("none"),
            }
        }
        ToolCommand::WaitTerminal {
            repo,
            refname,
            request,
            timeout_secs,
        } => wait_terminal(&repo, &refname, &request, timeout_secs)?,
        ToolCommand::Workspace { repo, head, name } => {
            let store = open(&repo)?;
            store.ensure_local(&head)?;
            let workspace = Conversation::open(&store, &head)?
                .workspace(&name)?
                .ok_or_else(|| ToolError::code(4))?;
            println!("commit {}", workspace.commit);
            println!("initial {}", workspace.initial);
        }
        ToolCommand::Transcript { repo, head } => {
            let store = open(&repo)?;
            store.ensure_local(&head)?;
            let view = Conversation::open(&store, &head)?;
            for (ordinal, message_id, entry) in view.transcript(0, view.transcript_len()?)? {
                let role = match entry.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => "system",
                };
                let text = text_blocks(&entry.blocks);
                println!(
                    "{ordinal} {role} {} {message_id} {}",
                    entry.actor,
                    serde_json::to_string(&text)
                        .map_err(|error| ToolError::new(error.to_string()))?
                );
            }
        }
        ToolCommand::Tools {
            repo,
            head,
            request,
        } => {
            let store = open(&repo)?;
            store.ensure_local(&head)?;
            let view = Conversation::open(&store, &head)?;
            let record = view
                .request(&request)?
                .ok_or_else(|| ToolError::new(format!("request {request} does not exist")))?;
            for round in 0..record.round {
                for tool in view.tools(&request, round)? {
                    write_record(&tool.encode())?;
                }
            }
        }
        ToolCommand::ToolObservation {
            repo,
            head,
            request,
            round,
            id,
        } => {
            let store = open(&repo)?;
            store.ensure_local(&head)?;
            let path = format!(
                "{}/observation.json",
                paths::tool_payload_dir(request.as_str(), round, &id)
            );
            let bytes = Conversation::open(&store, &head)?.payload(&path)?;
            std::io::stdout().write_all(&bytes).map_err(|error| {
                ToolError::new(format!("writing observation to standard output: {error}"))
            })?;
        }
        ToolCommand::Async { repo, head, task } => {
            let store = open(&repo)?;
            store.ensure_local(&head)?;
            let view = Conversation::open(&store, &head)?;
            if let Some(task) = task {
                match view.async_task(&task)? {
                    Some(record) => write_record(&record.encode())?,
                    None => println!("none"),
                }
            } else {
                for record in view.async_tasks()? {
                    write_record(&record.encode())?;
                }
            }
        }
        ToolCommand::AsyncStart {
            repo,
            head,
            task,
            target_ref,
            refname,
        } => {
            let mut store = open(&repo)?;
            store.ensure_local(&head)?;
            let new = append(
                &mut store,
                &head,
                Transition::AsyncStart {
                    record: AsyncRecord {
                        task,
                        status: AsyncStatus::Pending,
                        target_ref,
                        result: None,
                        reason: None,
                    },
                },
            )?;
            if let Some(refname) = refname {
                push_with_lease(&store, refname, &head, &new)?;
            }
            println!("head {new}");
        }
        ToolCommand::Children { repo, head } => {
            let store = open(&repo)?;
            store.ensure_local(&head)?;
            for record in Conversation::open(&store, &head)?.children()? {
                write_record(&record.encode())?;
            }
        }
        ToolCommand::Parents {
            repo,
            head,
            validate,
            started_tool,
            present_path,
        } => {
            let store = open(&repo)?;
            store.ensure_local(&head)?;
            if validate {
                validate_spine(&store, &head, &mut HashSet::new())
                    .map_err(|error| ToolError::new(error.to_string()))?;
            }
            parents(&store, head, started_tool.as_ref(), present_path.as_deref())?;
        }
    }
    Ok(())
}

struct TurnInput {
    repo: PathBuf,
    id: String,
    user: String,
    title: String,
    workspaces: Vec<(String, Oid)>,
    head: Option<Oid>,
    actor: String,
    text: String,
    secret_hash: Oid,
    model: String,
    configuration: Oid,
}

struct RequestTreeEntry {
    mode: &'static str,
    kind: &'static str,
    oid: Oid,
}

fn turn(input: TurnInput) -> Result<(), ToolError> {
    let mut fetch = vec![input.secret_hash.clone(), input.configuration.clone()];
    if let Some(head) = &input.head {
        fetch.push(head.clone());
    }
    fetch_objects(&input.repo, &fetch)?;

    let mut store = open(&input.repo)?;
    let creating = input.head.is_none();
    let prior = match input.head {
        Some(head) => head,
        None => conversation_root(&mut store, input.id.clone(), input.title, input.workspaces)?,
    };
    let (human, _) = append_user_message(&mut store, &prior, input.actor, input.text)?;
    let request = prepare_turn_request(
        &input.repo,
        &mut store,
        &input.configuration,
        &human,
        &input.secret_hash,
    )?;
    let admitted = admit_request(
        &mut store,
        &human,
        request.clone(),
        input.model,
        input.configuration.to_string(),
    )?;

    let refname = refs::head_ref(&input.id)?;
    push_plain(
        &input.repo,
        &[
            (human.clone(), format!("refs/caos/req/{human}")),
            (request.clone(), format!("refs/caos/req/{request}")),
        ],
    )?;
    let mut updates = vec![RefUpdate {
        refname: refname.clone(),
        expected: (!creating).then_some(prior.clone()),
        new: Some(admitted.clone()),
    }];
    if creating {
        updates.extend([
            RefUpdate {
                refname: refs::active_membership_ref(&input.user, &input.id)?,
                expected: None,
                new: Some(admitted.clone()),
            },
            RefUpdate {
                refname: refs::archived_membership_ref(&input.user, &input.id)?,
                expected: None,
                new: None,
            },
        ]);
    }
    push_updates(&store, updates, &refname, (!creating).then_some(&prior))?;

    println!("head {admitted}");
    println!("request {request}");
    println!("human {human}");
    println!("ref {refname}");
    Ok(())
}

fn conversation_root(
    store: &mut GitStore,
    id: String,
    title: String,
    workspaces: Vec<(String, Oid)>,
) -> Result<Oid, ToolError> {
    let genesis = ensure_genesis(store)?;
    let workspaces = workspaces
        .into_iter()
        .map(|(name, commit)| (name, (commit, None)))
        .collect::<BTreeMap<_, _>>();
    let transition = Transition::ConversationRoot {
        identity: Identity {
            id,
            kind: IdentityKind::Root,
            owner: None,
        },
        title,
        workspaces,
        files_seed: None,
    };
    let applied = apply(store, None, &transition)?;
    mint(
        store,
        &genesis,
        &applied.tree,
        transition.kind(),
        &signature(),
    )
    .map_err(ToolError::new)
}

fn append_user_message(
    store: &mut GitStore,
    head: &Oid,
    actor: String,
    text: String,
) -> Result<(Oid, String), ToolError> {
    let message_id = client_key()?;
    let conversation = Conversation::open(store, head)?;
    let id = conversation.identity()?.id;
    drop(conversation);
    let transition = Transition::MessageAppend {
        entry: TranscriptEntry {
            message_id: message_id.clone(),
            conversation: id,
            role: Role::User,
            actor,
            request: None,
            round: None,
            model: None,
            blocks: vec![Block::Text { text }],
            proposal: None,
            workspace_resolution: None,
        },
        payloads: Vec::new(),
    };
    Ok((append(store, head, transition)?, message_id))
}

fn admit_request(
    store: &mut GitStore,
    head: &Oid,
    request: Oid,
    model: String,
    configuration: String,
) -> Result<Oid, ToolError> {
    let view = Conversation::open(store, head)?;
    let request_workspaces = view.workspaces_tree()?;
    drop(view);
    let record = RequestRecord {
        id: request.clone(),
        request_head: head.clone(),
        request_workspaces,
        model,
        configuration,
        round: 0,
        calls: Vec::new(),
        interjections: Vec::new(),
        status: RequestStatus::Queued,
        latest_message: None,
        escape_reason: None,
        outcome: None,
    };
    append(store, head, Transition::RequestAdmit { record })
}

fn prepare_turn_request(
    repo: &Path,
    store: &mut GitStore,
    configuration: &Oid,
    human: &Oid,
    secret_hash: &Oid,
) -> Result<Oid, ToolError> {
    let configuration_entries = store.read_tree(configuration).map_err(String::from)?;
    if !configuration_entries
        .iter()
        .any(|entry| entry.name == ".caos-curry")
    {
        return Err(ToolError::new(format!(
            "model configuration {configuration} is not a curry node"
        )));
    }
    let base = required_entry(&configuration_entries, "base", configuration)?;
    if base.mode != Mode::Blob {
        return Err(ToolError::new(format!(
            "model configuration {configuration} has a non-blob base"
        )));
    }
    let configured_image = String::from_utf8(store.read_blob(&base.oid).map_err(String::from)?)
        .map_err(|_| {
            ToolError::new(format!(
                "model configuration {configuration} base is not UTF-8"
            ))
        })?;
    let args = required_entry(&configuration_entries, "args", configuration)?;
    if args.mode != Mode::Tree {
        return Err(ToolError::new(format!(
            "model configuration {configuration} has non-tree args"
        )));
    }

    let request_base = match Oid::parse(&configured_image, "model configuration image") {
        Ok(image) => {
            store.read_tree(&image).map_err(String::from)?;
            RequestTreeEntry {
                mode: "040000",
                kind: "tree",
                oid: image,
            }
        }
        Err(_) if configured_image.starts_with("docker://") => RequestTreeEntry {
            mode: "100644",
            kind: "blob",
            oid: base.oid.clone(),
        },
        Err(error) => return Err(ToolError::new(error)),
    };
    store.read_commit(human).map_err(String::from)?;
    store.read_blob(secret_hash).map_err(String::from)?;
    let mut entries = store
        .read_tree(&args.oid)
        .map_err(String::from)?
        .into_iter()
        .map(|entry| {
            let (mode, kind) = match entry.mode {
                Mode::Blob => ("100644", "blob"),
                Mode::Executable => ("100755", "blob"),
                Mode::Tree => ("040000", "tree"),
            };
            (
                entry.name,
                RequestTreeEntry {
                    mode,
                    kind,
                    oid: entry.oid,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    entries.insert(
        "head".to_string(),
        RequestTreeEntry {
            mode: "160000",
            kind: "commit",
            oid: human.clone(),
        },
    );
    entries.insert(
        "secret-hash".to_string(),
        RequestTreeEntry {
            mode: "100644",
            kind: "blob",
            oid: secret_hash.clone(),
        },
    );
    if let Ok(salt) = std::env::var("CAOS_SALT") {
        if !salt.is_empty() {
            entries.insert(
                "salt".to_string(),
                RequestTreeEntry {
                    mode: "100644",
                    kind: "blob",
                    oid: store.write_blob(salt.as_bytes()).map_err(String::from)?,
                },
            );
        }
    }
    entries.insert("base".to_string(), request_base);
    write_request_tree(repo, entries)
}

fn required_entry<'a>(
    entries: &'a [TreeEntry],
    name: &str,
    tree: &Oid,
) -> Result<&'a TreeEntry, ToolError> {
    entries
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| ToolError::new(format!("tree {tree} has no {name:?} entry")))
}

fn write_request_tree(
    repo: &Path,
    entries: BTreeMap<String, RequestTreeEntry>,
) -> Result<Oid, ToolError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["mktree", "-z"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| ToolError::new(format!("running git mktree: {error}")))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| ToolError::new("git mktree has no standard input"))?;
        for (name, entry) in entries {
            write!(
                stdin,
                "{} {} {}\t{name}\0",
                entry.mode, entry.kind, entry.oid
            )
            .map_err(|error| ToolError::new(format!("writing git mktree input: {error}")))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| ToolError::new(format!("waiting for git mktree: {error}")))?;
    if !output.status.success() {
        return Err(command_error("git mktree", &output));
    }
    let oid = String::from_utf8(output.stdout)
        .map_err(|_| ToolError::new("git mktree returned non-UTF-8 output"))?;
    parse_oid(oid.trim(), "turn request")
}

fn fetch_objects(repo: &Path, oids: &[Oid]) -> Result<(), ToolError> {
    let unique = oids
        .iter()
        .map(Oid::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "caos",
        ])
        .args(unique)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = command
        .output()
        .map_err(|error| ToolError::new(format!("running git fetch: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("git fetch", &output))
    }
}

fn push_plain(repo: &Path, updates: &[(Oid, String)]) -> Result<(), ToolError> {
    let refspecs = updates
        .iter()
        .map(|(oid, refname)| format!("{oid}:{refname}"))
        .collect::<Vec<_>>();
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["push", "--quiet", "caos"])
        .args(&refspecs)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| ToolError::new(format!("running git push: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("git push", &output))
    }
}

fn command_error(action: &str, output: &std::process::Output) -> ToolError {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        ToolError::new(format!("{action} failed with {}", output.status))
    } else {
        ToolError::new(format!("{action}: {detail}"))
    }
}

fn push_with_lease(
    store: &GitStore,
    refname: String,
    expected: &Oid,
    new: &Oid,
) -> Result<(), ToolError> {
    let updates = vec![RefUpdate {
        refname: refname.clone(),
        expected: Some(expected.clone()),
        new: Some(new.clone()),
    }];
    push_updates(store, updates, &refname, Some(expected))
}

fn push_updates(
    store: &GitStore,
    updates: Vec<RefUpdate>,
    refname: &str,
    expected: Option<&Oid>,
) -> Result<(), ToolError> {
    if let Err(error) = store.push(&updates) {
        let observed = store.read_ref(refname)?;
        if observed.as_ref() != expected {
            println!(
                "moved {}",
                observed.as_ref().map(Oid::as_str).unwrap_or("absent")
            );
            return Err(ToolError::code(3));
        }
        return Err(ToolError::new(error));
    }
    Ok(())
}

fn open(repo: &Path) -> Result<GitStore, ToolError> {
    GitStore::open(repo, Some("caos")).map_err(ToolError::new)
}

fn signature() -> conversation_protocol::v3::Signature {
    client_signature("caos", "caos@caos", 1_700_000_000)
}

fn append(store: &mut GitStore, head: &Oid, transition: Transition) -> Result<Oid, ToolError> {
    let parent_tree = store.read_commit(head).map_err(String::from)?.tree;
    let applied = apply(store, Some(&parent_tree), &transition)?;
    mint(store, head, &applied.tree, transition.kind(), &signature()).map_err(ToolError::new)
}

fn client_key() -> Result<String, ToolError> {
    let mut bytes = [0_u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| ToolError::new(format!("generating message id: {error}")))?;
    Ok(hex_lower(&bytes))
}

fn write_record(bytes: &[u8]) -> Result<(), ToolError> {
    std::io::stdout()
        .write_all(bytes)
        .map_err(|error| ToolError::new(format!("writing record to standard output: {error}")))
}

fn text_blocks(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            Block::Payload { .. } | Block::ToolUse { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn request_status_name(status: &RequestStatus) -> &'static str {
    match status {
        RequestStatus::Queued => "queued",
        RequestStatus::Running => "running",
        RequestStatus::Cancelling => "cancelling",
        RequestStatus::Idle => "idle",
        RequestStatus::Failed => "failed",
    }
}

fn wait_terminal(
    repo: &Path,
    refname: &str,
    request: &Oid,
    timeout_secs: u64,
) -> Result<(), ToolError> {
    let store = open(repo)?;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut seen = None;
    while Instant::now() < deadline {
        let observed = store.read_ref(refname)?;
        if observed != seen {
            seen = observed.clone();
            if observed.is_some() {
                let Some(head) = store.fetch_ref(refname)? else {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                };
                seen = Some(head.clone());
                let view = Conversation::open(&store, &head)?;
                if let Some(record) = view.request(request)? {
                    match record.status {
                        RequestStatus::Idle => {
                            println!("head {head}");
                            println!("status idle");
                            return Ok(());
                        }
                        RequestStatus::Failed => {
                            let error = match record.outcome {
                                Some(RequestOutcome::Failed { error }) => {
                                    let (ordinal, message_id) =
                                        paths::parse_transcript_entry_path(&error)?;
                                    let (found, entry) =
                                        view.transcript_entry(ordinal)?.ok_or_else(|| {
                                            ToolError::new(format!(
                                                "request error entry {error} is absent"
                                            ))
                                        })?;
                                    if found != message_id {
                                        return Err(ToolError::new(format!(
                                            "request error entry {error} has a mismatched id"
                                        )));
                                    }
                                    text_blocks(&entry.blocks)
                                }
                                _ => "request failed without an error entry".to_string(),
                            };
                            println!("head {head}");
                            println!("status failed");
                            println!("error {error}");
                            return Err(ToolError::code(1));
                        }
                        RequestStatus::Queued
                        | RequestStatus::Running
                        | RequestStatus::Cancelling => {}
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(ToolError::code(2))
}

fn parents(
    store: &GitStore,
    mut current: Oid,
    started_tool: Option<&(Oid, String)>,
    present_path: Option<&str>,
) -> Result<(), ToolError> {
    let mut found_started_tool = started_tool.is_none();
    loop {
        if current == g3() {
            return Err(ToolError::new(
                "conversation spine reached G3 without a root",
            ));
        }
        let info = store.read_commit(&current).map_err(String::from)?;
        if info.parents.len() != 1 {
            return Err(ToolError::new(format!(
                "conversation commit {current} has {} parents",
                info.parents.len()
            )));
        }
        let kind = Kind::parse_message(&info.message)?;
        if let Some(path) = present_path {
            let present = Conversation::open(store, &current)?
                .snapshot()
                .read(path)?
                .is_some();
            println!("{current} {} {present}", kind.as_str());
        } else {
            println!("{current} {}", kind.as_str());
        }
        if !found_started_tool && kind == Kind::ToolStart {
            let (request, id) = started_tool.expect("checked as present");
            let view = Conversation::open(store, &current)?;
            let request_record = view
                .request(request)?
                .ok_or_else(|| ToolError::new(format!("request {request} does not exist")))?;
            for round in 0..request_record.round {
                if view
                    .tools(request, round)?
                    .iter()
                    .any(|tool| tool.id == *id && tool.status == ToolStatus::Started)
                {
                    found_started_tool = true;
                    break;
                }
            }
        }
        let parent = info.parents[0].clone();
        if kind == Kind::ConversationRoot {
            if parent != g3() {
                return Err(ToolError::new(format!(
                    "conversation root {current} does not parent G3"
                )));
            }
            return if found_started_tool {
                Ok(())
            } else {
                let (_, id) = started_tool.expect("checked as present");
                Err(ToolError::new(format!(
                    "tool {id:?} has no started record on the conversation spine"
                )))
            };
        }
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repeated_workspaces_and_defaults() {
        let command = parse_args(vec![
            "root".to_string(),
            "--repo".to_string(),
            "/repo".to_string(),
            "--id".to_string(),
            "conversation".to_string(),
            "--title".to_string(),
            "Title".to_string(),
            "--workspace".to_string(),
            format!("main={}", "a".repeat(40)),
            "--workspace".to_string(),
            format!("docs={}", "b".repeat(40)),
        ])
        .unwrap();
        let ToolCommand::Root { workspaces, .. } = command else {
            panic!("wrong command")
        };
        assert_eq!(workspaces.len(), 2);

        let command = parse_args(vec![
            "wait-terminal".to_string(),
            "--repo".to_string(),
            "/repo".to_string(),
            "--ref".to_string(),
            "refs/test".to_string(),
            "--request".to_string(),
            "c".repeat(40),
        ])
        .unwrap();
        assert!(matches!(
            command,
            ToolCommand::WaitTerminal {
                timeout_secs: 120,
                ..
            }
        ));

        let command = parse_args(vec![
            "turn".to_string(),
            "--repo".to_string(),
            "/repo".to_string(),
            "--id".to_string(),
            "conversation".to_string(),
            "--user".to_string(),
            "tester".to_string(),
            "--title".to_string(),
            "Title".to_string(),
            "--actor".to_string(),
            "tester".to_string(),
            "--text".to_string(),
            "hello".to_string(),
            "--secret-hash".to_string(),
            "e".repeat(40),
            "--model".to_string(),
            "test-model".to_string(),
            "--configuration".to_string(),
            "f".repeat(40),
            "--workspace".to_string(),
            format!("main={}", "a".repeat(40)),
        ])
        .unwrap();
        assert!(matches!(
            command,
            ToolCommand::Turn {
                head: None,
                workspaces,
                ..
            } if workspaces.len() == 1
        ));

        let command = parse_args(vec![
            "parents".to_string(),
            "--repo".to_string(),
            "/repo".to_string(),
            "--head".to_string(),
            "a".repeat(40),
            "--validate".to_string(),
        ])
        .unwrap();
        assert!(matches!(
            command,
            ToolCommand::Parents { validate: true, .. }
        ));

        let command = parse_args(vec![
            "parents".to_string(),
            "--repo".to_string(),
            "/repo".to_string(),
            "--head".to_string(),
            "a".repeat(40),
            "--request".to_string(),
            "b".repeat(40),
            "--started-tool".to_string(),
            "toolu_01".to_string(),
            "--present-path".to_string(),
            ".caos/tools/request/0000/toolu_01.json".to_string(),
        ])
        .unwrap();
        assert!(matches!(
            command,
            ToolCommand::Parents {
                started_tool: Some((_, ref id)),
                present_path: Some(ref path),
                ..
            } if id == "toolu_01" && path == ".caos/tools/request/0000/toolu_01.json"
        ));
    }
}
