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
//! cheap and re-run on any edit, exactly like deep-deps.)
//!
//! The "summarize" step is a deterministic local stand-in so the demo runs
//! anywhere — no API key, no network egress — with a small simulated latency for
//! an LLM round-trip. `summarize_text` / `combine_summaries` are the seam where a
//! real LLM call goes; see the note there for what making it real needs.

use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use worker_common::{
    arg, caos, caos_curry, caos_run, entries, file_name, link, path, read_arg_opt, run_worker,
    scratch, std_image, Arg,
};

/// Stands in for an LLM round-trip's latency, so a fresh summary is visibly
/// "expensive" and the warm-cache speedup is dramatic. A cached node skips the
/// worker entirely (the server returns the memoized hash without spawning a
/// container), so it pays none of this — which is exactly what the demo shows.
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
    let post = caos_curry(&me(), &[("mode", Arg::Lit("summarize"))])?;
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
    let in_path = arg("in");
    let node = scratch("node")?;

    let summary = if Path::new(&in_path).is_file() {
        // Leaf: fetch the file's bytes and summarize them.
        caos(["get", &in_path])?;
        let bytes = fs::read(&in_path).map_err(|e| format!("reading {in_path}: {e}"))?;
        summarize_text(&String::from_utf8_lossy(&bytes))
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
        combine_summaries(&parts)
    };

    fs::write(node.join("summary"), summary).map_err(|e| format!("writing summary: {e}"))?;
    caos(["put", path(&node), "/cas/out"])
}

// ---- the "model": a deterministic stand-in for an LLM -------------------------
//
// This is the seam for a real LLM. To make it real:
//   * call the Anthropic Messages API here. Bake the model id and prompt into
//     THIS image (as consts), so they ride in the request's cache key — changing
//     the prompt or model then correctly invalidates every summary that used it.
//   * pass the API key through the worker's *environment*, never an arg, so it
//     stays out of the cache key. That needs the server to forward
//     ANTHROPIC_API_KEY into the worker container (a few lines in compute.rs,
//     mirroring how it injects CAOS_STD / CAOS_SALT).
//   * give the worker a TLS HTTP client (e.g. minreq with `https-rustls`); note
//     rustls/ring pull in C/asm, so confirm they still build static-musl under
//     the image build (the flake's "native C deps" caveat).
// Until then this is a deterministic, summary-shaped digest so the demo runs
// anywhere and the incremental-recompute behaviour is identical.

/// Summarize a leaf document's text.
fn summarize_text(text: &str) -> String {
    sleep(SIMULATED_LLM_LATENCY); // stands in for an LLM round-trip
    let words = text.split_whitespace().count();
    let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
    let gist: String = text
        .split_whitespace()
        .take(16)
        .collect::<Vec<_>>()
        .join(" ");
    format!("file · {words} words, {lines} non-blank lines · {gist}\n")
}

/// Summarize a directory from its children's summaries.
fn combine_summaries(parts: &[(String, String)]) -> String {
    sleep(SIMULATED_LLM_LATENCY); // stands in for an LLM round-trip
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
