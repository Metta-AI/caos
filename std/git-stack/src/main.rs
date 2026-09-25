//! Replay a plan over a stack directory without checking out its source trees.
use git_stack as objects;
mod rebase;
mod rebase_plan;

use conversation_protocol::v3::{Mode, ObjectStore, Oid, TreeBuilder};
use worker_common::{arg, caos, cas_hash, run_worker};

fn run() -> Result<(), String> {
    let input = Oid::parse(&cas_hash(&arg("in"))?, "input stack tree")?;
    let mut store = objects::RemoteStore::from_env()?;
    let (proposal, failed) = match rebase::propose(&mut store, &input) {
        Ok(proposal) => (proposal, false),
        Err(error) => (
            rebase::Proposal {
                tree: input,
                report: format!("Replay was not applied: {error}\n"),
            },
            true,
        ),
    };
    let out = store
        .write_blob(proposal.report.as_bytes())
        .map_err(String::from)?;
    let mut result = TreeBuilder::from(None);
    result.put_oid("prop", Mode::Tree, proposal.tree);
    result.put_oid("out", Mode::Blob, out);
    if failed {
        result.put("failed", Mode::Blob, Vec::new());
    }
    let result = result.build(&mut store)?;
    caos(["get-hash", result.as_str(), "/cas/out"])
}

fn main() -> std::process::ExitCode {
    run_worker("git-stack", run)
}
