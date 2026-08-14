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
//! NAME=run   --base:<type>=<image> [--k=v | --k:@=path]
//! NAME=curry --base:<type>=<image> [--k=v | --k:@=path]
//! curry --base:@=path --worker1:@=path    # the value (a run/curry, or a bare $NAME)
//! ```
//!
//! Variable names are `[A-Z][A-Z0-9_]*`; the verbs (`run`, `curry`) are
//! lowercase, so a line is an assignment iff it starts `NAME=run`/`NAME=curry`.
//! A `run` value evaluates to the run's result; a `curry` value to the curried
//! ArgTree. `$NAME` is the object a prior line produced.
//!
//! There is no positional image and no `--`: the worker to run (or curry onto)
//! is the reserved `--base` arg, typed like any other (`:@=` a path, `:docker=`
//! a registry ref, `:hash=` an object in the store, or `=$NAME`). Every other
//! `--k` is an ordinary arg.
//!
//! ## Argument resolution
//!
//! Paths in args are relative to the directory containing the `.caos-expr`
//! (i.e. resolved against the input subtree), NOT the host or `/cas`:
//! - `--k=value` — a literal blob, unless `value` is exactly `$NAME`, which
//!   binds the object that variable holds (by reference, at its own kind).
//! - `--k:@=path` — the object at `path` within the input subtree.
//! - `--k:docker=ref` — the blob `docker://ref`; `--k:hash=oid` — an object the
//!   store already holds, by oid.

use std::collections::HashMap;

use gix::objs::tree::{Entry, EntryKind, EntryMode};

use super::{
    assemble_arg_tree, build_secret_store, curry_from_entries, entry_name, fetch_tree_entries,
    is_hex_hash, mark_arg_tree, parse_oid, post_object, post_tree, request_compute,
    secret_store_header, ClientSecret, Transport, DOCKER_SCHEME,
};

/// Resolve one of the WORKSPACE's declared entry points: evaluate the tracked
/// tree and descend to `DEEP-DEPS/<name>`.
///
/// This is how a client reaches a tool without an ambient library. The workspace
/// declares what it needs in a `DEPS` file (`./std/llm-step llm-step` here,
/// `./flake-inputs/caos/std/llm-step llm-step` in a repo that mounted caos), the
/// root `.caos-expr` expands that into `DEEP-DEPS/`, and this descends it — the
/// same declaration, the same transform and the same mount names a worker sees.
///
/// Evaluating is not optional. A std entry's expression names its own
/// dependencies by mount (`run --base:@=DEEP-DEPS/rustc …`), and those exist only in
/// the DEEPENED tree — so resolving the raw `std/llm-step` directory out of the
/// worktree cannot work, whatever the path is spelled.
pub(crate) fn eval_workspace_dep(t: &dyn Transport, name: &str) -> Result<String, String> {
    let (_, oid) = t
        .ingest_path(".")?
        .ok_or_else(|| "this client cannot ingest the workspace tree".to_string())?;
    // Entry-point resolution feeds `assemble_arg_tree` (which marks the run) or
    // a reader match, so it carries no store of its own — no marking here.
    eval_path(t, &oid.to_string(), &format!("DEEP-DEPS/{name}"), &[])
        .map(|(_kind, hash)| hash)
        .map_err(|e| {
            format!("resolving {name:?} from the workspace: {e}\n  declare it in ./DEPS, e.g. `./std/{name} {name}`")
        })
}

/// Evaluate a tree's own root `.caos-expr` to the object it builds; a tree
/// carrying none evaluates to itself.
///
/// This is the rule [`resolve_expr_base`] applies to a path named *inside* an
/// expression, lifted to the CLI boundary — so a caller reaches a dependency by
/// its deep-deps mount (`run --base:@=DEEP-DEPS/rgrep`) exactly as an expression does,
/// rather than by any name looked up outside the tree it was handed.
pub(crate) fn eval_tree(t: &dyn Transport, tree: &str) -> Result<String, String> {
    // As in [`eval_workspace_dep`]: the result is an image that the caller's own
    // arg-tree assembly marks, so this walk carries no store.
    eval_path(t, tree, "", &[]).map(|(_kind, oid)| oid)
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
    // The caller's secret store, resolved once — so eval-path marks the `curry`
    // arg trees it returns, giving their callers per-user isolation, and carries
    // the store to any `run` it dispatches (design/secrets.md). A `:@=` target is
    // NOT marked; see `resolve_expr_path`.
    let store = build_secret_store(t)?;
    let (kind, hash) = eval_path(t, &start, path, &store)?;
    println!("{kind} {hash}");
    Ok(())
}

