//! Test worker for first-class commits — the llm-step shape in miniature,
//! built from source by the rustc builder in this test. Called with a
//! conversation head commit (`--head`, a `:commit=` arg), it launches one tool
//! call as a run-then sub-run, currying itself (plus the head) into `then`;
//! called back with `--result`, it reads the head commit (message aside: tree,
//! parent — walking one generation by hash), mints a child commit whose
//! message carries the tool's output, and returns `commit <hash>`.
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, cas_hash, path, read_commit, run_then, run_worker, scratch, std_image,
    write_commit, Arg,
};

fn main() -> ExitCode {
    run_worker("commit-worker", run)
}

/// Two positions, told apart by the args present: no `--result` is the first
/// call (launch the tool), `--result` is the callback (mint the child commit).
fn run() -> Result<(), String> {
    if Path::new(&arg("result")).exists() {
        finish()
    } else {
        start()
    }
}

/// First position: stage the tool's input, curry the tool (the bash builtin
/// bound to `--tool-script`) and ourselves, and tail-call run-then.
fn start() -> Result<(), String> {
    let input = scratch("toolin")?.join("input");
    fs::write(&input, "21\n").map_err(|e| format!("writing tool input: {e}"))?;
    caos(["put", path(&input), "/cas/toolin"])?;

    let tool = caos_curry(
        &std_image("bash"),
        &[("script", Arg::Path(&arg("tool-script")))],
    )?;
    // A source-built worker is curry(runner, bin), unwrapped into args by the
    // caller — so "ourselves" is that same curry rebuilt from our own args
    // (content-addressed, hence identical), plus the head commit to remember.
    // The head is a commit-valued CAS path, so it rides the curry as a gitlink.
    let me = caos_curry(
        &arg("image"),
        &[
            ("bin", Arg::Path(&arg("bin"))),
            ("head", Arg::Path(&arg("head"))),
        ],
    )?;
    run_then("/cas/toolin", &tool, Some(&me))
}

/// Then position: `--result` is the tool's output; `--head` rode through the
/// curry. Read the head commit, walk what it references by hash, and mint the
/// child turn.
fn finish() -> Result<(), String> {
    caos(["get", &arg("result")])?;
    let tool_out = fs::read_to_string(arg("result"))
        .map_err(|e| format!("reading result: {e}"))?
        .trim()
        .to_string();

    let head_hash = cas_hash(&arg("head"))?;
    let head = read_commit(&arg("head"))?;

    // Walk by hash through the ordinary transport: the head's tree fetches as
    // a real directory, and its first parent as a commit (one generation).
    caos(["get-hash", &head.tree, "/cas/head-tree"])?;
    if !Path::new("/cas/head-tree").is_dir() {
        return Err("head commit's tree did not materialize as a directory".to_string());
    }
    let parent = head.parents.first().ok_or("head commit has no parent")?;
    caos(["get-hash", parent, "/cas/head-parent"])?;
    read_commit("/cas/head-parent")?;

    // The child turn: same workspace tree, the head as parent, the tool's
    // output as the turn text. Minting it at /cas/out makes `commit <hash>`
    // this run's result.
    let message = format!("turn: tool said {tool_out}");
    write_commit(&head.tree, &[&head_hash], &message, "/cas/out")?;
    Ok(())
}
