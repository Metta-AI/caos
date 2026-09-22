//! Repository tool help and argument validation, shared by client and worker.
use serde_json::Value;

/// Arg names a tree tool may not declare: the interpreter binds these itself
/// on every tool run, and `caos curry` errors on a rebind (SPEC, "Currying").
/// `wc`/`refs` are bound only for `@git` tools, but reserved unconditionally
/// so a tool can't declare a model arg the interpreter would then clobber.
///
/// EVERY NAME HERE IS ONE SOMETHING BINDS. `std` used to be on this list and is
/// not any more: there is no `std` arg, a dependency rides inside the tree as a
/// `DEEP-DEPS/<name>` mount, so nothing would ever have collided with a tool
/// declaring one. Reserving a name for its history costs a tool author a
/// perfectly good parameter and tells the next reader that something binds it.
const RESERVED_ARGS: &[&str] = &[
    "in", "worker1", "base", "salt", "wc", "refs",
    // The tool's own ArgTree binds `help` (SPEC, "Tools"), and `caos curry`
    // refuses to rebind — so a tool declaring `@param help` would fail at
    // invocation rather than here, where the model can be told why.
    "help",
];

/// One tree tool as the registry sees it: its name, its description, and the
/// parameters it accepts — parsed from its javadoc `help` (SPEC, "Tools").
pub struct TreeTool {
    pub name: String,
    pub doc: String,
    pub args: Vec<TreeArg>,
    /// The tool declared `@git`: bind the source tree commit (`wc`) and the
    /// turn's ref snapshot (`refs`) so it can walk history. Off by default —
    /// `wc` changes every step, so binding it into a tool that doesn't need it
    /// (build/test) would turn every cache hit into a miss.
    pub git: bool,
}

/// One `@param` tag: `@param <name> <description>` is required, `@param
/// [<name>] <description>` optional. The name becomes the script's `--<name>`
/// arg, readable at `/cas/args/<name>`.
pub struct TreeArg {
    pub name: String,
    pub doc: String,
    pub required: bool,
}

/// Parse one `@param` tag's payload (everything after the tag) into a
/// parameter. `None` — reported by the caller — for a malformed name, so a
/// typo costs a visible skip rather than an arg the model can't use.
pub fn parse_arg(payload: &str) -> Option<TreeArg> {
    let (token, doc) = match payload.split_once(char::is_whitespace) {
        Some((t, d)) => (t, d.trim()),
        None => (payload, ""),
    };
    let (name, required) = match token.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        Some(inner) => (inner, false),
        None => (token, true),
    };
    let ok = !name.is_empty()
        && !RESERVED_ARGS.contains(&name)
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then(|| TreeArg {
        name: name.to_string(),
        doc: doc.to_string(),
        required,
    })
}

/// Parse a tool's `help` string as a JAVADOC comment (SPEC, "Tools"): the free
/// text before the first block tag is the description; `@param <name>` /
/// `@param [<name>]` tags declare the parameters; a bare `@git` tag asks for the
/// history context. Returns `(description, params, git)` — an empty description
/// for the caller to placeholder. A malformed `@param` is skipped with a
/// message.
pub fn parse_help(ctx: &str, text: &str) -> (String, Vec<TreeArg>, bool) {
    let mut doc: Vec<&str> = Vec::new();
    let mut args = Vec::new();
    let mut git = false;
    let mut in_tags = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("@param") {
            in_tags = true;
            match parse_arg(rest.trim()) {
                Some(a) => args.push(a),
                None => eprintln!("{ctx}: unusable @param tag: {line}"),
            }
        } else if trimmed == "@git" {
            in_tags = true;
            git = true;
        } else if !in_tags {
            // Description text — everything before the first block tag.
            doc.push(trimmed);
        }
    }
    (doc.join(" ").trim().to_string(), args, git)
}

