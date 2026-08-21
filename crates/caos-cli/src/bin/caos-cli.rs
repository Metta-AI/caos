//! caos-cli: the user-facing caos client.
//!
//! This is what a person runs from inside their working tree. It uses the server
//! as a `caos` git remote ([`caos::GitTransport`]): objects are built in the
//! local working repo and exchanged with the server by negotiated push/fetch, so
//! a large unchanged tree is almost free to "upload" and an edit ships only its
//! delta. Compute is triggered over HTTP against the same server — its URL is
//! always the `caos` remote's URL, never an env var.
//!
//! There is no `/cas` here — that's the worker's world. The commands: `run`
//! (compute, with the result checked out to any host path, or a file result
//! streamed to stdout when no path is given), `curry` (bind args to an image,
//! printing the curried ref), `import-image` (get a docker image into caos,
//! printing its hash), and `talk`/`tui`/`chat` (agent conversations — see
//! design/agent-harness.md; `talk` is the everyday surface, `chat` the
//! explicit one-turn form). The object-level commands (`get`/`put`/…) live
//! only in the worker `caos`, which runs inside a sandbox with a real `/cas`.

use std::io::Write;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use caos::{prog_name, GitTransport};

mod tui;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}: {err}", prog_name(&args));
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        // `run [--trace[=<file|->]] [--trace-id=<id>] [output] --base:<type>=<image> [...]`.
        // The trace id is invocation metadata, and `[output]` is a host path to
        // check the result out to — the run's only positional, and the only
        // token that isn't a `--flag`. Every `--name[:type]=value` after it is a
        // computation argument and therefore part of the ArgTree (the cache
        // key), including the reserved `--base`, which names the worker to run.
        Some("run") => {
            let mut trace_id = None;
            let mut trace_path = None;
            let mut index = 2;
            while let Some(arg) = args.get(index) {
                if let Some(id) = arg.strip_prefix("--trace-id=") {
                    if trace_id.replace(id).is_some() {
                        return Err("--trace-id given twice".to_string());
                    }
                } else if arg == "--trace" {
                    if trace_path.replace("-").is_some() {
                        return Err("--trace given twice".to_string());
                    }
                } else if let Some(path) = arg.strip_prefix("--trace=") {
                    if trace_path.replace(path).is_some() {
                        return Err("--trace given twice".to_string());
                    }
                } else {
                    break;
                }
                index += 1;
            }
            let (output, kvs) = match &args[index..] {
                [] => return Err(usage(args)),
                [output, kvs @ ..] if !output.starts_with("--") => (Some(output.as_str()), kvs),
                kvs => (None, kvs),
            };
            if trace_path == Some("") {
                return Err("--trace needs a file path or '-' for stdout".to_string());
            }
            if trace_id.is_some() && trace_path.is_none() {
                return Err("--trace-id is only an override for --trace".to_string());
            }
            if trace_path == Some("-") && output.is_none() {
                return Err("stdout tracing requires a computation output path".to_string());
            }
            if trace_path.is_some_and(|path| output == Some(path)) {
                return Err("trace and computation output paths must differ".to_string());
            }
            let generated_id = (trace_path.is_some() && trace_id.is_none()).then(fresh_trace_id);
            let trace_id = trace_id.or(generated_id.as_deref());
            let mut trace_output: Option<Box<dyn Write + Send>> = match trace_path {
                Some("-") => Some(Box::new(std::io::stdout())),
                Some(path) => Some(Box::new(
                    std::fs::File::create(path)
                        .map_err(|e| format!("creating trace file {path}: {e}"))?,
                )),
                None => None,
            };
            let transport = transport()?;
            let run = |trace| caos::cli_run(&transport, output, trace, kvs);
            match trace_output.as_mut() {
                Some(writer) => run(Some((
                    trace_id.expect("trace output always has an id"),
                    writer.as_mut(),
                ))),
                None => run(None),
            }
        }
        // `curry [--unbind=<name> ...] --base:<type>=<arg tree> [--name=value | --name:@=path ...]` —
        // bind args to the `--base` ArgTree (a bare image, a curry node, or a flat
        // args tree), printing a ref to the curried ArgTree (run it like any
        // other). Path args are host paths to ingest;
        // `--unbind` releases a bound arg so it can be rebound.
        Some("curry") => caos::cli_curry(&transport()?, &args[2..]),
        // `prepare-request --base:<type>=<image> [...]` — build and push the
        // exact flat runnable request without executing it.
        Some("prepare-request") => caos::cli_prepare_request(&transport()?, &args[2..]),
        // `import-image [--base docker://<ref>] <docker-archive>` — store a
        // docker-archive image into caos and print the git hash of the resulting
        // git-docker image. With `--base`, the archive's layers are stored as a
        // delta to stack on that stock base (which stays out of git).
        Some("import-image") => match &args[2..] {
            [archive] => caos::import_image(&transport()?, archive, None),
            [flag, base, archive] if flag == "--base" => {
                caos::import_image(&transport()?, archive, Some(base))
            }
            _ => Err(usage(args)),
        },
        // `talk [<prompt>] [flags]` — agent conversation, everyday surface:
        // continues the repo's most recent conversation (`-c` picks one,
        // `--new` starts another); with no prompt on a terminal it loops, one
        // turn per line. Flag parsing and usage live in the conversation client.
        Some("talk") => caos_cli::cli_talk(&transport()?, &args[2..]),
        Some("tui") => tui::run(&args[2..]).map_err(|error| format!("tui: {error}")),
        // `chat <name> [-m <message>] [flags]` — one explicit turn of a named
        // conversation on its shared canonical head. Flag parsing (and the
        // chat-specific usage) lives in the conversation client.
        Some("chat") => caos_cli::cli_chat(&transport()?, &args[2..]),
        // `run-tool <script | name> [--name=value ...]` — run a caos-tool (a
        // directory with a `.caos-expr`, `caos-tools/<name>` for a bare name) as a caos
        // job over this repo's tree: what an agent's tool invocation does,
        // callable by hand. See `caos::cli_run_tool` for the result conventions.
        Some("run-tool") => caos::cli_run_tool(&transport()?, &args[2..]),
        // `eval-path [--tree=<oid>] <path>` — evaluate the `.caos-expr` files
        // from the tree root down to <path> and print the result's
        // "<kind> <hash>". With no --tree, the tracked workspace tree is the
        // start. See design/caos-expr.md.
        Some("eval-path") => {
            let (tree, path) = match &args[2..] {
                [path] => (None, path.as_str()),
                [flag, path] if flag.starts_with("--tree=") => {
                    (Some(&flag["--tree=".len()..]), path.as_str())
                }
                _ => return Err(usage(args)),
            };
            caos::cli_eval_path(&transport()?, tree, path)
        }
        // `get <hash> <path>` — check a result out on the host, the escape
        // hatch from laziness. `run-tool` prints a hash and materializes
        // nothing, which is right for the common case and useless when you
        // want to read a test's full record (its output, the inner stack's
        // logs). Objects it already has cost nothing, so checking out a
        // 218 MB image you have most of is a local write, not a download.
        Some("get") => match &args[2..] {
            [hash, path] => caos::cli_get(&transport()?, hash, path),
            _ => Err(usage(args)),
        },
        // `status <arg tree hash>` — what is running under an ArgTree, as JSON
        // (SPEC.md "Tracing"). The same view `run`/`run-tool` show live, for a
        // run happening elsewhere or one you want to look at after the fact.
        Some("status") => match &args[2..] {
            [arg_tree] => caos::cli_status(&transport()?, arg_tree),
            _ => Err(usage(args)),
        },
        // `secrets [--check]` — tend the local `.caos-secrets` store: fill a
        // missing `entropy=` with fresh entropy and warn on a weak one, so
        // cache isolation is safe by default (design/secrets.md). `--check`
        // only reports (writes nothing) and exits non-zero on any issue — a CI
        // gate. Offline: no server, no transport.
        Some("secrets") => match &args[2..] {
            [] => caos::cli_secrets(false),
            [flag] if flag == "--check" => caos::cli_secrets(true),
            _ => Err(usage(args)),
        },
        _ => Err(usage(args)),
    }
}

