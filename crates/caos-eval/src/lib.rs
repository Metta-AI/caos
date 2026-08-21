//! `.caos-expr`: evaluable trees, factored behind `EvalHost` (design/caos-expr.md).
//!
//! A `.caos-expr` file makes its directory *evaluable*: instead of being taken
//! verbatim, the directory's contents are computed by running the expression the
//! file holds. `eval_path` walks a tree from the root down to a path, and
//! wherever a `.caos-expr` sits it evaluates that expression — with the
//! directory's own subtree as the input — and continues descending into the
//! *result*.
//!
//! ## Two backends, one walk
//!
//! Evaluation dispatches `run`s and BLOCKS on their results. Blocking is fine
//! for a client (top-level, holds no worker slot) and for the server (a request
//! thread resolving an `eval` continuation) — never for a worker. So the walk is
//! generic over `EvalHost`: CAS read/write plus a blocking `EvalHost::dispatch`.
//! The client backend dispatches via `request_compute`; the server backend via
//! `run_image`. They are byte-identical by construction — the same walk, the same
//! curry assembly — which is what lets a worker request eval server-side and get
//! the exact object a client `eval-path` would build.
//!
//! One capability is deliberately client-only: `EvalHost::resolve_remote`
//! (`:@@=`). A locator has to become an oid before the request is formed, or the
//! URL would sit inside the cache key and two consumers pinning the same rev
//! through different URLs would key differently. The default implementation
//! therefore refuses, and only the client overrides it.
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
//! NAME=<<TERM … TERM                        # a here-string: NAME is a literal blob
//! curry --base=$NAME --worker1:@=path       # the value (a run/curry, or a bare $NAME)
//! ```
//!
//! Variable names are `[A-Z][A-Z0-9_]*`; the verbs (`run`, `curry`) are
//! lowercase, so a line is an assignment iff it starts `NAME=run`/`NAME=curry`.
//! A `run` value evaluates to the run's result; a `curry` value to the curried
//! ArgTree. `--base=$NAME` names the object a prior line produced.
//!
//! `$CAOS_EXPR` is pre-bound to the `.caos-expr`'s OWN blob and is reserved: an
//! expression cannot reach its own text by path (the directive is stripped from
//! the tree it is evaluated against), so a worker that must VERIFY the
//! expression which launched it has no other route.
//!
//! A `NAME=<<TERM` line opens a HERE-STRING: the lines up to a line equal to
//! `TERM` are a literal blob (that terminator line excluded, ending in a
//! trailing newline), and `NAME` binds that blob — usable as `--k=$NAME` (a
//! literal blob arg, byte-identical to `--k:@=file`), an error in `--base`
//! position.
//!
//! ## Argument resolution
//!
//! Paths in args are relative to the directory containing the `.caos-expr`
//! (i.e. resolved against the input subtree), NOT the host or `/cas`. Every
//! token parses through [`parse_arg`], the one arg-type vocabulary this crate
//! owns and the client shares.

use std::collections::HashMap;

use gix::objs::tree::{Entry, EntryKind, EntryMode};

pub const DOCKER_SCHEME: &str = "docker://";
pub const CURRY_MARKER: &str = ".caos-curry";
/// The reserved arg naming the worker an ArgTree runs — there is no positional
/// image in any surface, so the base is named like every other argument.
pub const BASE_ARG: &str = "base";
/// The reserved variable naming a `.caos-expr`'s OWN blob (see [`eval_path`]).
pub const EXPR_VAR: &str = "CAOS_EXPR";

/// The **type tag** of a `--name[:type]=value` argument — the operator's
/// explicit choice of how the value is read (never sniffed from the value's
/// shape, so a value may start with anything, no escaping). Bare `=` is a
/// literal; `:@=` a path; `:commit=` a commit; `:hash=` an object by oid;
/// `:docker=` a docker ref; `:@@=` a tree in another repo.
///
/// This is the ONE arg-type vocabulary, shared by the CLI/worker arg builder,
/// the map-then image args, and the `.caos-expr` evaluator
/// (`resolve_expr_args`), so all of them accept exactly the same types and
/// emit the same errors. A resolver may still support only a subset, but they
/// all *parse* through [`parse_arg`]. The grammar is extensible: a new type adds
/// a variant here, a case in [`parse_arg`], and an arm in each resolver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgType {
    /// `--name=value` — the value verbatim, stored as a blob.
    Literal,
    /// `--name:@=path` — the value names a path to resolve/ingest (a host path
    /// on the CLI, a `/cas` path in a worker, a tree path in the evaluator).
    Path,
    /// `--name:commit=value` — the value names a *commit*, passed **unpeeled**
    /// as a gitlink entry: a commit hash, a `/cas` path recorded as a commit
    /// (worker), or a revspec like `HEAD` (CLI). The explicit opt-in exists
    /// because the default forms peel commits to trees (which image refs rely
    /// on); see the client's `resolve_commit_arg`.
    Commit,
    /// `--name:hash=oid` — the value is the hash of an object the *server*
    /// already holds (typically an earlier run's result), referenced directly
    /// by oid with no content round-trip: a **tree** or a **blob**. This is how
    /// results compose into new requests: e.g. a workspace-build job's `bin`
    /// tree feeding a downstream job as `--bins:hash=<oid>`. (Generalizes the
    /// former `:tree=`, which was tree-only.)
    Hash,
    /// `--name:docker=ref` — a docker image ref, stored as the blob
    /// `docker://<ref>` (the representation the server expects). The typed form
    /// is how a docker image is named without sniffing a bare token for a
    /// `docker://` prefix.
    Docker,
    /// `--name:@@=ref` — a tree in ANOTHER repo, named by a nix-style locator
    /// and pinned by a commit sha. The CLIENT fetches it at eval time and the
    /// arg entry is the resulting oid, byte-for-byte as if it had come from a
    /// local `:@=` — so the URL is a fetch coordinate and never enters an
    /// ArgTree or a cache key (design/flake-inputs.md).
    ///
    /// The sibling of [`ArgType::Path`], not a replacement: `:@=` stays a bare
    /// path because that is the common case, and `:@@=` carries the full ref
    /// grammar for the rare one.
    Remote,
}