/// The `help` string a tool's `.caos-expr` binds, read from the EXPRESSION'S
/// OWN TEXT: a `HELP=<<END … END` here-string whose variable the value line
/// passes as `--help=$HELP`, or a one-line `--help=<literal>`.
///
/// Reading help must not dispatch the target expression's runs or builds.
/// Clients and workers use this parser after evaluating only the ancestors.
///
/// `None` when the expression binds no `help` — a directory that is not a tool,
/// or a tool whose docs went missing; the caller says which and skips it.
pub fn expr_help(expr: &str) -> Option<String> {
    let mut here: Vec<(String, String)> = Vec::new();
    let mut value_lines: Vec<&str> = Vec::new();
    let lines: Vec<&str> = expr.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `NAME=<<TERM` opens a here-string: the body runs to a line equal to
        // TERM, exactly as the evaluator reads it (design/caos-expr.md).
        if let Some((name, term)) = line.split_once("=<<") {
            if !term.is_empty() && !term.contains(char::is_whitespace) {
                let mut body: Vec<&str> = Vec::new();
                while i < lines.len() && lines[i].trim() != term {
                    body.push(lines[i]);
                    i += 1;
                }
                i += 1; // the terminator
                here.push((name.to_string(), body.join("\n")));
                continue;
            }
        }
        value_lines.push(lines[i - 1]);
    }
    // `--help=…` on any command line: a `$VAR` names a here-string above, and
    // anything else is the literal itself.
    for line in value_lines {
        for tok in line.split_whitespace() {
            let Some(value) = tok.strip_prefix("--help=") else {
                continue;
            };
            return match value.strip_prefix('$') {
                Some(var) => here
                    .iter()
                    .find(|(name, _)| name == var)
                    .map(|(_, body)| body.clone()),
                None => Some(value.to_string()),
            };
        }
    }
    None
}

/// Parse the definition without evaluating its expression.
pub fn read_tool(name: &str, expr: &str) -> Result<TreeTool, String> {
    let help = expr_help(expr).ok_or_else(|| format!(
        "{name} has a `.caos-expr` but it binds no `--help`, so it is an evaluable entry rather than a tool"
    ))?;
    let (doc, args, git) = parse_help(name, &help);
    Ok(TreeTool {
        name: name.to_string(),
        doc: if doc.is_empty() {
            format!("Repository tool {name} (no description).")
        } else {
            doc
        },
        args,
        git,
    })
}

/// Validate before evaluating the target expression or dispatching its job.
pub fn bind_args(input: &Value, tool: &TreeTool) -> Result<Vec<(String, String)>, String> {
    let empty = serde_json::Map::new();
    let input = input.as_object().unwrap_or(&empty);
    for key in input.keys() {
        if !tool.args.iter().any(|a| &a.name == key) {
            let known: Vec<&str> = tool.args.iter().map(|a| a.name.as_str()).collect();
            return Err(format!(
                "{} takes no {key:?} argument (declared: {})",
                tool.name,
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
    }
    let mut out = Vec::new();
    for a in &tool.args {
        let value = match input.get(&a.name) {
            None | Some(Value::Null) => {
                if a.required {
                    return Err(format!("{} needs a {:?} argument", tool.name, a.name));
                }
                continue;
            }
            Some(Value::String(s)) => s.clone(),
            Some(v @ (Value::Number(_) | Value::Bool(_))) => v.to_string(),
            Some(_) => {
                return Err(format!("{}'s {:?} must be a string", tool.name, a.name));
            }
        };
        out.push((a.name.clone(), value));
    }
    Ok(out)
}

/// Split a conversation/source-relative tool path. Only its parent is evaluated;
/// the final directory must retain its own expression for help and validation.
pub fn parent_path(path: &str) -> Result<(String, String), String> {
    let parts: Vec<_> = path
        .trim()
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") {
        return Err(format!("invalid tool path: {path:?}"));
    }
    Ok((
        parts[..parts.len() - 1].join("/"),
        parts[parts.len() - 1].to_string(),
    ))
}
