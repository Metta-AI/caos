//! caos-worker-llm-summary: summarize a document tree, folding each file's
//! summary up into directory summaries.
//!
//! It drives `caos-worker-fold` over a tree of documents with this same image
//! curried in as the fold's `post` (there is no `pre` — a document tree's own
//! structure *is* the fold structure: a directory's children are its entries, a
//! file is a leaf). So the public API is just:
//!
//!   caos run <llm-summary> <out> -- --in:@=<doc-tree>
//!
//! The result mirrors the input tree: every node is `{ summary, <children…> }` —
//! a `summary` blob plus, for a directory, each child's result subtree (carried
//! through by hash). So `<out>/summary` is the whole tree's summary, and
//! `<out>/guide/intro.md/summary` is one file's.
//!
//! Incrementality is pure CAOS memoization: `post` is curried with nothing, so a
//! node's summary is keyed only on its content. Edit one file and only that
//! file's summary and the directory summaries on the path to the root recompute;
//! every sibling summary is a cache hit. (The driver and fold orchestration are
//! cheap and re-run on any edit, exactly like deep-deps.) Note this gives
//! *determinism by memoization* even over a nondeterministic model: an unchanged
//! node is returned from cache (byte-identical), never re-sampled.
//!
//! The summarize step calls a real model (the Anthropic Messages API) when an
//! API key is supplied as the `--key` argument; the worker copies it into
//! `ANTHROPIC_API_KEY` for the call. With no key it falls back to a deterministic
//! local stand-in, so the demo still runs anywhere. The driver curries `--key`
//! into `post`, so every summarize invocation receives it.
//!
//! NOTE: because `--key` is an *argument*, it rides in the content-addressed
//! request — it's stored in the CAS and folded into the cache key (so rotating
//! the key re-summarizes everything). That's a deliberate "for now" choice: the
//! worker owns its credential rather than the server forwarding ambient env. The
//! model id and prompts are baked into THIS image (consts below), so they ride in
//! the cache key too — changing either correctly invalidates every summary.

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::thread::sleep;
use std::time::Duration;

use worker_common::{
    arg, caos, caos_curry, caos_run, entries, file_name, link, path, read_arg_opt, run_worker,
    scratch, std_image, Arg,
};

// ---- model configuration (part of the image, hence the cache key) -------------

/// The model the summarizer calls. Opus is the default per Anthropic's guidance;
/// for a many-call document fold `claude-haiku-4-5` is far cheaper and faster —
/// swap it here if cost matters more than summary quality for your tree.
const MODEL: &str = "claude-opus-4-8";

/// Output cap for a one/two-sentence summary.
const MAX_TOKENS: u32 = 512;

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// CA bundle baked into the image (from `cacert`), for curl's TLS verification.
const CA_BUNDLE: &str = "/etc/ssl/certs/ca-bundle.crt";

const SYSTEM_LEAF: &str = "You are a concise technical summarizer. Given a document, reply with a \
     one or two sentence summary of what it contains, and nothing else.";
const SYSTEM_DIR: &str =
    "You are a concise technical summarizer. Given the summaries of the entries \
     in a directory, reply with a one or two sentence summary of what the directory as a whole \
     contains, and nothing else.";

/// Stands in for an LLM round-trip's latency on the *no-key* fallback path, so a
/// fresh summary is visibly "expensive" and the warm-cache speedup is dramatic. A
/// cached node skips the worker entirely (the server returns the memoized hash
/// without spawning a container), so it pays none of this — which is what the
/// demo shows. The real-model path has its own (larger) latency, so no sleep.
const SIMULATED_LLM_LATENCY: Duration = Duration::from_millis(200);

fn main() -> ExitCode {
    run_worker("llm-summary", run)
}

fn run() -> Result<(), String> {
    // `--mode` is internal: the driver (no mode) curries this image as the fold's
    // `post` in `summarize` mode. A caller only ever uses the no-mode API.
    match read_arg_opt("mode")?.as_deref() {
        None | Some("") => drive(),
        Some("summarize") => summarize(),
        Some(other) => Err(format!("unknown mode {other:?}")),
    }
}

// ---- driver: fold `summarize` over the document tree --------------------------

/// Default mode: fold this image (as `post`) over the `--in` document tree,
/// leaving the tree of summaries at `/cas/out`. No `pre` — a directory's children
/// are its own entries and a file is a leaf, which is the fold's structural
/// default.
fn drive() -> Result<(), String> {
    // Curry the optional --key into `post` so every summarize invocation (the only
    // place the API call happens) receives it. No key -> the local stand-in.
    let key = read_arg_opt("key")?;
    let mut post_args = vec![("mode", Arg::Lit("summarize"))];
    if let Some(key) = &key {
        post_args.push(("key", Arg::Lit(key.as_str())));
    }
    let post = caos_curry(&me(), &post_args)?;
    let in_path = arg("in");
    caos_run(
        &fold_image(),
        "/cas/out",
        &[("post", Arg::Lit(&post)), ("in", Arg::Path(&in_path))],
    )
}

// ---- the fold algebra: how one node becomes a summary -------------------------

