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

use std::process::ExitCode;

use caos::{prog_name, GitTransport};

mod tui;

fn main() -> ExitCode {
    ensure_helper_on_path();
    caos::install_ticket_transport(Box::new(IrohTransport {
        client: std::sync::OnceLock::new(),
    }));
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
        // `run [output] --base:<type>=<image> [...]`. `[output]` is a host path
        // to check the result out to — the run's only positional, and the only
        // token that isn't a `--flag`. Every `--name[:type]=value` after it is a
        // computation argument and therefore part of the ArgTree (the cache
        // key), including the reserved `--base`, which names the worker to run.
        Some("run") => {
            let (output, kvs) = match &args[2..] {
                [] => return Err(usage(args)),
                [output, kvs @ ..] if !output.starts_with("--") => (Some(output.as_str()), kvs),
                kvs => (None, kvs),
            };
            caos::cli_run(&transport()?, output, kvs)
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
        // `mcp hook` / `mcp serve` — record a Claude Code session as an ordinary
        // conversation. Its transport is not this one: the process is started
        // by Claude Code, not by a person standing in the repository.
        Some("mcp") => caos_cli::cli_mcp(cc_transport(), &args[2..]),
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
        // "<kind> <hash>". With no --tree, the tracked source tree is the
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
        // `status [--all] <arg tree hash>` — what is running under an ArgTree,
        // or (with --all) what happened, as JSON
        // (SPEC.md "Tracing"). The same view `run`/`run-tool` show live, for a
        // run happening elsewhere or one you want to look at after the fact.
        Some("status") => match &args[2..] {
            [arg_tree] => caos::cli_status(&transport()?, arg_tree, false),
            [flag, arg_tree] if flag == "--all" => caos::cli_status(&transport()?, arg_tree, true),
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

/// The transport for `caos mcp`, which is NOT started by a person in a shell.
///
/// Claude Code spawns the hook and the tool server itself, and neither one's
/// working directory is contractually the project: in a cloud session the
/// checkout sits at `/home/user/<repo>` while `$HOME` resolves to `/root`. A
/// wrong cwd does not produce a wrong answer here, it produces NO answer --
/// `gix::discover` fails, the process exits before it has spoken, and Claude
/// Code reports `CONNECTION_CLOSED`, which names neither the directory nor the
/// repository it wanted. `$CLAUDE_PROJECT_DIR` is what Claude Code sets for
/// exactly this, so ask it before falling back to where we happen to stand.
fn cc_transport() -> Result<GitTransport, String> {
    let t = match std::env::var("CLAUDE_PROJECT_DIR") {
        Ok(dir) if !dir.is_empty() => GitTransport::discover(&dir)
            .map_err(|error| format!("CLAUDE_PROJECT_DIR={dir}: {error}"))?,
        _ => GitTransport::from_cwd()?,
    };
    // And then STAND there. Finding the repository is not enough on its own:
    // plenty below here resolves a relative path against the process's cwd
    // rather than against the transport, so a correct workspace reached from
    // the wrong directory still fails -- and it fails as ".: outside the git
    // worktree", which reads as a broken entry rather than a wrong directory.
    // One chdir fixes every such caller at once, and leaves this process where
    // a person running caos by hand would be standing anyway.
    std::env::set_current_dir(t.work_dir())
        .map_err(|error| format!("entering {}: {error}", t.work_dir().display()))?;
    Ok(t)
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
         {prog} run [output] --base:<type>=<image> [--name=value | --name:@=path ...]\n  \
         {prog} curry [--unbind=<name> ...] --base:<type>=<arg tree> [--name=value | --name:@=path ...]\n    \
         (an image is --base:@=<dir>, --base:docker=<ref> or --base:hash=<oid>)\n  \
         {prog} prepare-request --base:<type>=<image-or-arg tree> [--name=value | --name:@=path ...]\n  \
         {prog} import-image [--base docker://<ref>] <docker-archive>\n  \
         {prog} talk [<prompt>] [-c <name>] [--new] [--log] [--username <name>] [conversation options]\n  \
         {prog} tui [-c <name>] [--new] [--username <name>] [conversation options]\n  \
         {prog} chat <name> [-m <message>] [--base <revspec>] [--log] [--username <name>] [conversation options]\n    \
         (a conversation names its two workers: --llm-step:@=<path> --llm-call:@=<path>,\n     \
         typed like any image arg — caos-std/<name> in a repo that mounted caos)\n  \
         {prog} mcp <hook | serve | warm> [--llm-step:@=<path>]\n    \
         (Claude Code: the hook that records a session, and the MCP tool server it spawns)\n  \
         {prog} run-tool <script | name> [--name=value ...]\n  \
         {prog} eval-path [--tree=<oid>] <path>\n  \
         {prog} get <hash> <path>\n  \
         {prog} status [--all] <arg tree hash>\n  \
         {prog} secrets [--check]"
    )
}

/// Teach this binary to reach a `caos://` server.
///
/// The transport is INSTALLED rather than linked into `caos` itself, because
/// that crate is also the worker's setuid `/bin/caos` and cargo unifies features
/// across workspace members — a dependency there would bake iroh, quinn and
/// tokio into every worker image. A host binary is the right place to pay for
/// it, and this is the host binary.
///
/// Lazy in the useful sense: the client is built on the first `caos://` request,
/// so an ordinary HTTP-remote run never binds an endpoint or starts a runtime.
struct IrohTransport {
    client: std::sync::OnceLock<Result<caos_iroh::http::HttpClient, String>>,
}

impl caos::TicketTransport for IrohTransport {
    fn request(
        &self,
        base: &str,
        request: &caos::ServerRequest,
    ) -> Result<caos::ServerResponse, String> {
        // Set CAOS_IROH_TRACE to see each request and how long it took. A ticket
        // server is remote by definition, so "which call is slow" is the first
        // question about any run that feels wrong, and it is invisible otherwise.
        let trace = std::env::var_os("CAOS_IROH_TRACE").is_some();
        if trace {
            eprintln!("caos-iroh: -> {} {}", request.method, request.path);
        }
        let started = std::time::Instant::now();
        let answer = self.send(base, request);
        if trace {
            match &answer {
                Ok(response) => eprintln!(
                    "caos-iroh: <- {} {} {} in {:.3}s ({} bytes)",
                    request.method,
                    request.path,
                    response.status,
                    started.elapsed().as_secs_f64(),
                    response.body.len()
                ),
                Err(error) => eprintln!(
                    "caos-iroh: <- {} {} failed in {:.3}s: {error}",
                    request.method,
                    request.path,
                    started.elapsed().as_secs_f64()
                ),
            }
        }
        answer
    }
}

impl IrohTransport {
    fn send(
        &self,
        base: &str,
        request: &caos::ServerRequest,
    ) -> Result<caos::ServerResponse, String> {
        let client = self
            .client
            .get_or_init(caos_iroh::http::HttpClient::new)
            .as_ref()
            .map_err(String::clone)?;
        let response = client.request(
            base,
            request.method,
            request.path,
            request.headers,
            request.body,
            request.timeout_secs.map(std::time::Duration::from_secs),
        )?;
        Ok(caos::ServerResponse {
            status: response.status,
            reason: response.reason,
            body: response.body,
        })
    }
}

/// Put this binary's own directory on PATH, so `git` can find
/// `git-remote-caos`.
///
/// A `caos://` remote is served by a helper git execs BY NAME off PATH, and
/// `caos-cli` shells out to git for every push and fetch — so a client that can
/// reach a ticket server itself, while the git it drives cannot, fails halfway
/// through a run with `git: 'remote-caos' is not a git command`. Which is what
/// happened the first time this was tried end to end.
///
/// Derived from `current_exe` rather than baked in at build time, so it holds for
/// a cargo target directory, a nix store path and a copied-out binary alike —
/// the helper always ships beside the client.
///
/// APPENDED, not prepended: this is about adding a name nothing else provides,
/// not about winning over the user's own tools. A directory already on PATH is
/// left where it is.
fn ensure_helper_on_path() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    if std::env::split_paths(&path).any(|entry| entry == dir) {
        return;
    }
    let mut entries: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
    entries.push(dir.to_path_buf());
    if let Ok(joined) = std::env::join_paths(entries) {
        // SAFETY: called once at the top of main, before any thread is spawned.
        unsafe { std::env::set_var("PATH", joined) };
    }
}
