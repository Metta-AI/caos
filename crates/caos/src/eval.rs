//! `.caos-expr`: evaluable trees (design/caos-expr.md).
//!
//! A `.caos-expr` file makes the directory it sits in *evaluable*: instead of
//! being taken verbatim, the directory's contents are computed by running the
//! expression the file holds. `caos-cli eval-path <path>` walks a tree from the
//! root down to `<path>`, and wherever a `.caos-expr` sits along the way it
//! evaluates that expression — with the directory's own subtree as the input —
//! and continues descending into the *result*. The final object at `<path>` is
//! returned. Evaluation reuses the ordinary ArgTree/curry/run pipeline, so a
//! `run` expression caches exactly like a hand-written `caos-cli run`.
//!
//! ## Grammar
//!
//! One `.caos-expr` is a sequence of lines. Blank lines and `#` comments are
//! ignored. Any line but the last may bind a variable; the last line is the
//! file's *value*:
//!
//! ```text
//! NAME=run   <image> -- [--k=v | --k:@=path]
//! NAME=curry <image> -- [--k=v | --k:@=path]
//! curry $NAME -- --worker1:@=path         # the value (a run/curry, or a bare $NAME)
//! ```
//!
//! Variable names are `[A-Z][A-Z0-9_]*`; the verbs (`run`, `curry`) are
//! lowercase, so a line is an assignment iff it starts `NAME=run`/`NAME=curry`.
//! A `run` value evaluates to the run's result; a `curry` value to the curried
//! ArgTree. `$NAME` is the object a prior line produced.
//!
//! ## Argument resolution
//!
//! Paths in args are relative to the directory containing the `.caos-expr`
//! (i.e. resolved against the input subtree), NOT the host or `/cas`:
//! - `--k=value` — a literal blob, unless `value` is exactly `$NAME`, which
//!   binds the object that variable holds (by reference, at its own kind).
//! - `--k:@=path` — the object at `path` within the input subtree.
//! - `/std/<name>` (image position or a `:@=` value) — the published builtin,
//!   resolved as elsewhere ("interpreted as normal for now").

use std::collections::HashMap;

use gix::objs::tree::{Entry, EntryKind, EntryMode};

use super::{
    assemble_arg_tree, curry_from_entries, entry_name, fetch_tree_entries, is_hex_hash, parse_oid,
    post_object, request_compute, resolve_std_image, Transport,
};

/// Evaluate a std library entry named `<name>` within the std tree `std_tree`
/// (design/caos-expr.md, Phase 3). This is just [`eval_path`] with the entry
/// name as the path: a std-root `.caos-expr` is applied to the whole tree first
/// (e.g. a `deep-deps` transform), then the named entry's own `.caos-expr` (if
/// any) computes its image — so both the direct-image form and the
/// source-plus-`.caos-expr` form resolve identically for every
/// `/cas/std/<name>` consumer.
pub(crate) fn eval_std_entry(
    t: &dyn Transport,
    std_tree: &str,
    name: &str,
) -> Result<String, String> {
    eval_path(t, std_tree, name).map(|(_kind, oid)| oid)
}

/// `eval-path [--tree=<oid>] <path>` — evaluate the `.caos-expr` files from the
/// root of the tree down to `<path>` and print the resulting object's
/// `"<kind> <hash>"`. With no `--tree`, the tracked workspace tree is the start
/// (dirty edits included, like `run-tool`'s `--in:@=.`).
pub fn cli_eval_path(t: &dyn Transport, tree: Option<&str>, path: &str) -> Result<(), String> {
    let start = match tree {
        Some(oid) => {
            let (kind, _) = t.get_object(oid)?;
            if kind != "tree" {
                return Err(format!("--tree={oid} is a {kind}, not a tree"));
            }
            oid.to_string()
        }
        None => {
            let (_, oid) = t
                .ingest_path(".")?
                .ok_or_else(|| "this client cannot ingest the workspace tree".to_string())?;
            oid.to_string()
        }
    };
    let (kind, hash) = eval_path(t, &start, path)?;
    println!("{kind} {hash}");
    Ok(())
}