/// Split a `--name[:type]=value` argument into its name, [`ArgType`] and raw
/// value, validating that the name is a single path component (it becomes a
/// tree-entry filename). Shared by every arg resolver so the type vocabulary and
/// its errors are defined exactly once.
pub fn parse_arg(kv: &str) -> Result<(&str, ArgType, &str), String> {
    let body = kv
        .strip_prefix("--")
        .ok_or_else(|| format!("argument must look like --name=value, got: {kv}"))?;
    let (key, value) = body
        .split_once('=')
        .ok_or_else(|| format!("argument must look like --name[:type]=value, got: {kv}"))?;
    // The key is `name` (literal) or `name:type` (typed); the type sits before `=`.
    let (name, ty) = match key.split_once(':') {
        None => (key, ArgType::Literal),
        Some((name, "@")) => (name, ArgType::Path),
        Some((name, "@@")) => (name, ArgType::Remote),
        Some((name, "commit")) => (name, ArgType::Commit),
        Some((name, "hash")) => (name, ArgType::Hash),
        Some((name, "docker")) => (name, ArgType::Docker),
        Some((_, ty)) => {
            return Err(format!(
                "unknown argument type {ty:?} in {kv:?}; use --name=value (literal), \
                 --name:@=path, --name:@@=<git ref>, --name:commit=rev, --name:hash=oid \
                 (a tree/blob the server holds), or --name:docker=ref"
            ))
        }
    };
    if name.is_empty() || name.contains('/') {
        return Err(format!(
            "argument name must be a single path component, got: {name:?}"
        ));
    }
    Ok((name, ty, value))
}

/// The host a `.caos-expr` walk runs against: CAS read/write plus a BLOCKING run
/// dispatch. A client implements it over its `Transport` + `request_compute`; the
/// server over its git object database + `run_image`. Workers never implement it
/// — they request eval through the `eval` continuation, which the server resolves.
pub trait EvalHost {
    /// `(kind, bytes)` of a stored object (`kind` one of `blob`/`tree`/`commit`).
    fn get_object(&self, oid: &str) -> Result<(String, Vec<u8>), String>;
    /// Store a blob/commit/... and return its oid. In this walk `kind` is always
    /// `"blob"` (literal args, here-strings).
    fn post_object(&self, kind: &str, bytes: &[u8]) -> Result<gix::ObjectId, String>;
    /// The entries of a tree, or `None` if `tree` is not a tree object.
    fn fetch_tree_entries(&self, tree: &str) -> Result<Option<Vec<Entry>>, String>;
    /// Store a tree and return its oid.
    fn post_tree(&self, entries: Vec<Entry>) -> Result<gix::ObjectId, String>;
    /// Assemble the ArgTree from `image` + `entries` and COMPUTE it, blocking,
    /// returning the result's `(kind, hash)`. The client uses `request_compute`,
    /// the server `run_image`; both build a byte-identical ArgTree.
    fn dispatch(&self, image: &str, entries: Vec<Entry>) -> Result<(String, String), String>;
    /// Mark a freshly-built curry ArgTree for this host's secret model
    /// (design/secrets.md). A no-op when no secret is in play, so the common
    /// (tool) path is byte-identical whether or not the host marks.
    fn mark_curry(&self, oid: &str) -> Result<String, String> {
        Ok(oid.to_string())
    }
    /// Resolve a `:@@=` locator to the object it names, descending `dir=`
    /// through evaluation. CLIENT-ONLY: the fetch needs a repo to fetch INTO,
    /// and — the real reason — a locator must become an oid *before* the request
    /// is formed, or its URL would sit in the cache key. Hosts that cannot fetch
    /// keep this default, whose error says where the resolution belongs.
    fn resolve_remote(&self, value: &str) -> Result<(EntryMode, gix::ObjectId), String> {
        Err(format!(
            "cannot resolve {value:?}: a `:@@=` locator is resolved by the CLIENT, \
             so it must already be an oid by the time this host evaluates it"
        ))
    }
    /// A previously computed answer for `key` at this granularity, if the host
    /// keeps one. Every key the walk builds is CONTENT — object hashes and a
    /// path within them — so an entry cannot go stale and there is nothing to
    /// invalidate; what a host adds is its own SCOPE (the client folds in its
    /// secret store, whose readers change what a `curry` evaluates to).
    ///
    /// Not memoizing is always correct, which is why the default is `None`: the
    /// client keeps a process-wide map, and the server — whose salt and stack
    /// vary per request — opts out until it has a key that says so.
    fn memo_get(&self, _kind: MemoKind, _key: &str) -> Option<(String, String)> {
        None
    }
    /// Record `value` as the answer for `key`. Paired with [`EvalHost::memo_get`];
    /// a host that keeps no memo ignores it.
    fn memo_put(&self, _kind: MemoKind, _key: &str, _value: &(String, String)) {}
}

