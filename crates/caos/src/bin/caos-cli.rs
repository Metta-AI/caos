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
//! printing its hash), and `talk`/`chat` (agent conversations — see
//! design/agent-harness.md; `talk` is the everyday surface, `chat` the
//! explicit one-turn form). The object-level commands (`get`/`put`/…) live
//! only in the worker `caos`, which runs inside a sandbox with a real `/cas`.

use std::io::Write;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use caos::{prog_name, GitTransport};

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
        // `run [--trace[=<file|->]] [--trace-id=<id>] <image> [output] -- [...]`.
        // The trace id is invocation metadata; everything after `--` is a
        // computation argument and therefore part of the request hash.
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
            let (image, output, kvs) = match &args[index..] {
                [image, sep, kvs @ ..] if sep == "--" => (image, None, kvs),
                [image, output, sep, kvs @ ..] if sep == "--" => {
                    (image, Some(output.as_str()), kvs)
                }
                _ => return Err(usage(args)),
            };
            if trace_path == Some("") {
                return Err("--trace needs a file path or '-' for stdout".to_string());
            }
            if trace_id.is_some() && trace_path.is_none() {
                return Err("--trace-id is only an override for --trace".to_string());
            }
            if trace_path == Some("-") && output.is_none() {
                return Err(
                    "--trace=- requires an <output> path for the computation result".to_string(),
                );
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
            match trace_output.as_mut() {
                Some(writer) => caos::cli_run(
                    &transport,
                    image,
                    output,
                    trace_id,
                    Some(writer.as_mut()),
                    kvs,
                ),
                None => caos::cli_run(&transport, image, output, trace_id, None, kvs),
            }
        }
        // `curry <image> -- [--name=value | --name:@=path ...]` — bind args to an
        // image, printing a ref to the curried image (run it like any image).
        // Path args are host paths to ingest, or `/cas/std/<name>` builtin refs.
        Some("curry") => match &args[2..] {
            [image, sep, kvs @ ..] if sep == "--" => caos::cli_curry(&transport()?, image, kvs),
            _ => Err(usage(args)),
        },
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
        // turn per line. Flag parsing (and usage) lives in `caos::cli_talk`.
        Some("talk") => caos::cli_talk(&transport()?, &args[2..]),
        // `chat <name> [-m <message>] [flags]` — one explicit turn of a named
        // conversation: mint the human commit, run llm-step over it, print
        // progress, advance `refs/caos/conversations/<name>` on success. Flag
        // parsing (and the chat-specific usage) lives in `caos::cli_chat`.
        Some("chat") => caos::cli_chat(&transport()?, &args[2..]),
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

fn usage(args: &[String]) -> String {
    let prog = prog_name(args);
    format!(
        "usage:\n  \
         {prog} run [--trace[=<file | ->]] [--trace-id=<id>] <image | /cas/std/<name>> [output] -- [--name=value | --name:@=path ...]\n  \
         {prog} curry <image | /cas/std/<name>> -- [--name=value | --name:@=path ...]\n  \
         {prog} import-image [--base docker://<ref>] <docker-archive>\n  \
         {prog} talk [<prompt>] [-c <name>] [--new] [--log] [options]\n  \
         {prog} chat <name> [-m <message>] [--base <revspec>] [--log] [options]"
    )
}