/// Walk `start_tree` from its root down to `path`, evaluating every `.caos-expr`
/// encountered (each in the tree its parent produced) and returning the final
/// object's `(kind, oid)`. `node` is the object at the prefix walked so far: a
/// `.caos-expr` at the root of a tree node replaces the node with its result
/// before the next segment is resolved within it.
fn eval_path(t: &dyn Transport, start_tree: &str, path: &str) -> Result<(String, String), String> {
    let comps: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let mut node_kind = String::from("tree");
    let mut node_oid = start_tree.to_string();
    let mut i = 0usize;
    loop {
        // A `.caos-expr` at the root of this tree node transforms the node.
        if node_kind == "tree" {
            if let Some((mode, oid)) = lookup_in_tree(t, &node_oid, ".caos-expr")? {
                if !mode.is_tree() {
                    let (k, o) = eval_expr(t, &node_oid, &oid.to_string())?;
                    node_kind = k;
                    node_oid = o;
                }
            }
        }
        if i == comps.len() {
            break;
        }
        if node_kind != "tree" {
            return Err(format!(
                "eval-path: cannot descend into {:?}: the prefix evaluated to a {node_kind}",
                comps[i]
            ));
        }
        let (mode, oid) = lookup_in_tree(t, &node_oid, comps[i])?
            .ok_or_else(|| format!("eval-path: {:?} not found in {node_oid}", comps[i]))?;
        node_kind = kind_of_mode(mode).to_string();
        node_oid = oid.to_string();
        i += 1;
    }
    Ok((node_kind, node_oid))
}

/// Evaluate one `.caos-expr` blob against `input_tree` (the subtree the file
/// sits at the root of), returning the value's `(kind, oid)`.
fn eval_expr(
    t: &dyn Transport,
    input_tree: &str,
    expr_oid: &str,
) -> Result<(String, String), String> {
    let (kind, content) = t.get_object(expr_oid)?;
    if kind != "blob" {
        return Err(format!(".caos-expr {expr_oid} is a {kind}, not a blob"));
    }
    let text =
        String::from_utf8(content).map_err(|e| format!(".caos-expr {expr_oid} not UTF-8: {e}"))?;

    let mut env: HashMap<String, (String, String)> = HashMap::new();
    let mut value: Option<(String, String)> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if value.is_some() {
            return Err("eval-path: .caos-expr has content after its final expression".to_string());
        }
        if let Some((name, cmd)) = parse_assignment(line) {
            let v = eval_command(t, input_tree, cmd, &env)?;
            env.insert(name.to_string(), v);
        } else {
            value = Some(eval_value(t, input_tree, line, &env)?);
        }
    }
    value.ok_or_else(|| "eval-path: .caos-expr has no final expression".to_string())
}

/// An assignment line `NAME=<run|curry …>` → `(NAME, "<run|curry …>")`. Any
/// other line (a bare command or `$NAME`) → `None`. The verbs are lowercase and
/// var names uppercase, so the two never blur (a bare `run …` has no leading
/// `NAME=`).
fn parse_assignment(line: &str) -> Option<(&str, &str)> {
    let eq = line.find('=')?;
    let (name, rest) = (&line[..eq], &line[eq + 1..]);
    let mut chars = name.chars();
    let first = chars.next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    match rest.split_whitespace().next() {
        Some("run") | Some("curry") => Some((name, rest)),
        _ => None,
    }
}

/// Evaluate the file's value line: a `run`/`curry` command, or a bare `$NAME`
/// that names a previously bound variable.
fn eval_value(
    t: &dyn Transport,
    input_tree: &str,
    line: &str,
    env: &HashMap<String, (String, String)>,
) -> Result<(String, String), String> {
    if let Some(var) = line.strip_prefix('$') {
        if var.is_empty() || var.split_whitespace().count() != 1 {
            return Err(format!("eval-path: malformed variable reference {line:?}"));
        }
        return env
            .get(var)
            .cloned()
            .ok_or_else(|| format!("eval-path: undefined variable ${var}"));
    }
    eval_command(t, input_tree, line, env)
}

