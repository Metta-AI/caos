//! caos: the worker-side client, baked setuid-root into worker images as
//! `/bin/caos`.
//!
//! It speaks HTTP to the server (`/object`, for storage) via
//! [`caos::HttpTransport`], and provides the container `runner` — which runs a
//! job (set up the root-owned `/cas`, run `/worker` as an unprivileged user,
//! post the kind + hash recorded at `/cas/out` back to the server), then
//! long-polls for more work for its image until an idle TTL passes (see
//! `design/runner-protocol.md`). It normally records continuations that the
//! server resolves after the worker's job finishes; `sub-run` starts detached
//! work while retaining the current server-side run context. The
//! shared command logic lives in the `caos` library; this binary is the worker's
//! CLI surface plus the privileged runner.
//!
//! Subcommands: `get-hash`, `get`, `put`, `put-commit`, `hash`, `forward`, `map-then`,
//! `run-then`, `run-request-then`, `sub-run`, `prepare-request`, `curry`, `next`
//! and `runner`. (Image import and ref resolution are user-facing only — see
//! `caos-cli`.)

use std::process::ExitCode;

use caos::{prog_name, HttpTransport};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // This is the worker-side binary, so the job context a resident runner
    // writes under /cas is for it to read (see `caos::runner::job_nonce`).
    caos::runner::trust_job_context_files();
    // `caos next` has a distinguished exit status for "stop", which is not an
    // error and so cannot be an `Err`.
    if args.get(1).map(String::as_str) == Some("next") {
        return match caos::runner::next_client(&args[2..]) {
            Ok(status) => ExitCode::from(status),
            Err(err) => {
                eprintln!("{}: {err}", prog_name(&args));
                ExitCode::FAILURE
            }
        };
    }
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
        Some("import-git") => caos::import_git::run(&args[2..]),
        Some("push-git") => caos::push_git::run(&args[2..]),
        Some("get-hash") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(hash), Some(path), None) => caos::get_hash(&http()?, hash, path),
            _ => Err(usage(args)),
        },
        Some("kind") => match &args[2..] {
            [path] => caos::cas_kind(path),
            _ => Err(usage(args)),
        },
        Some("resolve") => match &args[2..] {
            [hash, relative, destination] => {
                caos::gitlinks::resolve(&http()?, hash, relative, destination)
            }
            _ => Err(usage(args)),
        },
        Some("get") => {
            let (path, depth) = caos::parse_get(&args[2..])?;
            caos::get(&http()?, path, depth)
        }
        Some("put") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(src), Some(dst), None) => caos::put(&http()?, src, dst),
            _ => Err(usage(args)),
        },
        // `put-commit <src-file> <cas-path>` — store the file's bytes as a git
        // *commit* object, record it (kind-tagged) at the CAS path, and print
        // its hash. How a worker mints a turn/step commit.
        Some("put-commit") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(src), Some(dst), None) => caos::put_commit(&http()?, src, dst),
            _ => Err(usage(args)),
        },
        // `hash <cas-path>` — print the git hash recorded on a CAS path (e.g. a
        // commit-valued arg whose hash becomes the next commit's parent).
        Some("hash") => match (args.get(2), args.get(3)) {
            (Some(path), None) => caos::cas_hash(path),
            _ => Err(usage(args)),
        },
        // `forward <src> <dst>` — preserve a blob/tree/commit CAS value under
        // another path without fetching or re-uploading it.
        Some("forward") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(src), Some(dst), None) => caos::forward(src, dst),
            _ => Err(usage(args)),
        },
        // `map-then <in> [--map:<type>=<image>] [--then:<type>=<image>]` —
        // record a map-then
        // continuation over the CAS path `<in>` as this worker's result at
        // /cas/out (a tail call; the server resolves it after the worker exits).
        Some("map-then") => match &args[2..] {
            [input, kvs @ ..] => caos::caos_map_then(&http()?, input, kvs),
            _ => Err(usage(args)),
        },
        // `run-then <in> --run:<type>=<image> [--then:<type>=<image>] [--catch]` — the
        // single-valued map-then: the server runs `run(--in=<in>)` once, then
        // (optionally) `then(--in=<in>, --result=<R>)`. The same tail-call
        // contract. With `--catch`, a failing `run` reaches `then` as
        // `--error=<blob>` instead of failing the whole request.
        Some("run-then") => match &args[2..] {
            [input, kvs @ ..] => caos::caos_run_then(&http()?, input, kvs),
            _ => Err(usage(args)),
        },
        // `eval-path-then <in> --eval=<path> [--then:<type>=<image>] [--catch]` — the
        // evaluation sibling of run-then: the server walks `.caos-expr` from
        // `<in>`'s root down to `<path>` (blocking a request thread, not a worker
        // slot), then (optionally) `then(--in=<in>, --result=<R>)`. How a worker
        // gets a `.caos-expr` evaluated without blocking (design/caos-expr.md).
        Some("eval-path-then") => match &args[2..] {
            [input, kvs @ ..] => caos::caos_eval_then(&http()?, input, kvs),
            _ => Err(usage(args)),
        },
        // `run-request-then <R> [--then:<type>=<image>] [--catch]` — tail-call
        // the exact, already-complete ArgTree R, optionally delivering its
        // result (or caught error) to a callback image.
        Some("run-request-then") => match &args[2..] {
            [request, kvs @ ..] => caos::caos_run_request_then(&http()?, request, kvs),
            _ => Err(usage(args)),
        },
        // `trace-child <name> <arg-tree>` — record under this job that it
        // started work on another stack, so `status` descends into it.
        Some("trace-child") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(name), Some(arg_tree), None) => caos::caos_trace_child(&http()?, name, arg_tree),
            _ => Err(usage(args)),
        },
        // Start an already-stored ArgTree without waiting, preserving this
        // job's server-side run stack and secret store.
        Some("sub-run") => match &args[2..] {
            [arg_tree] => caos::caos_sub_run(&http()?, arg_tree),
            _ => Err(usage(args)),
        },
        // `prepare-request --base:<type>=<image> [...]` — construct and store the
        // exact flat runnable ArgTree without executing it. This is the durable
        // identity accepted by sub-run.
        Some("prepare-request") => caos::caos_prepare_request(&http()?, &args[2..]),
        // `resolve-image <hash|docker://ref>` — the reference a runner would
        // pull. Converts (and caches) a git-docker tree exactly as a run would.
        Some("resolve-image") => caos::caos_resolve_image(&args[2..]),
        // `curry [--unbind=<name> ...] --base:<type>=<arg tree> [--name=value | --name:@=path ...]` —
        // bind args to the `--base` ArgTree (a bare image, a curry node, or a flat
        // args tree like `own_args_tree`), printing a ref to the resulting curried
        // ArgTree (run/curry it like any other). `--unbind` releases a bound arg
        // so it can be rebound.
        Some("curry") => caos::caos_curry(&http()?, &args[2..]),
        // `runner --job=<json>` — run the handed-in job, then poll for more; see
        // `caos::runner::run`.
        Some("runner") => match &args[2..] {
            [flag] => match flag.strip_prefix("--job=") {
                Some(json) => caos::runner::run(json),
                None => Err(usage(args)),
            },
            _ => Err(usage(args)),
        },
        _ => Err(usage(args)),
    }
}

