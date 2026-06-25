//! caos-cli: the user-facing caos client.
//!
//! This is what a person runs from inside their working tree. It uses the server
//! as a `caos` git remote ([`caos::GitTransport`]): objects are built in the
//! local working repo and exchanged with the server by negotiated push/fetch, so
//! a large unchanged tree is almost free to "upload" and an edit ships only its
//! delta. Compute is still triggered over HTTP (`$CAOS_SERVER_URL`, `/run`).
//!
//! Subcommands: `get-hash`, `get`, `put`, `import-image`, `resolve`, `run`,
//! `curry`. (There is no `entrypoint` — that's the worker's job.)

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
        Some("get-hash") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(hash), Some(path), None) => caos::get_hash(&transport()?, hash, path),
            _ => Err(usage(args)),
        },
        Some("get") => {
            let (path, depth) = caos::parse_get(&args[2..])?;
            caos::get(&transport()?, path, depth)
        }
        Some("put") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(src), Some(dst), None) => caos::put(&transport()?, src, dst),
            _ => Err(usage(args)),
        },
        Some("import-image") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(archive), Some(dst), None) => caos::import_image(&transport()?, archive, dst),
            _ => Err(usage(args)),
        },
        // `resolve <ref>` — print the tree hash a local git ref points at (e.g.
        // refs/caos/std). No transport needed: the refs are already local.
        Some("resolve") => match (args.get(2), args.get(3)) {
            (Some(name), None) => {
                println!("{}", caos::resolve_ref(name)?);
                Ok(())
            }
            _ => Err(usage(args)),
        },
        // `run <image> <output> -- [--name=value | --name:@=path ...]`. The `--` separates the
        // fixed arguments from the (possibly empty) list of key/value args. `<image>`
        // may be `/cas/std/<name>` to run a builtin from the published library —
        // the same path workers use — resolved to its hash here.
        Some("run") => match &args[2..] {
            [image, output, sep, kvs @ ..] if sep == "--" => {
                let t = transport()?;
                let image = caos::resolve_cli_image(&t, image)?;
                caos::caos_run(&t, &image, output, kvs)
            }
            _ => Err(usage(args)),
        },
        // `curry <image> -- [--name=value | --name:@=path ...]` — bind args to an image, printing
        // a ref to the resulting curried image (run/curry it like any image).
        Some("curry") => match &args[2..] {
            [image, sep, kvs @ ..] if sep == "--" => caos::caos_curry(&transport()?, image, kvs),
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
        "usage:\n  {prog} get-hash <hash> <path>\n  \
         {prog} get [-r | --recursive[=<depth>]] <path>\n  \
         {prog} put <src-path> <cas-path>\n  \
         {prog} import-image <docker-archive> <cas-path>\n  \
         {prog} resolve <ref>\n  \
         {prog} run <image | /cas/std/<name>> <output-cas-path> -- [--name=value | --name:@=path ...]\n  \
         {prog} curry <image> -- [--name=value | --name:@=path ...]"
    )
}