/// Evaluate a single `run <image> -- …` or `curry <image> -- …` command against
/// `input_tree`, returning the result's `(kind, oid)`. A `curry` yields the
/// curried ArgTree (a tree); a `run` triggers compute and yields its result.
fn eval_command(
    t: &dyn Transport,
    input_tree: &str,
    cmd: &str,
    env: &HashMap<String, (String, String)>,
) -> Result<(String, String), String> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let verb = tokens.first().copied().unwrap_or("");
    if verb != "run" && verb != "curry" {
        return Err(format!(
            "eval-path: expected `run` or `curry`, got {verb:?}"
        ));
    }
    let sep = tokens
        .iter()
        .position(|&x| x == "--")
        .ok_or_else(|| format!("eval-path: `{verb}` needs `--` before its args"))?;
    if sep != 2 {
        return Err(format!(
            "eval-path: `{verb}` takes exactly one image before `--`"
        ));
    }
    let image_ref = resolve_expr_image(t, input_tree, tokens[1], env)?;
    let entries = resolve_expr_args(t, input_tree, &tokens[sep + 1..], env)?;

    if verb == "curry" {
        let oid = curry_from_entries(t, &image_ref, &[], entries)?;
        return Ok(("tree".to_string(), oid.to_string()));
    }
    let arg_tree = assemble_arg_tree(t, &image_ref, entries)?;
    let server = t.server_url()?;
    request_compute(&server, &arg_tree)
}

/// Resolve the image token of a command to a ref: a `$NAME` variable, a
/// `/std/<name>` builtin, a bare hash, or a relative path naming an image tree
/// (a flake dir or a git-docker image) within `input_tree`.
fn resolve_expr_image(
    t: &dyn Transport,
    input_tree: &str,
    tok: &str,
    env: &HashMap<String, (String, String)>,
) -> Result<String, String> {
    if let Some(var) = tok.strip_prefix('$') {
        let (_, oid) = env
            .get(var)
            .ok_or_else(|| format!("eval-path: undefined variable ${var}"))?;
        return Ok(oid.clone());
    }
    if let Some(name) = tok.strip_prefix("/std/") {
        return resolve_std_image(t, name);
    }
    // A `docker://<ref>` image passes straight through — it names a registry
    // image, not a tree, so there is nothing to resolve. This is how a core
    // item breaks a resolution-time cycle: `flake-builder`'s `.caos-expr` names
    // its own image as the sentinel `docker://seeded`, so evaluating it never
    // re-enters `/std/flake-builder` (design/caos-expr.md, "Breaking the
    // cycles"). The formed arg-tree carries the ref as a blob (image_arg_entry),
    // exactly what the seeder registers and answers.
    if tok.starts_with(crate::DOCKER_SCHEME) {
        return Ok(tok.to_string());
    }
    if is_hex_hash(tok) {
        return Ok(tok.to_string());
    }
    let (mode, oid) = lookup_in_tree(t, input_tree, tok)?
        .ok_or_else(|| format!("eval-path: image path {tok:?} not found in tree"))?;
    if !mode.is_tree() {
        return Err(format!("eval-path: image path {tok:?} is not a directory"));
    }
    // Evaluate the subtree's own `.caos-expr` (if any) to the image it builds,
    // so a path to an EVALUABLE dependency resolves to its image, not its raw
    // source. This is what lets a core item name a dep by a local path instead
    // of `/std/<name>`: a deep-deps mount at `DEEP-DEPS/<name>` carries that
    // dep's (deepened) source, and `run DEEP-DEPS/<name>` here resolves it
    // through the dep's own `.caos-expr` — the same resolution `/std/<name>`
    // gets. A subtree with NO `.caos-expr` evaluates to itself, so a plain flake
    // dir or a git-docker image tree passes through unchanged. (For a seeded
    // worker this dispatches its build — which is why forming a `run` expr's
    // key needs that worker already seeded; see design/caos-expr.md, the
    // dependency-ordered seeding.)
    let (_kind, hash) = eval_path(t, &oid.to_string(), "")?;
    Ok(hash)
}