/// The worker talks to the server over HTTP.
fn http() -> Result<HttpTransport, String> {
    HttpTransport::from_env()
}

// import-git <https-url> <commit> [--github-token-file=<path>]
fn usage(args: &[String]) -> String {
    let prog = prog_name(args);
    format!(
        "usage:\n  {prog} import-git <https-url> <commit> [--github-token-file=<path>]\n  {prog} push-git <https-url> <commit> <branch> --expected=<hash|absent> [--force] [--github-token-file=<path>]\n  {prog} resolve <hash> <relative-path> <cas-path>\n  {prog} kind <cas-path>\n  {prog} get-hash <hash> <path>\n  \
         {prog} get [-r | --recursive[=<depth>]] <path>\n  \
         {prog} put <src-path> <cas-path>\n  \
         {prog} put-commit <src-file> <cas-path>\n  \
         {prog} hash <cas-path>\n  \
         {prog} forward <src-cas-path> <dst-cas-path>\n  \
         {prog} map-then <in-cas-path> [--map:<type>=<image>] [--then:<type>=<image>]\n      [--max-parallel=<n>]\n  \
         {prog} run-then <in-cas-path> --run:<type>=<image> [--then:<type>=<image>] [--catch]\n  \
         {prog} eval-path-then <in-cas-path> --eval=<path> [--then:<type>=<image>] [--catch]\n  \
         {prog} run-request-then <arg-tree-hash|cas-path> [--then:<type>=<image>] [--catch]\n  \
         {prog} sub-run <arg-tree-hash>\n  \
         {prog} trace-child <name> <arg-tree-hash>\n  \
         {prog} prepare-request --base:<type>=<image-or-arg tree> [--name=value | --name:@=path ...]\n  \
         {prog} resolve-image <hex hash | docker://<ref>>\n  \
         {prog} curry [--unbind=<name> ...] --base:<type>=<arg tree> [--name=value | --name:@=path ...]\n    \
         (an image is :@=<cas path>, :docker=<ref> or :hash=<oid>)\n  \
         {prog} next [--error <text> | --stream]\n  \
         {prog} runner --job=<json>"
    )
}