/// The fold's `post`. A file leaf (`--in` is a blob) is summarized from its
/// contents; a directory (`--in` is a tree) is summarized from its children's
/// summaries (`--children`, each a child's already-folded result). The result is
/// a node tree `{ summary, <children…> }`: the node's own `summary` blob plus,
/// for a directory, each child's result subtree carried through by hash.
fn summarize() -> Result<(), String> {
    // The key (if any) was curried in as --key; copy it into the environment so
    // the API call reads it like any ANTHROPIC_API_KEY.
    if let Some(key) = read_arg_opt("key")? {
        std::env::set_var("ANTHROPIC_API_KEY", key);
    }
    let in_path = arg("in");
    let node = scratch("node")?;

    let summary = if Path::new(&in_path).is_file() {
        // Leaf: fetch the file's bytes and summarize them.
        caos(["get", &in_path])?;
        let bytes = fs::read(&in_path).map_err(|e| format!("reading {in_path}: {e}"))?;
        summarize_text(&String::from_utf8_lossy(&bytes))?
    } else {
        // Directory: combine the children's summaries, and carry each child's
        // result subtree through under its name (by hash — no content re-read).
        let children = arg("children");
        caos(["get", &children])?; // one level: a placeholder per child result
        let mut parts = Vec::new();
        for child in entries(&children)? {
            let name = file_name(&child);
            caos(["get", path(&child)])?; // one level: reveals the child's `summary`
            let child_summary = child.join("summary");
            caos(["get", path(&child_summary)])?; // expand the child's summary blob
            let text = fs::read_to_string(&child_summary)
                .map_err(|e| format!("reading {}: {e}", child_summary.display()))?;
            parts.push((name.clone(), text));
            link(&child, node.join(&name))?; // carry the whole child subtree by hash
        }
        combine_summaries(&parts)?
    };

    fs::write(node.join("summary"), summary).map_err(|e| format!("writing summary: {e}"))?;
    caos(["put", path(&node), "/cas/out"])
}

// ---- the "model": real Anthropic call, or a deterministic stand-in ------------

/// The API key for the call, read from the environment (the worker copies the
/// `--key` argument into `ANTHROPIC_API_KEY` in `summarize`). Empty/unset means
/// run the local stand-in instead of calling the API.
fn anthropic_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
}

/// Summarize a leaf document's text.
fn summarize_text(text: &str) -> Result<String, String> {
    match anthropic_key() {
        Some(key) => anthropic(
            &key,
            SYSTEM_LEAF,
            &format!("Summarize this document:\n\n{text}"),
        ),
        None => {
            sleep(SIMULATED_LLM_LATENCY);
            Ok(local_summarize_text(text))
        }
    }
}

/// Summarize a directory from its children's summaries.
fn combine_summaries(parts: &[(String, String)]) -> Result<String, String> {
    match anthropic_key() {
        Some(key) => {
            let listing = parts
                .iter()
                .map(|(name, child)| format!("- {name}: {}", child.trim()))
                .collect::<Vec<_>>()
                .join("\n");
            anthropic(
                &key,
                SYSTEM_DIR,
                &format!("Summarize this directory given its entries' summaries:\n\n{listing}"),
            )
        }
        None => {
            sleep(SIMULATED_LLM_LATENCY);
            Ok(local_combine_summaries(parts))
        }
    }
}

/// Call the Anthropic Messages API by shelling out to `curl` (the image bundles
/// curl + a CA bundle; the static worker binary stays pure-Rust — no TLS stack to
/// cross-compile). The request body goes through a temp file so the document text
/// never rides in argv. Returns the first text block, trimmed.
fn anthropic(key: &str, system: &str, user: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "messages": [{ "role": "user", "content": user }],
    });
    let body = serde_json::to_vec(&body).map_err(|e| format!("encoding request: {e}"))?;
    let req = scratch("llm-req")?.join("body.json");
    fs::write(&req, &body).map_err(|e| format!("writing request body: {e}"))?;

    // `--fail-with-body` makes curl exit non-zero on an HTTP error while still
    // printing the (JSON error) body, so we can surface the API's message.
    let out = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--max-time",
            "120",
        ])
        .args(["--cacert", CA_BUNDLE])
        .args(["-X", "POST", ANTHROPIC_URL])
        .args(["-H", "content-type: application/json"])
        .args(["-H", &format!("anthropic-version: {ANTHROPIC_VERSION}")])
        .args(["-H", &format!("x-api-key: {key}")])
        .arg("--data-binary")
        .arg(format!("@{}", req.display()))
        .output()
        .map_err(|e| format!("running curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "anthropic request failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stdout).trim()
        ));
    }

    let resp: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("parsing response: {e}"))?;
    if let Some(err) = resp.get("error") {
        return Err(format!("anthropic error: {err}"));
    }
    // The summary is the first text block: content[] | select(.type=="text") | .text
    let text = resp
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks.iter().find_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| {
            format!(
                "no text block in response: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })?;
    Ok(format!("{}\n", text.trim()))
}

/// Deterministic stand-in for a leaf summary (no API key): a summary-shaped digest
/// so the demo runs anywhere and the incremental-recompute behaviour is identical.
fn local_summarize_text(text: &str) -> String {
    let words = text.split_whitespace().count();
    let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
    let gist: String = text
        .split_whitespace()
        .take(16)
        .collect::<Vec<_>>()
        .join(" ");
    format!("file · {words} words, {lines} non-blank lines · {gist}\n")
}

/// Deterministic stand-in for a directory summary (no API key).
fn local_combine_summaries(parts: &[(String, String)]) -> String {
    let mut out = format!("dir · {} entries\n", parts.len());
    for (name, child) in parts {
        let first = child.lines().next().unwrap_or("").trim();
        out.push_str(&format!("  - {name}: {first}\n"));
    }
    out
}

/// This image, to curry as the fold's `post` — the built-in `/cas/std/llm-summary`.
fn me() -> String {
    std_image("llm-summary")
}

/// The fold worker's image — the built-in `/cas/std/fold`.
fn fold_image() -> String {
    std_image("fold")
}