/// Which question a memoized answer answers. The two are *different* questions
/// and both pay, which is why the walk asks at both granularities:
///
/// * [`MemoKind::Node`] — "what does this tree evaluate to", keyed on the tree
///   alone. This is the one that pays. One expression asks many questions that
///   share a PREFIX: a consumer mounting caos writes two locators over one pin
///   (`dir=std` and `dir=std/flake-input-loader`), and both descend through
///   evaluation, so both apply caos' root `.caos-expr` — a deep-deps run over
///   the whole repo, itself preceded by the seeded-sentinel run — and then
///   `std/.caos-expr`. Those are different [`MemoKind::Path`] questions, so only
///   the node transforms can join them.
/// * [`MemoKind::Path`] — "what does this path under this tree evaluate to",
///   for a repeat of the whole question.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemoKind {
    /// Keyed on `<tree>`: one node transform.
    Node,
    /// Keyed on `<start tree>\0<path>`: a whole walk.
    Path,
}

/// Walk `start_tree` from its root down to `path`, evaluating every `.caos-expr`
/// encountered (each in the tree its parent produced) and returning the final
/// object's `(kind, oid)`. `node` is the object at the prefix walked so far: a
/// `.caos-expr` at the root of a tree node replaces the node with its result
/// before the next segment is resolved within it.
///
/// **An expression is evaluated against its directory MINUS the `.caos-expr`
/// itself** (`strip_caos_expr`) — the directive is not part of the input it
/// describes.
///
/// Memoized through the host at both granularities ([`MemoKind`]): here for a
/// repeat of the WHOLE question, and in `eval_node` for each transform along
/// the way, which is where two different paths under one root share their work.
pub fn eval_path(
    host: &dyn EvalHost,
    start_tree: &str,
    path: &str,
) -> Result<(String, String), String> {
    let key = format!("{start_tree}\0{path}");
    if let Some(hit) = host.memo_get(MemoKind::Path, &key) {
        return Ok(hit);
    }
    let result = eval_path_uncached(host, start_tree, path)?;
    host.memo_put(MemoKind::Path, &key, &result);
    Ok(result)
}

/// [`eval_path`]'s body — the actual walk, run once per distinct question.
fn eval_path_uncached(
    host: &dyn EvalHost,
    start_tree: &str,
    path: &str,
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
            if let Some((k, o)) = eval_node(host, &node_oid)? {
                node_kind = k;
                node_oid = o;
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
        let (mode, oid) = lookup_in_tree(host, &node_oid, comps[i])?
            .ok_or_else(|| format!("eval-path: {:?} not found in {node_oid}", comps[i]))?;
        node_kind = kind_of_mode(mode).to_string();
        node_oid = oid.to_string();
        i += 1;
    }
    Ok((node_kind, node_oid))
}

