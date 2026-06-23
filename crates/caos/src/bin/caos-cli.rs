//! caos-cli: the user-facing caos client.
//!
//! This is what a person runs from inside their working tree. It will use the
//! server as a `caos` git remote — building objects in the local repo and
//! exchanging them with the server by negotiated push/fetch — but for now it
//! shares the worker's HTTP transport ([`caos::HttpTransport`]); swapping in the
//! git remote is the next step.
//!
//! Subcommands: `get-hash`, `get`, `put`, `import-image`, `resolve`, `run`,
//! `curry`, `build-args`. (There is no `entrypoint` — that's the worker's job.)

use std::process::ExitCode;

use caos::{prog_name, HttpTransport};

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
        // `run <image> <output> -- [--name=value ...]`. The `--` separates the
        // fixed arguments from the (possibly empty) list of key/value args.
        Some("run") => match &args[2..] {
            [image, output, sep, kvs @ ..] if sep == "--" => {
                caos::caos_run(&transport()?, image, output, kvs)
            }
            _ => Err(usage(args)),
        },
        // `curry <image> -- [--name=value ...]` — bind args to an image, printing
        // a ref to the resulting curried image (run/curry it like any image).
        Some("curry") => match &args[2..] {
            [image, sep, kvs @ ..] if sep == "--" => caos::caos_curry(&transport()?, image, kvs),
            _ => Err(usage(args)),
        },
        // `build-args [--name=value ...]` — print the hash of the assembled args
        // tree (path values stored from disk, everything else a literal blob).
        Some("build-args") => caos::build_args(&transport()?, &args[2..]),
        _ => Err(usage(args)),
    }
}

/// For now the CLI uses the same HTTP transport as the worker; the git remote
/// transport replaces this in the next step.
fn transport() -> Result<HttpTransport, String> {
    HttpTransport::from_env()
}

fn usage(args: &[String]) -> String {
    let prog = prog_name(args);
    format!(
        "usage:\n  {prog} get-hash <hash> <path>\n  \
         {prog} get [-r | --recursive[=<depth>]] <path>\n  \
         {prog} put <src-path> <cas-path>\n  \
         {prog} import-image <docker-archive> <cas-path>\n  \
         {prog} resolve <ref>\n  \
         {prog} run <image> <output-cas-path> -- [--name=value ...]\n  \
         {prog} curry <image> -- [--name=value ...]\n  \
         {prog} build-args [--name=value ...]"
    )
}
