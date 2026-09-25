//! Deterministic replay of endpoint tree differences. Every call runs the plan
//! from its beginning; the input plan is never rewritten with execution state.
use conversation_protocol::v3::{paths, CommitInfo, Oid, Signature};

pub trait Backend {
    fn read_commit(&mut self, oid: &Oid) -> Result<CommitInfo, String>;
    fn read_message(&mut self, path: &str) -> Result<Vec<u8>, String>;
    fn merge_trees(&mut self, base: &Oid, ours: &Oid, theirs: &Oid) -> Result<Merge, String>;
    fn commit_tree(&mut self, commit: &CommitInfo) -> Result<Oid, String>;
}

#[derive(Debug)]
pub struct Merge {
    pub tree: Oid,
    pub conflicts: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Layer {
    pub name: String,
    pub tip: Oid,
    pub base: Oid,
}

#[derive(Debug)]
pub struct Conflict {
    pub line: usize,
    pub parent: Oid,
    pub draft: Oid,
    pub report: Vec<u8>,
}

#[derive(Debug)]
pub struct Outcome {
    pub layers: Vec<Layer>,
    pub conflict: Option<Conflict>,
}

#[derive(Debug)]
enum Selection {
    Single(Oid),
    Range(Oid, Oid),
}

#[derive(Debug)]
enum Instruction {
    Pick(Selection),
    Message(String),
    Branch(String),
}

pub struct Plan {
    onto: Oid,
    committer: Signature,
    instructions: Vec<(usize, Instruction)>,
}

impl Plan {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().enumerate().filter_map(|(index, line)| {
            let line = line.trim();
            (!line.is_empty() && !line.starts_with('#')).then_some((index + 1, line))
        });
        let (_, first) = lines.next().ok_or("replay plan is empty")?;
        let onto = Oid::parse(
            first
                .strip_prefix("onto=")
                .ok_or("first instruction must be onto=<commit>")?
                .trim(),
            "onto",
        )?;
        let (_, second) = lines.next().ok_or("plan needs an explicit committer")?;
        let committer = crate::objects::parse_signature(
            second
                .strip_prefix("committer=")
                .ok_or(
                    "second instruction must be committer=Name <email> <unix-seconds> <+/-HHMM>",
                )?
                .trim(),
        )?;
        let mut instructions = Vec::new();
        let mut unsealed = false;
        let mut branches = 0;
        for (line, text) in lines {
            let (command, value) = text
                .split_once('=')
                .ok_or_else(|| format!("line {line}: expected command=value"))?;
            let value = value.trim();
            let instruction = match command {
                "pick" => {
                    let selection = match value.split_once("..") {
                        Some((from, to)) => Selection::Range(
                            Oid::parse(from, "range start")?,
                            Oid::parse(to, "range end")?,
                        ),
                        None => Selection::Single(Oid::parse(value, "picked commit")?),
                    };
                    unsealed = true;
                    Instruction::Pick(selection)
                }
                "message" => {
                    if !unsealed {
                        return Err(format!(
                            "line {line}: message requires a pick since the last branch"
                        ));
                    }
                    paths::validate_source_tree_name(value)?;
                    Instruction::Message(value.into())
                }
                "branch" => {
                    paths::validate_component(value)?;
                    if value.chars().any(char::is_whitespace) {
                        return Err(format!(
                            "line {line}: branch name cannot contain whitespace"
                        ));
                    }
                    // Numbers are generated, so accepting a supplied prefix would
                    // silently produce misleading names such as 00-01-feature.
                    if value.split_once('-').is_some_and(|(prefix, _)| {
                        !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit())
                    }) {
                        return Err(format!(
                            "line {line}: branch takes a name without a number prefix"
                        ));
                    }
                    branches += 1;
                    unsealed = false;
                    Instruction::Branch(value.into())
                }
                _ => return Err(format!("line {line}: unknown replay command {command}")),
            };
            instructions.push((line, instruction));
        }
        if branches == 0 || unsealed {
            return Err("the final output tip must be recorded by branch=<name>".into());
        }
        Ok(Self {
            onto,
            committer,
            instructions,
        })
    }
}

pub fn run(backend: &mut impl Backend, source: &str) -> Result<Outcome, String> {
    let plan = Plan::parse(source)?;
    let mut tip = plan.onto;
    let mut current = backend.read_commit(&tip)?;
    let mut layer_base = tip.clone();
    let mut layers = Vec::new();
    for (line, instruction) in plan.instructions {
        match instruction {
            Instruction::Branch(name) => {
                layers.push(Layer {
                    name: format!("{:02}-{name}", layers.len()),
                    tip: tip.clone(),
                    base: layer_base,
                });
                layer_base = tip.clone();
            }
            Instruction::Message(path) => {
                // Read at execution, not while parsing: a later missing message
                // must not prevent an earlier conflict from being returned.
                let message = backend.read_message(&path)?;
                if current.message != message {
                    current.message = message;
                    current.committer = plan.committer.clone();
                    current.extra_headers = rewritten_headers(&current.extra_headers, true);
                    tip = backend.commit_tree(&current)?;
                }
            }
            Instruction::Pick(selection) => {
                let (from, to, single) = match selection {
                    Selection::Single(to) => {
                        let selected = backend.read_commit(&to)?;
                        let [parent] = selected.parents.as_slice() else {
                            return Err(format!(
                                "line {line}: single-commit pick requires exactly one parent: {to}"
                            ));
                        };
                        (parent.clone(), to, true)
                    }
                    Selection::Range(from, to) => (from, to, false),
                };
                let selected = backend.read_commit(&to)?;
                if single && from == tip {
                    // Reusing the object preserves all source metadata, including
                    // committer and valid signatures. The plan committer applies
                    // only to commits that actually have to be created.
                    tip = to;
                    current = selected;
                    continue;
                }
                let base = backend.read_commit(&from)?;
                let merge = backend.merge_trees(&base.tree, &current.tree, &selected.tree)?;
                if let Some(report) = merge.conflicts {
                    let draft = backend.commit_tree(&CommitInfo {
                        tree: merge.tree,
                        parents: vec![tip.clone()],
                        author: plan.committer.clone(),
                        committer: plan.committer.clone(),
                        extra_headers: Vec::new(),
                        message: b"rebase conflict\n".to_vec(),
                    })?;
                    return Ok(Outcome {
                        layers,
                        conflict: Some(Conflict {
                            line,
                            parent: tip,
                            draft,
                            report,
                        }),
                    });
                }
                current = CommitInfo {
                    tree: merge.tree,
                    parents: vec![tip],
                    author: selected.author,
                    committer: plan.committer.clone(),
                    extra_headers: rewritten_headers(&selected.extra_headers, false),
                    message: selected.message,
                };
                tip = backend.commit_tree(&current)?;
            }
        }
    }
    Ok(Outcome {
        layers,
        conflict: None,
    })
}

// Continuations belong to their header. Rewritten commits cannot retain
// signatures or mergetags; replacement messages no longer use source encoding.
fn rewritten_headers(headers: &[u8], replace_message: bool) -> Vec<u8> {
    let mut result = Vec::new();
    let mut keep = false;
    for line in headers.split_inclusive(|byte| *byte == b'\n') {
        if !line.starts_with(b" ") {
            let name = line.split(|byte| *byte == b' ').next().unwrap_or_default();
            keep = !matches!(name, b"gpgsig" | b"gpgsig-sha256" | b"mergetag")
                && !(replace_message && name == b"encoding");
        }
        if keep {
            result.extend_from_slice(line);
        }
    }
    result
}

#[cfg(test)]
#[path = "rebase_plan_tests.rs"]
mod tests;