/// The CLI talks to the server as the `caos` git remote, over the local repo.
fn transport() -> Result<GitTransport, String> {
    GitTransport::from_cwd()
}

fn fresh_trace_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("cli-{}-{now}", std::process::id())
}

/// The caos revision this build came from, injected by the flake's wrapper
/// (`CAOS_REV`) rather than compiled in — a compile-time rev would re-key the
/// Rust workspace on every commit. `unknown` when run straight out of `cargo
/// build`, or from any build that did not set it, which is the honest answer:
/// the point of printing it is to tell a STALE command apart from a current
/// one, and a binary that cannot say is not evidence that it is current.
fn build_rev() -> String {
    std::env::var("CAOS_REV").unwrap_or_else(|_| "unknown".to_string())
}

fn usage(args: &[String]) -> String {
    let prog = prog_name(args);
    let rev = build_rev();
    format!(
        "{prog} ({rev})\n\
         usage:\n  \
         {prog} run [--trace[=<file|->]] [--trace-id=<id>] [output] --base:<type>=<image> [--name=value | --name:@=path ...]\n  \
         {prog} curry [--unbind=<name> ...] --base:<type>=<arg tree> [--name=value | --name:@=path ...]\n    \
         (an image is --base:@=<dir>, --base:docker=<ref> or --base:hash=<oid>)\n  \
         {prog} prepare-request --base:<type>=<image-or-arg tree> [--name=value | --name:@=path ...]\n  \
         {prog} import-image [--base docker://<ref>] <docker-archive>\n  \
         {prog} talk [<prompt>] [-c <name>] [--new] [--log] [--username <name>] [options]\n  \
         {prog} tui [-c <name>] [--new] [--username <name>] [options]\n  \
         {prog} chat <name> [-m <message>] [--base <revspec>] [--log] [--username <name>] [options]\n  \
         {prog} run-tool <script | name> [--name=value ...]\n  \
         {prog} eval-path [--tree=<oid>] <path>\n  \
         {prog} get <hash> <path>\n  \
         {prog} status <arg tree hash>\n  \
         {prog} secrets [--check]"
    )
}