/// What the tree `tree` evaluates to: the value of the `.caos-expr` at its root,
/// or `None` when it carries none (a tree without a directive is its own value).
///
/// **This is the memo that pays**, and the granularity is the reason — see
/// [`MemoKind::Node`] for the measurement. Keyed on the tree alone because a
/// transform is a function of its input and nothing else: a `.caos-expr` reaches
/// only what its own tree holds, by construction.
///
/// The lookup of the directive is deliberately OUTSIDE the memo: it is one
/// cheap tree read, and doing it first means a tree with no `.caos-expr` — the
/// common case, every ordinary directory the walk descends — never touches the
/// map at all.
fn eval_node(host: &dyn EvalHost, tree: &str) -> Result<Option<(String, String)>, String> {
    let Some((mode, expr)) = lookup_in_tree(host, tree, ".caos-expr")? else {
        return Ok(None);
    };
    if mode.is_tree() {
        return Ok(None);
    }
    if let Some(hit) = host.memo_get(MemoKind::Node, tree) {
        return Ok(Some(hit));
    }
    let input = strip_caos_expr(host, tree)?;
    let result = eval_expr(host, &input, &expr.to_string())?;
    host.memo_put(MemoKind::Node, tree, &result);
    Ok(Some(result))
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
///   evaluating — unbounded recursion, at every nesting level.
/// - **Worker-vs-data becomes a real signal.** A `:@=` target *with* a
///   `.caos-expr` is an expression; one *without* evaluates to itself.
/// - **The directive stops keying its own input.** Editing a comment in
///   `std/bash/.caos-expr` no longer re-keys the flake build it describes.
///
/// A tree with no `.caos-expr` is returned unchanged, so this costs nothing on
/// the common path.
fn strip_caos_expr(host: &dyn EvalHost, tree: &str) -> Result<String, String> {
    let entries = host
        .fetch_tree_entries(tree)?
        .ok_or_else(|| format!("eval-path: {tree} is not a tree"))?;
    let kept: Vec<Entry> = entries
        .into_iter()
        .filter(|e| entry_name(e) != b".caos-expr")
        .collect();
    Ok(host.post_tree(kept)?.to_string())
}

/// Evaluate one `.caos-expr` blob against `input_tree` (the subtree the file
/// sits at the root of, minus the file — see [`strip_caos_expr`]), returning
/// the value's `(kind, oid)`.
fn eval_expr(
    host: &dyn EvalHost,
    input_tree: &str,
    expr_oid: &str,
) -> Result<(String, String), String> {
    let (kind, content) = host.get_object(expr_oid)?;
    if kind != "blob" {
        return Err(format!(".caos-expr {expr_oid} is a {kind}, not a blob"));
    }
    let text =
        String::from_utf8(content).map_err(|e| format!(".caos-expr {expr_oid} not UTF-8: {e}"))?;

    let mut env: HashMap<String, (String, String)> = HashMap::new();
    // `$CAOS_EXPR` — THIS `.caos-expr`'s own blob, pre-bound so it reads like any
    // other variable. It exists because [`strip_caos_expr`] removes the
    // directive from the tree the expression is evaluated against, so an
    // expression cannot reach its own text by path: `--in:@=.` is deliberately
    // inert. A worker that must VERIFY the expression that launched it — the
    // consumer-input expander checking that its locators agree with
    // `flake.lock` (design/flake-inputs.md) — therefore has no other way to see
    // it.
    //
    // Hermetic, unlike passing `:@@=path:.caos-expr`: this blob comes from the
    // tree being evaluated, so it is still the right file under
    // `eval-path --tree=<oid>` and when a THIRD repo pins this one by locator,
    // where a host path would read whatever the working directory happened to
    // hold.
    //
    // The cost is opt-in and worth stating: an expression that passes `$CAOS_EXPR`
    // puts its own bytes in the arg tree, so editing a COMMENT in it re-keys
    // that run — exactly what stripping otherwise buys. That is correct for a
    // caller whose worker inspects the text: then the text is an input.
    env.insert(
        EXPR_VAR.to_string(),
        ("blob".to_string(), expr_oid.to_string()),
    );
    let mut value: Option<(String, String)> = None;
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        i += 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if value.is_some() {
            return Err("eval-path: .caos-expr has content after its final expression".to_string());
        }
        if let Some((name, term)) = parse_heredoc(line) {
            // A here-string binds a BLOB-valued variable: the lines up to a line
            // equal to TERM are its content (that terminator line excluded),
            // ending in a trailing newline — byte-identical to a `:@=` file of
            // the same text. Its bytes never ride the command tokenizer (only
            // the `$NAME` that references it does), so it may hold the spaces and
            // newlines a `--k=value` literal cannot.
            reserved_check(name)?;
            let mut body: Vec<&str> = Vec::new();
            let mut closed = false;
            while i < lines.len() {
                let bl = lines[i];
                i += 1;
                if bl.trim() == term {
                    closed = true;
                    break;
                }
                body.push(bl);
            }
            if !closed {
                return Err(format!(
                    "eval-path: here-string ${name} is not closed by a {term:?} line"
                ));
            }
            let content = if body.is_empty() {
                String::new()
            } else {
                format!("{}\n", body.join("\n"))
            };
            let oid = host.post_object("blob", content.as_bytes())?;
            env.insert(name.to_string(), ("blob".to_string(), oid.to_string()));
        } else if let Some((name, cmd)) = parse_assignment(line) {
            reserved_check(name)?;
            let v = eval_command(host, input_tree, cmd, &env)?;
            env.insert(name.to_string(), v);
        } else {
            value = Some(eval_value(host, input_tree, line, &env)?);
        }
    }
    value.ok_or_else(|| "eval-path: .caos-expr has no final expression".to_string())
}

/// `$CAOS_EXPR` is reserved, like `base` and `unbind` on the verbs: a binding
/// that shadowed it would silently change what a verifier sees.
fn reserved_check(name: &str) -> Result<(), String> {
    if name == EXPR_VAR {
        return Err(format!(
            "eval-path: ${EXPR_VAR} is reserved (this .caos-expr's own blob) \
             and cannot be assigned"
        ));
    }
    Ok(())
}