/// Walk `start_tree` from its root down to `path`, evaluating every `.caos-expr`
/// encountered (each in the tree its parent produced) and returning the final
/// object's `(kind, oid)`. `node` is the object at the prefix walked so far: a
/// `.caos-expr` at the root of a tree node replaces the node with its result
/// before the next segment is resolved within it.
///
/// **An expression is evaluated against its directory MINUS the `.caos-expr`
/// itself** ([`strip_caos_expr`]) — the directive is not part of the input it
/// describes. See [`resolve_expr_path`] for what that buys.
pub(crate) fn eval_path(
    t: &dyn Transport,
    start_tree: &str,
    path: &str,
    store: &[ClientSecret],
) -> Result<(String, String), String> {
    let comps: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let mut node_kind = String::from("tree");
    let mut node_oid = start_tree.to_string();
    let mut i = 0usize;
    loop {
        // A `.caos-expr` at the root of this tree node transforms the node — from
        // the node's contents WITHOUT the directive (see `strip_caos_expr`).
        if node_kind == "tree" {
            if let Some((mode, oid)) = lookup_in_tree(t, &node_oid, ".caos-expr")? {
                if !mode.is_tree() {
                    let input = strip_caos_expr(t, &node_oid)?;
                    let (k, o) = eval_expr(t, &input, &oid.to_string(), store)?;
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

/// `tree` without its own `.caos-expr` entry — the input an expression is
/// evaluated against.
///
/// **A `.caos-expr` computes a replacement for its directory from the
/// directory's contents excluding the directive itself.** That is the
/// definition, not an optimization, and three things fall out of it:
///
/// - **A self-reference is inert.** `--in:@=.` resolves to the stripped tree,
///   which carries no `.caos-expr`, so evaluating it is the identity. Without
///   this, evaluating a `:@=` target re-runs the very expression doing the
///   evaluating — unbounded recursion, at every nesting level. This is what
///   makes [`resolve_expr_path`] able to evaluate at all.
/// - **Worker-vs-data becomes a real signal.** A `:@=` target *with* a
///   `.caos-expr` is an expression; one *without* evaluates to itself. The
///   distinction is structural, so nothing has to guess.
/// - **The directive stops keying its own input.** Editing a comment in
///   `std/bash/.caos-expr` no longer re-keys the flake build it describes.
///
/// A tree with no `.caos-expr` is returned unchanged, so this costs nothing on
/// the common path.
fn strip_caos_expr(t: &dyn Transport, tree: &str) -> Result<String, String> {
    let entries =
        fetch_tree_entries(t, tree)?.ok_or_else(|| format!("eval-path: {tree} is not a tree"))?;
    let kept: Vec<Entry> = entries
        .into_iter()
        .filter(|e| entry_name(e) != b".caos-expr")
        .collect();
    Ok(post_tree(t, kept)?.to_string())
}

/// Evaluate one `.caos-expr` blob against `input_tree` (the subtree the file
/// sits at the root of, minus the file — see [`strip_caos_expr`]), returning
/// the value's `(kind, oid)`.
fn eval_expr(
    t: &dyn Transport,
    input_tree: &str,
    expr_oid: &str,
    store: &[ClientSecret],
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
            let v = eval_command(t, input_tree, cmd, &env, store)?;
            env.insert(name.to_string(), v);
        } else {
            value = Some(eval_value(t, input_tree, line, &env, store)?);
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
    store: &[ClientSecret],
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
    eval_command(t, input_tree, line, env, store)
}

/// Evaluate a single `run --base:<t>=<image> …` or `curry --base:<t>=<image> …` command against
/// `input_tree`, returning the result's `(kind, oid)`. A `curry` yields the
/// curried ArgTree (a tree); a `run` triggers compute and yields its result.
fn eval_command(
    t: &dyn Transport,
    input_tree: &str,
    cmd: &str,
    env: &HashMap<String, (String, String)>,
    store: &[ClientSecret],
) -> Result<(String, String), String> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let verb = tokens.first().copied().unwrap_or("");
    if verb != "run" && verb != "curry" {
        return Err(format!(
            "eval-path: expected `run` or `curry`, got {verb:?}"
        ));
    }
    // No positional image and no `--`: every token after the verb is a
    // `--name[:type]=value` arg. Exactly one names the reserved `base` — the
    // worker to run (or curry onto); the rest are its args.
    let mut base: Option<(crate::ArgType, &str)> = None;
    let mut arg_toks: Vec<&str> = Vec::new();
    for &tok in &tokens[1..] {
        let (name, ty, value) = crate::parse_arg(tok)?;
        if name == crate::BASE_ARG {
            if base.replace((ty, value)).is_some() {
                return Err(format!("eval-path: `{verb}` given --base twice"));
            }
        } else {
            arg_toks.push(tok);
        }
    }
    let (bty, bval) =
        base.ok_or_else(|| format!("eval-path: `{verb}` needs a --base:<type>=<image> arg"))?;
    let image_ref = resolve_expr_base(t, input_tree, bty, bval, env, store)?;
    let entries = resolve_expr_args(t, input_tree, &arg_toks, env, store)?;

    if verb == "curry" {
        // Mark the returned arg tree, so a caller that embeds it is per-user too
        // (design/secrets.md, caller-propagation).
        let oid = curry_from_entries(t, &image_ref, &[], entries)?;
        let marked = mark_arg_tree(t, store, &oid.to_string())?;
        return Ok(("tree".to_string(), marked));
    }
    // A `run` marks via `assemble_arg_tree` and must carry the store to the
    // server (to inject and pass the double-check), exactly like `caos-cli run`.
    let arg_tree = assemble_arg_tree(t, &image_ref, entries, store)?;
    let server = t.server_url()?;
    request_compute(&server, &arg_tree, &secret_store_header(store))
}

/// Resolve a command's `--base` arg to an image ref string, dispatched on its
/// explicit type (never sniffed from the value's shape): `$VAR` (an object a
/// prior line produced), `:@=<path>` (a path naming an image tree in
/// `input_tree` — a flake dir, a git-docker image, or an evaluable dir, resolved
/// through its own `.caos-expr`), `:docker=<ref>` (a registry image), or
/// `:hash=<oid>` (an object already in the store). A path is the only way to
/// name another tree — there is no by-name lookup, so an expression reaches only
/// what its own tree holds.
fn resolve_expr_base(
    t: &dyn Transport,
    input_tree: &str,
    ty: crate::ArgType,
    value: &str,
    env: &HashMap<String, (String, String)>,
    store: &[ClientSecret],
) -> Result<String, String> {
    // A `$VAR` base names an object a prior assignment produced, whatever its
    // declared type — matched first, exactly as `resolve_expr_args` does.
    if let Some(var) = value.strip_prefix('$') {
        let (_, oid) = env
            .get(var)
            .ok_or_else(|| format!("eval-path: undefined variable ${var}"))?;
        return Ok(oid.clone());
    }
    match ty {
        // `:docker=<ref>` — a registry image, carried as the blob
        // `docker://<ref>` (base_arg_entry stores those bytes). This is how a
        // core item breaks a resolution-time cycle: `flake-builder`'s
        // `.caos-expr` names its own image as the sentinel `--base:docker=seeded`,
        // so evaluating it never re-enters flake-builder's own entry
        // (design/caos-expr.md, "Breaking the cycles"). The formed arg-tree
        // carries the ref as a blob, exactly what the seeder registers.
        crate::ArgType::Docker => Ok(format!("{DOCKER_SCHEME}{value}")),
        // `:hash=<oid>` — an object already in the store (a git image or a curry
        // node), referenced as-is. Location-independent.
        crate::ArgType::Hash => {
            if !is_hex_hash(value) {
                return Err(format!(
                    "eval-path: --base:hash= wants an object hash, got {value:?}"
                ));
            }
            Ok(value.to_string())
        }
        // `:@=<path>` — a path naming an image tree within `input_tree`.
        crate::ArgType::Path => {
            let (mode, oid) = lookup_in_tree(t, input_tree, value)?
                .ok_or_else(|| format!("eval-path: base path {value:?} not found in tree"))?;
            if !mode.is_tree() {
                return Err(format!("eval-path: base path {value:?} is not a directory"));
            }
            // Evaluate the subtree's own `.caos-expr` (if any) to the image it
            // builds — so a path to an EVALUABLE dependency (a `DEEP-DEPS/<name>`
            // mount) resolves to its image, not its raw source; a subtree with no
            // `.caos-expr` (a plain flake dir, a git-docker image) evaluates to
            // itself. For a seeded worker this dispatches its build — which is
            // why forming a `run` expr's key needs that worker already seeded
            // (design/caos-expr.md). Carries the secret store, like any `run`
            // eval-path performs (design/secrets.md).
            let (_kind, hash) = eval_path(t, &oid.to_string(), "", store)?;
            Ok(hash)
        }
        crate::ArgType::Literal => Err(
            "eval-path: --base needs a type: use :@= (path), :docker=, :hash=, or =$VAR"
                .to_string(),
        ),
        crate::ArgType::Commit => Err("eval-path: a commit is not an image base".to_string()),
    }
}

/// Resolve a command's `--name:type=value` args to tree entries, against
/// `input_tree`. Shares the [`crate::parse_arg`] vocabulary: `$VAR`, a literal
/// blob (`=`), a `:@=` path (within the tree, relative to the `.caos-expr`'s
/// directory), a `:docker=` ref, or a `:hash=` object already in the store.
/// (The reserved `base` arg is pulled out by [`eval_command`] before this.)
fn resolve_expr_args(
    t: &dyn Transport,
    input_tree: &str,
    toks: &[&str],
    env: &HashMap<String, (String, String)>,
    store: &[ClientSecret],
) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for &tok in toks {
        let (name, ty, value) = crate::parse_arg(tok)?;
        let (mode, oid) = if let Some(var) = value.strip_prefix('$') {
            // `$VAR` names an object a prior assignment produced, whatever the
            // declared type — matched first.
            let (kind, oid) = env
                .get(var)
                .ok_or_else(|| format!("eval-path: undefined variable ${var}"))?;
            (mode_of_kind(kind), parse_oid(oid)?)
        } else {
            match ty {
                crate::ArgType::Literal => (
                    EntryKind::Blob.into(),
                    post_object(t, "blob", value.as_bytes())?,
                ),
                crate::ArgType::Path => resolve_expr_path(t, input_tree, value, store)?,
                // `:docker=<ref>` — the blob `docker://<ref>`.
                crate::ArgType::Docker => (
                    EntryKind::Blob.into(),
                    post_object(t, "blob", format!("{DOCKER_SCHEME}{value}").as_bytes())?,
                ),
                // `:hash=<oid>` — an object already in the store, by oid (tree
                // or blob).
                crate::ArgType::Hash => {
                    let (kind, _) = t.get_object(value)?;
                    (mode_of_kind(&kind), parse_oid(value)?)
                }
                crate::ArgType::Commit => {
                    return Err(format!(
                        "eval-path: :commit= is not supported in .caos-expr yet: {tok:?}"
                    ))
                }
            }
        };
        entries.push(Entry {
            mode,
            filename: name.as_bytes().to_vec().into(),
            oid,
        });
    }
    Ok(entries)
}

/// Resolve a `:@=` path value: a path within `input_tree`, relative to the
/// `.caos-expr`'s directory. Paths only — a `:@=` value cannot name anything
/// outside the tree being evaluated.
///
/// **A target carrying a `.caos-expr` is an EXPRESSION and is evaluated; one
/// without is DATA and is referenced raw.** The two cases are told apart
/// structurally, not guessed, because [`strip_caos_expr`] has already removed
/// the directive from `input_tree` — so a self-reference like `--in:@=.` lands
/// on a tree with no `.caos-expr` and evaluates to itself. Nothing recurses.
///
/// Evaluating is what closes caller-propagation (design/secrets.md): an
/// embedded worker is no longer referenced raw, so `--pusher:@=github-push`
/// binds whatever eval produced — a `curry` result already marked with
/// `secret-hash`, or a `run` result computed under a marked arg tree. Either
/// way the embedder's own tree turns over per-user, without this function
/// marking anything itself. Marking here would be wrong: an expression may be
/// `run`-valued, and folding a `secret-hash` entry into a *data* tree would
/// corrupt it.
fn resolve_expr_path(
    t: &dyn Transport,
    input_tree: &str,
    value: &str,
    store: &[ClientSecret],
) -> Result<(EntryMode, gix::ObjectId), String> {
    let (mode, oid) = lookup_in_tree(t, input_tree, value)?
        .ok_or_else(|| format!("eval-path: path {value:?} not found in tree"))?;
    if !mode.is_tree() {
        return Ok((mode, oid));
    }
    let tree = oid.to_string();
    match lookup_in_tree(t, &tree, ".caos-expr")? {
        Some((m, _)) if !m.is_tree() => {
            let (kind, hash) = eval_path(t, &tree, "", store)?;
            Ok((mode_of_kind(&kind), parse_oid(&hash)?))
        }
        _ => Ok((mode, oid)),
    }
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