/// Resolve a command's `--name[:@]=value` args to tree entries, against
/// `input_tree`. See the module grammar: `$NAME`, a literal blob, or a `:@=`
/// path (within the tree, or a `/std/<name>` builtin).
fn resolve_expr_args(
    t: &dyn Transport,
    input_tree: &str,
    toks: &[&str],
    env: &HashMap<String, (String, String)>,
) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for &tok in toks {
        let body = tok
            .strip_prefix("--")
            .ok_or_else(|| format!("eval-path: expected --name=value, got {tok:?}"))?;
        let (key, value) = body
            .split_once('=')
            .ok_or_else(|| format!("eval-path: expected --name[:@]=value, got {tok:?}"))?;
        let (name, is_path) = match key.split_once(':') {
            None => (key, false),
            Some((n, "@")) => (n, true),
            Some((_, ty)) => {
                return Err(format!(
                    "eval-path: unknown arg type {ty:?} in {tok:?} \
                     (use --name=value or --name:@=path)"
                ))
            }
        };
        if name.is_empty() || name.contains('/') {
            return Err(format!(
                "eval-path: arg name must be a single path component, got {name:?}"
            ));
        }

        let (mode, oid) = if let Some(var) = value.strip_prefix('$') {
            let (kind, oid) = env
                .get(var)
                .ok_or_else(|| format!("eval-path: undefined variable ${var}"))?;
            (mode_of_kind(kind), parse_oid(oid)?)
        } else if is_path {
            resolve_expr_path(t, input_tree, value)?
        } else {
            (
                EntryKind::Blob.into(),
                post_object(t, "blob", value.as_bytes())?,
            )
        };
        entries.push(Entry {
            mode,
            filename: name.as_bytes().to_vec().into(),
            oid,
        });
    }
    Ok(entries)
}

/// Resolve a `:@=` path value: a `/std/<name>` builtin, or a path within
/// `input_tree` (relative to the `.caos-expr`'s directory).
fn resolve_expr_path(
    t: &dyn Transport,
    input_tree: &str,
    value: &str,
) -> Result<(EntryMode, gix::ObjectId), String> {
    if let Some(name) = value.strip_prefix("/std/") {
        let hash = resolve_std_image(t, name)?;
        return Ok((EntryKind::Tree.into(), parse_oid(&hash)?));
    }
    lookup_in_tree(t, input_tree, value)?
        .ok_or_else(|| format!("eval-path: path {value:?} not found in tree"))
}

/// Look up `rel` (a `/`-separated path) within the tree `tree_oid`, returning
/// the entry's `(mode, oid)` — `None` if any component is missing or a
/// non-final component isn't a directory. An empty `rel` (or `.`) is the tree
/// itself.
fn lookup_in_tree(
    t: &dyn Transport,
    tree_oid: &str,
    rel: &str,
) -> Result<Option<(EntryMode, gix::ObjectId)>, String> {
    let comps: Vec<&str> = rel
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if comps.is_empty() {
        return Ok(Some((EntryKind::Tree.into(), parse_oid(tree_oid)?)));
    }
    let mut current = tree_oid.to_string();
    for (idx, comp) in comps.iter().enumerate() {
        let entries = fetch_tree_entries(t, &current)?
            .ok_or_else(|| format!("{current} is not a tree while resolving {rel}"))?;
        let Some(e) = entries
            .into_iter()
            .find(|e| entry_name(e) == comp.as_bytes())
        else {
            return Ok(None);
        };
        if idx == comps.len() - 1 {
            return Ok(Some((e.mode, e.oid)));
        }
        if !e.mode.is_tree() {
            return Ok(None);
        }
        current = e.oid.to_string();
    }
    unreachable!("loop returns on the last component")
}

/// The result-kind string for a tree-entry mode (as `run`/`eval-path` print it).
fn kind_of_mode(mode: EntryMode) -> &'static str {
    match mode.kind() {
        EntryKind::Tree => "tree",
        EntryKind::Commit => "commit",
        _ => "blob",
    }
}

/// The tree-entry mode for a result kind — the inverse of [`kind_of_mode`], used
/// to place a `$NAME` variable's object into an args tree at its own kind.
fn mode_of_kind(kind: &str) -> EntryMode {
    match kind {
        "tree" => EntryKind::Tree.into(),
        "commit" => EntryKind::Commit.into(),
        _ => EntryKind::Blob.into(),
    }
}
