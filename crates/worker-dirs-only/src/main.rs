//! caos-worker-dirs-only: keeps only a node's directory children, dropping file
//! (and any other non-directory) children.
//!
//! It receives the node as `--in` and leaves the filtered children tree at
//! /cas/out: one entry per surviving directory child, under its original name and
//! pointing at that child's unchanged subtree. A non-directory `--in` (e.g. a file
//! leaf) has no children, so the output is an empty tree. It only touches the
//! server (no `caos run`); the server injects that URL at runtime. This is the
//! `worker-dirs-only` crate, a static binary at /worker — so the image needs no
//! shell or coreutils.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{arg, caos, entries, file_name, link, path, run_worker, scratch};

fn main() -> ExitCode {
    run_worker("dirs-only", run)
}

fn run() -> Result<(), String> {
    let in_path = arg("in");
    let work = scratch("dirs-only")?;

    // Only a directory has children to filter; anything else (a file leaf) has
    // none, so its child tree is empty.
    if Path::new(&in_path).is_dir() {
        caos(["get", &in_path])?; // one level: a placeholder per child
        for child in entries(&in_path)? {
            // Keep directory children; drop files (and other non-directory
            // entries). symlink_metadata, not is_dir(), so a symlink to a
            // directory isn't followed and kept.
            let is_dir = fs::symlink_metadata(&child)
                .map(|m| m.is_dir())
                .map_err(|e| format!("stat {}: {e}", child.display()))?;
            if is_dir {
                let name = file_name(&child);
                // Link the kept child into the result tree under its original
                // name; `caos put` resolves the link to its recorded hash, so the
                // subtree is referenced, not re-read.
                link(&child, work.join(&name))?;
                eprintln!("dirs-only: keeping directory {name}");
            } else {
                eprintln!("dirs-only: dropping non-directory {}", file_name(&child));
            }
        }
    }

    caos(["put", path(&work), "/cas/out"])
}
