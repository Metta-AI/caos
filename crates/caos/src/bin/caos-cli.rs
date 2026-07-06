//! caos-cli: the user-facing caos client.
//!
//! This is what a person runs from inside their working tree. It uses the server
//! as a `caos` git remote ([`caos::GitTransport`]): objects are built in the
//! local working repo and exchanged with the server by negotiated push/fetch, so
//! a large unchanged tree is almost free to "upload" and an edit ships only its
//! delta. Compute is triggered over HTTP (`$CAOS_SERVER_URL`, `/run`).
//!
//! There is no `/cas` here — that's the worker's world. The CLI has three
//! commands: `run` (compute, with the result checked out to any host path, or
//! a file result streamed to stdout when no path is given), `curry` (bind args
//! to an image, printing the curried ref), and `import-image` (get a docker
//! image into caos, printing its hash). The object-level commands
//! (`get`/`put`/…) live only in the worker `caos`, which runs inside a sandbox
//! with a real `/cas`.

use std::process::ExitCode;

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
        // `run <image> [output] -- [--name=value | --name:@=path ...]`. The `--`
        // separates the fixed arguments from the (possibly empty) list of
        // key/value args. `<output>`, if given, is any path on the host; the
        // result is checked out there in full. If it's omitted and the result is
        // a file, the file's bytes are written to stdout. `<image>` may be
        // `/cas/std/<name>` to run a builtin from the published library, a
        // `docker://<ref>`, or a git hash.
        Some("run") => match &args[2..] {
            [image, sep, kvs @ ..] if sep == "--" => caos::cli_run(&transport()?, image, None, kvs),
            [image, output, sep, kvs @ ..] if sep == "--" => {
                caos::cli_run(&transport()?, image, Some(output), kvs)
            }
            _ => Err(usage(args)),
        },
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
        _ => Err(usage(args)),
    }
}

/// The CLI talks to the server as the `caos` git remote, over the local repo.
fn transport() -> Result<GitTransport, String> {
    GitTransport::from_cwd()
}

fn usage(args: &[String]) -> String {
    let prog = prog_name(args);
    format!(
        "usage:\n  \
         {prog} run <image | /cas/std/<name>> [output] -- [--name=value | --name:@=path ...]\n  \
         {prog} curry <image | /cas/std/<name>> -- [--name=value | --name:@=path ...]\n  \
         {prog} import-image [--base docker://<ref>] <docker-archive>"
    )
}