/// A variable name is `[A-Z][A-Z0-9_]*` — uppercase so it never blurs with the
/// lowercase verbs (`run`, `curry`) or a here-string terminator.
fn valid_var_name(name: &str) -> bool {
    matches!(name.chars().next(), Some(c) if c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// An assignment line `NAME=<run|curry …>` → `(NAME, "<run|curry …>")`; any
/// other line (a bare command or `$NAME`) → `None`.
fn parse_assignment(line: &str) -> Option<(&str, &str)> {
    let eq = line.find('=')?;
    let (name, rest) = (&line[..eq], &line[eq + 1..]);
    if !valid_var_name(name) {
        return None;
    }
    match rest.split_whitespace().next() {
        Some("run") | Some("curry") => Some((name, rest)),
        _ => None,
    }
}

/// A here-string opener `NAME=<<TERM` → `(NAME, TERM)`; any other line → `None`.
/// Binds a BLOB, not an object.
fn parse_heredoc(line: &str) -> Option<(&str, &str)> {
    let eq = line.find('=')?;
    let (name, rest) = (&line[..eq], &line[eq + 1..]);
    if !valid_var_name(name) {
        return None;
    }
    let term = rest.strip_prefix("<<")?;
    (!term.is_empty() && !term.chars().any(char::is_whitespace)).then_some((name, term))
}

/// Evaluate the file's value line: a `run`/`curry` command, or a bare `$NAME`
/// that names a previously bound variable.
fn eval_value(
    host: &dyn EvalHost,
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
    eval_command(host, input_tree, line, env)
}

/// Assemble `args` as one `curry` command against `input_tree`, without
/// dispatching the resulting request. Callers that already supply the
/// operation (such as a `.caos-secrets` `reader=` field) use this entry point
/// so they share the expression grammar without accepting a verb of their own.
pub fn assemble_curry(host: &dyn EvalHost, input_tree: &str, args: &str) -> Result<String, String> {
    let command = format!("curry {args}");
    let (kind, oid) = eval_command(host, input_tree, &command, &HashMap::new())?;
    if kind != "tree" {
        return Err(format!(
            "curry assembly returned a {kind}, expected a partial ArgTree"
        ));
    }
    Ok(oid)
}

/// Evaluate a single `run --base:<t>=<image> …` or `curry --base:<t>=<image> …`
/// command against `input_tree`, returning the result's `(kind, oid)`. A `curry`
/// yields the curried ArgTree (a tree); a `run` triggers compute and yields its
/// result.
fn eval_command(
    host: &dyn EvalHost,
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
    // No positional image and no `--`: every token after the verb is a
    // `--name[:type]=value` arg. Exactly one names the reserved `base` — the
    // worker to run (or curry onto); the rest are its args.
    let mut base: Option<(ArgType, &str)> = None;
    let mut arg_toks: Vec<&str> = Vec::new();
    for &tok in &tokens[1..] {
        let (name, ty, value) = parse_arg(tok)?;
        if name == BASE_ARG {
            if base.replace((ty, value)).is_some() {
                return Err(format!("eval-path: `{verb}` given --base twice"));
            }
        } else {
            arg_toks.push(tok);
        }
    }
    let (bty, bval) =
        base.ok_or_else(|| format!("eval-path: `{verb}` needs a --base:<type>=<image> arg"))?;
    let image_ref = resolve_expr_base(host, input_tree, bty, bval, env)?;
    let entries = resolve_expr_args(host, input_tree, &arg_toks, env)?;

    if verb == "curry" {
        // Mark the returned arg tree, so a caller that embeds it is per-user too
        // (design/secrets.md, caller-propagation) — a no-op without secrets.
        let oid = build_curry(host, &image_ref, entries)?;
        let marked = host.mark_curry(&oid.to_string())?;
        return Ok(("tree".to_string(), marked));
    }
    host.dispatch(&image_ref, entries)
}

/// Resolve a command's `--base` arg to an image ref string, dispatched on its
/// explicit type (never sniffed from the value's shape): `$VAR` (an object a
/// prior line produced), `:@=<path>` (a path naming an image tree in
/// `input_tree` — a flake dir, a git-docker image, or an evaluable dir, resolved
/// through its own `.caos-expr`), `:@@=<ref>` (a tree in another repo),
/// `:docker=<ref>` (a registry image), or `:hash=<oid>` (an object already in
/// the store). A path is the only way to name another tree in THIS repo — there
/// is no by-name lookup, so an expression reaches only what its own tree holds.
fn resolve_expr_base(
    host: &dyn EvalHost,
    input_tree: &str,
    ty: ArgType,
    value: &str,
    env: &HashMap<String, (String, String)>,
) -> Result<String, String> {
    // A `$VAR` base names an object a prior assignment produced, whatever its
    // declared type — matched first, exactly as `resolve_expr_args` does.
    if let Some(var) = value.strip_prefix('$') {
        let (kind, oid) = env
            .get(var)
            .ok_or_else(|| format!("eval-path: undefined variable ${var}"))?;
        // A here-string is bytes, not a worker: naming one as the base would
        // form an ArgTree whose image is a text blob.
        if kind == "blob" {
            return Err(format!(
                "eval-path: ${var} is a here-string (a blob), not an image"
            ));
        }
        return Ok(oid.clone());
    }
    match ty {
        // `:docker=<ref>` — a registry image, carried as the blob
        // `docker://<ref>`. This is how a core item breaks a resolution-time
        // cycle: `flake-builder`'s `.caos-expr` names its own image as the
        // sentinel `--base:docker=seeded`, so evaluating it never re-enters
        // flake-builder's own entry (design/caos-expr.md, "Breaking the
        // cycles"). The formed arg-tree carries the ref as a blob, exactly what
        // the seeder registers.
        ArgType::Docker => Ok(format!("{DOCKER_SCHEME}{value}")),
        // `:hash=<oid>` — an object already in the store (a git image or a curry
        // node), referenced as-is. Location-independent.
        ArgType::Hash => {
            if !is_hex_hash(value) {
                return Err(format!(
                    "eval-path: --base:hash= wants an object hash, got {value:?}"
                ));
            }
            Ok(value.to_string())
        }
        // `:@=<path>` — a path naming an image tree within `input_tree`.
        ArgType::Path => {
            let (mode, oid) = lookup_in_tree(host, input_tree, value)?
                .ok_or_else(|| format!("eval-path: base path {value:?} not found in tree"))?;
            if !mode.is_tree() {
                return Err(format!("eval-path: base path {value:?} is not a directory"));
            }
            // Evaluate the subtree's own `.caos-expr` (if any) to the image it
            // builds — so a path to an EVALUABLE dependency (a `DEEP-DEPS/<name>`
            // mount) resolves to its image, not its raw source; a subtree with no
            // `.caos-expr` evaluates to itself. For a seeded worker this
            // dispatches its build — which is why forming a `run` expr's key
            // needs that worker already seeded (design/caos-expr.md).
            let (_kind, hash) = eval_path(host, &oid.to_string(), "")?;
            Ok(hash)
        }
        // `:@@=<ref>` — the base lives in ANOTHER repo. Fetched by the CLIENT
        // (never a worker, and not the server) and resolved by DESCENT THROUGH
        // EVALUATION, so a pinned consumer reaches `dir=std/<x>` exactly as caos
        // reaches its own entries (design/flake-inputs.md).
        ArgType::Remote => {
            let (mode, oid) = host.resolve_remote(value)?;
            if !mode.is_tree() {
                return Err(format!(
                    "eval-path: git ref {value:?} names a file, not an image tree"
                ));
            }
            Ok(oid.to_string())
        }
        ArgType::Literal => Err(
            "eval-path: --base needs a type: use :@= (path), :@@= (a git ref), :docker=, \
             :hash=, or =$VAR"
                .to_string(),
        ),
        ArgType::Commit => Err("eval-path: a commit is not an image base".to_string()),
    }
}

/// Resolve a command's `--name:type=value` args to tree entries, against
/// `input_tree`. Shares the [`parse_arg`] vocabulary: `$VAR`, a literal blob
/// (`=`), a `:@=` path (within the tree, relative to the `.caos-expr`'s
/// directory), a `:@@=` locator, a `:docker=` ref, or a `:hash=` object already
/// in the store. (The reserved `base` arg is pulled out by [`eval_command`]
/// before this.)
fn resolve_expr_args(
    host: &dyn EvalHost,
    input_tree: &str,
    toks: &[&str],
    env: &HashMap<String, (String, String)>,
) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for &tok in toks {
        let (name, ty, value) = parse_arg(tok)?;
        let (mode, oid) = if let Some(var) = value.strip_prefix('$') {
            // `$VAR` names an object a prior assignment produced (or a
            // here-string's blob), whatever the declared type — matched first.
            let (kind, oid) = env
                .get(var)
                .ok_or_else(|| format!("eval-path: undefined variable ${var}"))?;
            (mode_of_kind(kind), parse_oid(oid)?)
        } else {
            match ty {
                ArgType::Literal => (
                    EntryKind::Blob.into(),
                    host.post_object("blob", value.as_bytes())?,
                ),
                ArgType::Path => resolve_expr_path(host, input_tree, value)?,
                // `:@@=<ref>` — a tree from ANOTHER repo. The client's resolver
                // descends `dir=` through evaluation, so this is already the
                // same rule `:@=` applies (an expression is evaluated, data is
                // referenced raw) — just to a tree that arrived from elsewhere.
                ArgType::Remote => host.resolve_remote(value)?,
                // `:docker=<ref>` — the blob `docker://<ref>`.
                ArgType::Docker => (
                    EntryKind::Blob.into(),
                    host.post_object("blob", format!("{DOCKER_SCHEME}{value}").as_bytes())?,
                ),
                // `:hash=<oid>` — an object already in the store, by oid (tree
                // or blob).
                ArgType::Hash => {
                    let (kind, _) = host.get_object(value)?;
                    (mode_of_kind(&kind), parse_oid(value)?)
                }
                ArgType::Commit => {
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
/// embedded worker is no longer referenced raw, so the embedder binds whatever
/// eval produced — a `curry` result already marked, or a `run` result computed
/// under a marked arg tree. Marking *here* would be wrong: an expression may be
/// `run`-valued, and folding a `secret-hash` entry into a *data* tree would
/// corrupt it.
fn resolve_expr_path(
    host: &dyn EvalHost,
    input_tree: &str,
    value: &str,
) -> Result<(EntryMode, gix::ObjectId), String> {
    let (mode, oid) = lookup_in_tree(host, input_tree, value)?
        .ok_or_else(|| format!("eval-path: path {value:?} not found in tree"))?;
    eval_if_evaluable(host, mode, oid)
}

/// The worker-vs-data rule, applied to an object an arg resolved to: a TREE
/// carrying a `.caos-expr` is an expression and is evaluated; anything else — a
/// blob, or a tree without one — is data and is referenced raw.
///
/// Shared by `:@=` (`resolve_expr_path`) and the client's `:@@=` resolver, so
/// where the tree CAME FROM makes no difference to what naming it means.
pub fn eval_if_evaluable(
    host: &dyn EvalHost,
    mode: EntryMode,
    oid: gix::ObjectId,
) -> Result<(EntryMode, gix::ObjectId), String> {
    if !mode.is_tree() {
        return Ok((mode, oid));
    }
    let tree = oid.to_string();
    match lookup_in_tree(host, &tree, ".caos-expr")? {
        Some((m, _)) if !m.is_tree() => {
            let (kind, hash) = eval_path(host, &tree, "")?;
            Ok((mode_of_kind(&kind), parse_oid(&hash)?))
        }
        _ => Ok((mode, oid)),
    }
}

/// Look up `rel` (a `/`-separated path) within the tree `tree_oid`, returning
/// the entry's `(mode, oid)` — `None` if any component is missing or a non-final
/// component isn't a directory. An empty `rel` (or `.`) is the tree itself.
pub fn lookup_in_tree(
    host: &dyn EvalHost,
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
        let entries = host
            .fetch_tree_entries(&current)?
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

// ---- Curry assembly (byte-identical to crates/caos's `curry_from_entries`) ---

/// Build a curry ArgTree `{base, args, .caos-curry}` from a resolved `image_ref`
/// and `entries`, peeling any curry/args layers off `image_ref` first so a
/// `curry --base=$VAR …` over a prior curry flattens into one layer. This mirrors
/// the client's `curry_from_entries` (unbind-free) exactly, so an expression
/// curries to the same object whichever host walks it.
fn build_curry(
    host: &dyn EvalHost,
    image_ref: &str,
    new: Vec<Entry>,
) -> Result<gix::ObjectId, String> {
    let (base, bound) = unwrap_curry(host, image_ref)?;

    // Keep expression curries as strict as ordinary client curries. The merge
    // helper is deliberately last-wins for overlays, but repeating one name on
    // a curry command line is a malformed binding, not an override.
    let mut new_names = std::collections::BTreeSet::new();
    for entry in &new {
        if !new_names.insert(entry.filename.to_vec()) {
            return Err(format!(
                "curry: arg {:?} was provided more than once",
                String::from_utf8_lossy(&entry.filename)
            ));
        }
    }

    for e in &new {
        if bound.iter().any(|b| b.filename == e.filename) {
            return Err(format!(
                "curry: arg {:?} is already bound in {image_ref}; rename one of them",
                String::from_utf8_lossy(&e.filename)
            ));
        }
    }
    let args = merge_entries(bound, new);
    let args_tree = host.post_tree(args)?;
    let entries = vec![
        Entry {
            mode: EntryKind::Blob.into(),
            filename: BASE_ARG.as_bytes().to_vec().into(),
            oid: host.post_object("blob", base.as_bytes())?,
        },
        Entry {
            mode: EntryKind::Tree.into(),
            filename: b"args".to_vec().into(),
            oid: args_tree,
        },
        Entry {
            mode: EntryKind::Blob.into(),
            filename: CURRY_MARKER.as_bytes().to_vec().into(),
            oid: host.post_object("blob", b"1")?,
        },
    ];
    host.post_tree(entries)
}

/// Peel curry/args layers off `image` (a resolved ref), returning the underlying
/// plain image and the args bound into it (outer layers win).
fn unwrap_curry(host: &dyn EvalHost, image: &str) -> Result<(String, Vec<Entry>), String> {
    let mut image = image.to_string();
    let mut bound: Vec<Entry> = Vec::new();
    while is_hex_hash(&image) {
        if let Some((inner_image, inner_args)) = curry_node(host, &image)? {
            bound = merge_entries(inner_args, bound);
            image = inner_image;
            continue;
        }
        if let Some((inner_image, inner_args)) = args_tree_node(host, &image)? {
            bound = merge_entries(inner_args, bound);
            image = inner_image;
            continue;
        }
        break;
    }
    Ok((image, bound))
}

/// If `hash` is a curry node, its base image ref and bound args; else `None`.
fn curry_node(host: &dyn EvalHost, hash: &str) -> Result<Option<(String, Vec<Entry>)>, String> {
    let Some(entries) = host.fetch_tree_entries(hash)? else {
        return Ok(None);
    };
    if !entries
        .iter()
        .any(|e| entry_name(e) == CURRY_MARKER.as_bytes())
    {
        return Ok(None);
    }
    let oid_of = |name: &[u8]| {
        entries
            .iter()
            .find(|e| entry_name(e) == name)
            .map(|e| e.oid)
            .ok_or_else(|| {
                format!(
                    "curry node {hash} missing {:?}",
                    String::from_utf8_lossy(name)
                )
            })
    };
    let base_ref = fetch_blob_string(host, &oid_of(BASE_ARG.as_bytes())?.to_string())?;
    let args = host
        .fetch_tree_entries(&oid_of(b"args")?.to_string())?
        .ok_or_else(|| format!("curry node {hash} 'args' is not a tree"))?;
    Ok(Some((base_ref, args)))
}

/// If `hash` is a flat **args tree** (a tree with the reserved `base` entry but
/// no `CURRY_MARKER`), its base image ref and remaining args; else `None`.
///
/// `base` ALONE DOES NOT SAY "args tree": a git-docker image tree carries its
/// own `base` — the `docker://` ref its `layer<NN>`s are a delta over. Reading
/// one as an args tree peels it into its own base and scatters
/// `config.json`/`layer<NN>` into the caller's args. `config.json` is the
/// discriminator: the converter requires it on every image tree, and it can
/// never be an arg name (arg names are `[a-z][a-z0-9-]*` — no dot).
fn args_tree_node(host: &dyn EvalHost, hash: &str) -> Result<Option<(String, Vec<Entry>)>, String> {
    let Some(entries) = host.fetch_tree_entries(hash)? else {
        return Ok(None);
    };
    if entries
        .iter()
        .any(|e| entry_name(e) == CURRY_MARKER.as_bytes())
    {
        return Ok(None); // a curry node — handled by `curry_node`
    }
    if entries.iter().any(|e| entry_name(e) == b"config.json") {
        return Ok(None); // a git-docker image tree, whose `base` is its own
    }
    let Some(image) = entries
        .iter()
        .find(|e| entry_name(e) == BASE_ARG.as_bytes())
    else {
        return Ok(None); // no reserved `base` entry — not an args tree
    };
    // A git image rides embedded (the entry IS its tree, so the ref is the oid);
    // a `docker://` ref rides as a blob naming the registry ref.
    let base_ref = if image.mode.is_tree() {
        image.oid.to_string()
    } else {
        fetch_blob_string(host, &image.oid.to_string())?
    };
    let bound = entries
        .into_iter()
        .filter(|e| entry_name(e) != BASE_ARG.as_bytes())
        .collect();
    Ok(Some((base_ref, bound)))
}

fn fetch_blob_string(host: &dyn EvalHost, oid: &str) -> Result<String, String> {
    let (kind, bytes) = host.get_object(oid)?;
    if kind != "blob" {
        return Err(format!("expected a blob at {oid}, got {kind}"));
    }
    String::from_utf8(bytes).map_err(|e| format!("blob {oid} is not UTF-8: {e}"))
}

/// Merge two entry sets by filename; entries in `high` override those in `low`.
fn merge_entries(low: Vec<Entry>, high: Vec<Entry>) -> Vec<Entry> {
    let mut by_name = std::collections::BTreeMap::new();
    for e in low.into_iter().chain(high) {
        by_name.insert(e.filename.to_vec(), e);
    }
    by_name.into_values().collect()
}

// ---- small helpers ----------------------------------------------------------

fn entry_name(e: &Entry) -> &[u8] {
    e.filename.as_ref()
}

fn parse_oid(hex: &str) -> Result<gix::ObjectId, String> {
    gix::ObjectId::from_hex(hex.as_bytes()).map_err(|e| format!("bad object id {hex:?}: {e}"))
}

fn is_hex_hash(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The result-kind string for a tree-entry mode.
pub fn kind_of_mode(mode: EntryMode) -> &'static str {
    match mode.kind() {
        EntryKind::Tree => "tree",
        EntryKind::Commit => "commit",
        _ => "blob",
    }
}

/// The tree-entry mode for a result kind — the inverse of [`kind_of_mode`], used
/// to place a `$NAME` variable's object into an args tree at its own kind.
pub fn mode_of_kind(kind: &str) -> EntryMode {
    match kind {
        "tree" => EntryKind::Tree.into(),
        "commit" => EntryKind::Commit.into(),
        _ => EntryKind::Blob.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_arg, parse_assignment, parse_heredoc, valid_var_name, ArgType};

    #[test]
    fn var_names() {
        assert!(valid_var_name("NAME"));
        assert!(valid_var_name("A1_B"));
        assert!(!valid_var_name("name"));
        assert!(!valid_var_name("1A"));
        assert!(!valid_var_name(""));
        assert!(!valid_var_name("A-B"));
    }

    #[test]
    fn assignment_vs_heredoc_vs_value() {
        assert_eq!(
            parse_assignment("W=run --base:@=img --x=y"),
            Some(("W", "run --base:@=img --x=y"))
        );
        assert_eq!(
            parse_assignment("C=curry --base:@=img"),
            Some(("C", "curry --base:@=img"))
        );
        assert_eq!(parse_assignment("H=<<END"), None);
        assert_eq!(parse_assignment("curry --base=$W --help=$H"), None);

        assert_eq!(parse_heredoc("H=<<END"), Some(("H", "END")));
        assert_eq!(parse_heredoc("W=run --base:@=img"), None);
        assert_eq!(parse_heredoc("H=<<"), None);
        assert_eq!(parse_heredoc("H=<<A B"), None);
        assert_eq!(parse_heredoc("curry --base=$W --help=$H"), None);
    }

    #[test]
    fn arg_types() {
        assert_eq!(
            parse_arg("--base:@=std/bash").unwrap(),
            ("base", ArgType::Path, "std/bash")
        );
        assert_eq!(
            parse_arg("--in:@@=git+https://x?rev=abc").unwrap(),
            ("in", ArgType::Remote, "git+https://x?rev=abc")
        );
        assert_eq!(
            parse_arg("--help=$HELP").unwrap(),
            ("help", ArgType::Literal, "$HELP")
        );
        assert!(parse_arg("--k:nope=v").is_err());
        assert!(parse_arg("--a/b=v").is_err());
        assert!(parse_arg("no-dashes=v").is_err());
    }
}
