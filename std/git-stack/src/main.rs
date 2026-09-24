//! Promote work snapshots into named stack layers.
mod add_layer;
mod objects;
mod stack;

use conversation_protocol::v3::{Mode, ObjectStore, Oid, TreeBuilder};
use worker_common::{arg, caos, cas_hash, read_arg, run_worker};

fn run() -> Result<(), String> {
    let root = Oid::parse(&cas_hash(&arg("in"))?, "input conversation tree")?;
    let mut store = objects::RemoteStore::from_env()?;
    let operation = read_arg("operation")?;
    let message = if operation == "add-layer" {
        caos(["get", &arg("message")])?;
        std::fs::read(arg("message")).map_err(|e| format!("reading message: {e}"))?
    } else {
        Vec::new()
    };
    let proposal = match operation.as_str() {
        "add-layer" => add_layer::add_layer(
            &mut store,
            &root,
            &add_layer::Parameters {
                stack: read_arg("stack")?,
                name: read_arg("name")?,
                author: objects::parse_signature(&read_arg("author")?)?,
                committer: objects::parse_signature(&read_arg("committer")?)?,
                message,
            },
        )?,
        operation => return Err(format!("unknown stack operation {operation}")),
    };
    let report = store
        .write_blob(
            serde_json::to_string_pretty(&proposal.report)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        )
        .map_err(String::from)?;
    let mut output = TreeBuilder::from(None);
    output.put_oid("proposal", Mode::Tree, proposal.tree);
    output.put_oid("report", Mode::Blob, report);
    let output = output.build(&mut store)?;
    caos(["get-hash", output.as_str(), "/cas/out"])
}

fn main() -> std::process::ExitCode {
    run_worker("git-stack", run)
}
