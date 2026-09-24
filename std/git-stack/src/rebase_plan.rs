//! Replay Git objects without checking out their source trees. The caller guards
//! and applies conversation changes; this module produces objects and a plan.

use std::collections::{HashMap, HashSet};

use conversation_protocol::v3::{CommitInfo, Oid, Signature};
use serde_json::json;

pub trait Backend {
    fn read_commit(&mut self, oid: &Oid) -> Result<CommitInfo, String>;
    fn merge_trees(&mut self, base: &Oid, ours: &Oid, theirs: &Oid) -> Result<Merge, String>;
    fn commit_tree(&mut self, commit: &CommitInfo) -> Result<Oid, String>;
}

#[derive(Debug)]
pub struct Merge {
    pub tree: Oid,
    pub conflicts: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    pub name: String,
    pub tip: Oid,
    pub base: Oid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    Conflict,
}

impl PauseReason {
    pub fn as_str(self) -> &'static str {
        "conflict"
    }
}

#[derive(Debug)]
pub struct Pause {
    pub reason: PauseReason,
    pub instruction: String,
    pub draft_commit: Oid,
    pub conflicts: Option<Vec<u8>>,
}

#[derive(Debug)]
pub struct Outcome {
    pub plan: String,
    pub layers: Vec<Layer>,
    pub pause: Option<Pause>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Pick,
    Squash,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Squash => "squash",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selection {
    Single(Oid),
    Range { from: Oid, to: Oid },
}

impl Selection {
    fn parse(value: &str) -> Result<Self, String> {
        match value.split_once("..") {
            Some((from, to)) => Ok(Self::Range {
                from: Oid::parse(from, "range start")?,
                to: Oid::parse(to, "range end")?,
            }),
            None => Ok(Self::Single(Oid::parse(value, "commit")?)),
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Single(oid) => oid.to_string(),
            Self::Range { from, to } => format!("{from}..{to}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Instruction {
    Change {
        action: Action,
        selection: Selection,
    },
    Amend(Vec<String>),
    Drop(Oid),
    Branch(String),
}

impl Instruction {
    /// Consume physical lines, so blank lines and comments separate amend
    /// blocks. Never trim the literal text following `amend=`.
    fn parse(lines: &[&str], index: &mut usize, prefix: &str) -> Result<Self, String> {
        let line = lines[*index]
            .trim_start()
            .strip_prefix(prefix)
            .ok_or("invalid instruction prefix")?;
        if line.starts_with("amend=") {
            let mut message = Vec::new();
            while let Some(value) = lines
                .get(*index)
                .and_then(|line| line.trim_start().strip_prefix(prefix))
                .and_then(|line| line.strip_prefix("amend="))
            {
                message.push(value.to_owned());
                *index += 1;
            }
            return Ok(Self::Amend(message));
        }
        *index += 1;
        let (command, value) = line
            .trim()
            .split_once('=')
            .ok_or_else(|| format!("expected command=value: {line}"))?;
        let value = value.trim();
        match command {
            "pick" | "squash" => Ok(Self::Change {
                action: if command == "pick" {
                    Action::Pick
                } else {
                    Action::Squash
                },
                selection: Selection::parse(value)?,
            }),
            "drop" => Ok(Self::Drop(Oid::parse(value, "dropped commit")?)),
            "branch" => {
                layer_number(value)?;
                Ok(Self::Branch(value.to_owned()))
            }
            _ => Err(format!("invalid replay instruction: {line}")),
        }
    }

    fn lines(&self) -> Vec<String> {
        match self {
            Self::Change { action, selection } => {
                vec![format!("{}={}", action.as_str(), selection.render())]
            }
            Self::Amend(message) => message.iter().map(|line| format!("amend={line}")).collect(),
            Self::Drop(oid) => vec![format!("drop={oid}")],
            Self::Branch(name) => vec![format!("branch={name}")],
        }
    }

    fn render(&self) -> String {
        self.lines().join("\n")
    }
}

pub fn layer_number(name: &str) -> Result<usize, String> {
    conversation_protocol::v3::paths::validate_component(name)?;
    let (prefix, suffix) = name
        .split_once('-')
        .ok_or_else(|| format!("layer name must be <number>-<name>: {name}"))?;
    if suffix.is_empty()
        || name.chars().any(char::is_whitespace)
        || !prefix.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid layer name: {name}"));
    }
    let number: usize = prefix
        .parse()
        .map_err(|_| format!("invalid layer number: {prefix}"))?;
    if prefix != format!("{number:02}") {
        return Err(format!("layer number must be written as {number:02}"));
    }
    Ok(number)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Completed {
    instruction: Instruction,
    result: Oid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Marker {
    instruction: Instruction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub onto: Oid,
    pub original: Option<Oid>,
    pub committer: Option<Signature>,
    done: Vec<Completed>,
    here: Option<Marker>,
    pending: Vec<Instruction>,
}

impl Plan {
    pub fn parse(text: &str) -> Result<Self, String> {
        let lines: Vec<_> = text.lines().collect();
        let ignored = |line: &&str| line.trim().is_empty() || line.trim_start().starts_with('#');
        let mut index = lines
            .iter()
            .position(|line| !ignored(line))
            .ok_or("replay plan is empty")?;
        let onto = lines[index]
            .trim()
            .strip_prefix("onto=")
            .ok_or("the first plan instruction must be onto=<commit>")?;
        let mut plan = Self {
            onto: Oid::parse(onto.trim(), "onto")?,
            original: None,
            committer: None,
            done: Vec::new(),
            here: None,
            pending: Vec::new(),
        };
        index += 1;
        while index < lines.len() {
            let line = lines[index].trim_start();
            if ignored(&lines[index]) {
                index += 1;
            } else if let Some(value) = line.strip_prefix("original ") {
                if plan.original.is_some()
                    || !plan.done.is_empty()
                    || plan.here.is_some()
                    || !plan.pending.is_empty()
                {
                    return Err("original metadata must appear once before instructions".into());
                }
                plan.original = Some(Oid::parse(value.trim(), "original stack")?);
                index += 1;
            } else if let Some(value) = line.strip_prefix("committer ") {
                if plan.committer.is_some()
                    || !plan.done.is_empty()
                    || plan.here.is_some()
                    || !plan.pending.is_empty()
                {
                    return Err("committer metadata must appear once before instructions".into());
                }
                let value: serde_json::Value =
                    serde_json::from_str(value).map_err(|error| error.to_string())?;
                let field = |name: &str| {
                    value[name]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("committer metadata requires {name}"))
                };
                plan.committer = Some(Signature {
                    name: field("name")?,
                    email: field("email")?,
                    time: value["time"]
                        .as_i64()
                        .ok_or("committer metadata requires time")?,
                    offset: field("offset")?,
                });
                index += 1;
            } else if let Some(value) = line.strip_prefix("done ") {
                if plan.here.is_some() || !plan.pending.is_empty() {
                    return Err("completed instructions must precede the unfinished plan".into());
                }
                let (result, _) = value
                    .split_once(' ')
                    .ok_or("done requires a result and instruction")?;
                let result = Oid::parse(result, "completed result")?;
                let instruction =
                    Instruction::parse(&lines, &mut index, &format!("done {result} "))?;
                plan.done.push(Completed {
                    instruction,
                    result,
                });
            } else if let Some(instruction) = line.strip_prefix("here conflict ") {
                if plan.here.is_some() || !plan.pending.is_empty() {
                    return Err("here must appear once before unfinished instructions".into());
                }
                let instruction = Instruction::parse(&[instruction], &mut 0, "")?;
                if !matches!(instruction, Instruction::Change { .. }) {
                    return Err("only pick and squash can have a conflicted draft".into());
                }
                plan.here = Some(Marker { instruction });
                index += 1;
            } else {
                plan.pending
                    .push(Instruction::parse(&lines, &mut index, "")?);
            }
        }
        plan.validate()?;
        Ok(plan)
    }

    fn validate(&self) -> Result<(), String> {
        let mut number = 0;
        let mut unsaved_commit = false;
        let mut tip_branched = false;
        for instruction in self
            .done
            .iter()
            .map(|done| &done.instruction)
            .chain(&self.pending)
        {
            match instruction {
                Instruction::Branch(name) => {
                    if layer_number(name)? != number {
                        return Err(format!(
                            "branches must be consecutive; expected {number:02}-<name>"
                        ));
                    }
                    number += 1;
                    unsaved_commit = false;
                    tip_branched = true;
                }
                Instruction::Change { action, .. } => {
                    if *action == Action::Squash && !unsaved_commit {
                        return Err(
                            "squash requires an output commit since the last branch boundary"
                                .into(),
                        );
                    }
                    unsaved_commit = true;
                    tip_branched = false;
                }
                Instruction::Amend(_) => {
                    if !unsaved_commit {
                        return Err(
                            "amend requires an output commit since the last branch boundary".into(),
                        );
                    }
                    tip_branched = false;
                }
                Instruction::Drop(_) => (),
            }
        }
        if !tip_branched {
            return Err("the plan must record its final tip with branch".into());
        }
        let mut tip = &self.onto;
        for done in &self.done {
            if matches!(
                done.instruction,
                Instruction::Branch(_) | Instruction::Drop(_)
            ) && &done.result != tip
            {
                return Err(
                    "completed branch and drop instructions must keep the current tip".into(),
                );
            }
            tip = &done.result;
        }
        Ok(())
    }

    pub fn render(&self) -> String {
        let mut text = format!("onto={}\n", self.onto);
        if let Some(original) = &self.original {
            text.push_str(&format!("original {original}\n"));
        }
        if let Some(signature) = &self.committer {
            let value = json!({"name": signature.name, "email": signature.email,
                "time": signature.time, "offset": signature.offset});
            text.push_str(&format!("committer {value}\n"));
        }
        for done in &self.done {
            text.push('\n');
            for line in done.instruction.lines() {
                text.push_str(&format!("done {} {line}\n", done.result));
            }
        }
        if let Some(marker) = &self.here {
            text.push_str(&format!(
                "\nhere conflict {}\n",
                marker.instruction.render()
            ));
        }
        for instruction in &self.pending {
            // Adjacent amend blocks separated by a comment or blank in the input
            // remain distinct instructions after rendering.
            text.push('\n');
            text.push_str(&instruction.render());
            text.push('\n');
        }
        text
    }

    pub fn uses_draft(&self) -> bool {
        self.here
            .as_ref()
            .is_some_and(|marker| self.pending.first() == Some(&marker.instruction))
    }

    pub fn layers(&self) -> Vec<Layer> {
        let mut base = self.onto.clone();
        self.done
            .iter()
            .filter_map(|done| {
                let Instruction::Branch(name) = &done.instruction else {
                    return None;
                };
                let layer = Layer {
                    name: name.clone(),
                    tip: done.result.clone(),
                    base: base.clone(),
                };
                base = done.result.clone();
                Some(layer)
            })
            .collect()
    }

    fn tip(&self) -> &Oid {
        self.done
            .last()
            .map(|done| &done.result)
            .unwrap_or(&self.onto)
    }
}

pub fn start(
    backend: &mut impl Backend,
    source: &str,
    original: Oid,
    committer: Signature,
) -> Result<Outcome, String> {
    let mut plan = Plan::parse(source)?;
    if plan.original.is_some()
        || plan.committer.is_some()
        || !plan.done.is_empty()
        || plan.here.is_some()
    {
        return Err("a new plan cannot contain recorded progress or metadata".into());
    }
    plan.original = Some(original);
    plan.committer = Some(committer);
    Engine::new(backend).run(plan, None)
}

/// `previous` must be the last immutable result for this replay, not another
/// editable conversation file. The caller also guards saved output layers.
pub fn resume(
    backend: &mut impl Backend,
    source: &str,
    previous: &str,
    draft_tree: Option<&Oid>,
) -> Result<Outcome, String> {
    let plan = Plan::parse(source)?;
    let previous = Plan::parse(previous)?;
    if previous.here.is_none() || previous.original.is_none() || previous.committer.is_none() {
        return Err("the previous result is not an active replay".into());
    }
    if plan.onto != previous.onto
        || plan.original != previous.original
        || plan.committer != previous.committer
        || plan.done != previous.done
        || plan.here != previous.here
    {
        return Err(
            "only unfinished instructions can be edited; recorded progress and metadata are fixed"
                .into(),
        );
    }
    Engine::new(backend).run(plan, draft_tree)
}

struct Engine<'a, B> {
    backend: &'a mut B,
    commits: HashMap<Oid, CommitInfo>,
}

impl<'a, B: Backend> Engine<'a, B> {
    fn new(backend: &'a mut B) -> Self {
        Self {
            backend,
            commits: HashMap::new(),
        }
    }

    fn commit(&mut self, oid: &Oid) -> Result<CommitInfo, String> {
        if let Some(commit) = self.commits.get(oid) {
            return Ok(commit.clone());
        }
        let commit = self.backend.read_commit(oid)?;
        self.commits.insert(oid.clone(), commit.clone());
        Ok(commit)
    }

    fn range(&mut self, selection: &Selection) -> Result<(Oid, Oid), String> {
        match selection {
            Selection::Range { from, to } => Ok((from.clone(), to.clone())),
            Selection::Single(to) => {
                let commit = self.commit(to)?;
                match commit.parents.as_slice() {
                    [parent] => Ok((parent.clone(), to.clone())),
                    _ => Err(format!(
                        "single-commit replay requires exactly one parent: {to}"
                    )),
                }
            }
        }
    }

    fn validate_objects(&mut self, plan: &Plan) -> Result<(), String> {
        self.commit(&plan.onto)?;
        for instruction in &plan.pending {
            let selection = match instruction {
                Instruction::Change { selection, .. } => selection,
                Instruction::Drop(oid) => {
                    self.commit(oid)?;
                    continue;
                }
                _ => continue,
            };
            let (from, to) = self.range(selection)?;
            self.commit(&from)?;
            let mut cursor = to.clone();
            let mut seen = HashSet::new();
            while cursor != from {
                if !seen.insert(cursor.clone()) {
                    return Err("source history contains a cycle".into());
                }
                let commit = self.commit(&cursor)?;
                match commit.parents.as_slice() {
                    [parent] => cursor = parent.clone(),
                    [] => return Err(format!("range start {from} is not an ancestor of {to}")),
                    _ => return Err(format!("range {from}..{to} contains a merge commit; replay requires linear source history")),
                }
            }
        }
        Ok(())
    }

    fn write(&mut self, commit: CommitInfo) -> Result<Oid, String> {
        let oid = self.backend.commit_tree(&commit)?;
        self.commits.insert(oid.clone(), commit);
        Ok(oid)
    }

    fn run(&mut self, mut plan: Plan, draft_tree: Option<&Oid>) -> Result<Outcome, String> {
        self.validate_objects(&plan)?;
        let mut resolved = if plan.uses_draft() {
            Some(
                draft_tree
                    .ok_or("the paused instruction needs its draft at rebase/work")?
                    .clone(),
            )
        } else {
            None
        };
        plan.here = None;
        let signature = plan.committer.clone().ok_or("missing replay committer")?;
        let pending = std::mem::take(&mut plan.pending);
        for (index, instruction) in pending.iter().enumerate() {
            let tip = plan.tip().clone();
            let result = match instruction {
                Instruction::Branch(_) | Instruction::Drop(_) => tip,
                Instruction::Amend(lines) => {
                    let head = self.commit(&tip)?;
                    self.write(CommitInfo {
                        tree: head.tree,
                        parents: head.parents,
                        author: head.author,
                        committer: signature.clone(),
                        extra_headers: rewritten_headers(&head.extra_headers, true),
                        message: lines.join("\n").into_bytes(),
                    })?
                }
                Instruction::Change { action, selection } => {
                    let (from, to) = self.range(selection)?;
                    let head = self.commit(&tip)?;
                    let source = self.commit(&to)?;
                    let squash = *action == Action::Squash;
                    if squash
                        && !message_encoding(&head.extra_headers)
                            .eq_ignore_ascii_case(message_encoding(&source.extra_headers))
                    {
                        return Err(format!(
                            "squash requires compatible message encodings: preceding commit uses {}, source commit uses {}",
                            String::from_utf8_lossy(message_encoding(&head.extra_headers)),
                            String::from_utf8_lossy(message_encoding(&source.extra_headers)),
                        ));
                    }
                    let merge = match resolved.take() {
                        Some(tree) => Merge {
                            tree,
                            conflicts: None,
                        },
                        None => {
                            let base = self.commit(&from)?;
                            self.backend
                                .merge_trees(&base.tree, &head.tree, &source.tree)?
                        }
                    };
                    if merge.conflicts.is_some() {
                        let draft_commit = self.write(CommitInfo {
                            tree: merge.tree,
                            parents: vec![tip],
                            author: signature.clone(),
                            committer: signature.clone(),
                            extra_headers: Vec::new(),
                            message: b"rebase draft\n".to_vec(),
                        })?;
                        plan.here = Some(Marker {
                            instruction: instruction.clone(),
                        });
                        plan.pending = pending[index..].to_vec();
                        return Ok(Outcome {
                            plan: plan.render(),
                            layers: plan.layers(),
                            pause: Some(Pause {
                                reason: PauseReason::Conflict,
                                instruction: instruction.render(),
                                draft_commit,
                                conflicts: merge.conflicts,
                            }),
                        });
                    }
                    let message = if squash {
                        squash_messages(&head.message, &source.message)
                    } else {
                        source.message.clone()
                    };
                    let commit = CommitInfo {
                        tree: merge.tree,
                        parents: if squash {
                            head.parents.clone()
                        } else {
                            vec![tip.clone()]
                        },
                        author: if squash {
                            head.author.clone()
                        } else {
                            source.author.clone()
                        },
                        committer: signature.clone(),
                        extra_headers: rewritten_headers(
                            if squash {
                                &head.extra_headers
                            } else {
                                &source.extra_headers
                            },
                            false,
                        ),
                        message,
                    };
                    if !squash
                        && source.parents == vec![from.clone()]
                        && tip == from
                        && commit.tree == source.tree
                        && commit.message == source.message
                    {
                        to
                    } else {
                        self.write(commit)?
                    }
                }
            };
            plan.done.push(Completed {
                instruction: instruction.clone(),
                result,
            });
        }
        Ok(Outcome {
            plan: plan.render(),
            layers: plan.layers(),
            pause: None,
        })
    }
}

// Continuation lines belong to their preceding header, including signatures.
fn rewritten_headers(headers: &[u8], remove_encoding: bool) -> Vec<u8> {
    let mut result = Vec::new();
    let mut keep = false;
    for line in headers.split_inclusive(|byte| *byte == b'\n') {
        if !line.starts_with(b" ") {
            let name = line.split(|byte| *byte == b' ').next().unwrap_or_default();
            keep = !matches!(name, b"gpgsig" | b"gpgsig-sha256" | b"mergetag")
                && !(remove_encoding && name == b"encoding");
        }
        if keep {
            result.extend_from_slice(line);
        }
    }
    result
}

fn message_encoding(headers: &[u8]) -> &[u8] {
    let encoding = headers
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"encoding "))
        .unwrap_or(b"UTF-8");
    if encoding.eq_ignore_ascii_case(b"UTF-8") || encoding.eq_ignore_ascii_case(b"UTF8") {
        b"UTF-8"
    } else {
        encoding
    }
}

fn squash_messages(previous: &[u8], next: &[u8]) -> Vec<u8> {
    let end = previous
        .iter()
        .rposition(|byte| *byte != b'\n')
        .map_or(0, |index| index + 1);
    let mut message = previous[..end].to_vec();
    message.extend_from_slice(b"\n\n");
    message.extend_from_slice(next);
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn oid(number: usize) -> Oid {
        Oid::parse(&format!("{number:040x}"), "test oid").unwrap()
    }
    fn signature(time: i64) -> Signature {
        Signature {
            name: "Author".into(),
            email: "author@example.com".into(),
            time,
            offset: "+0000".into(),
        }
    }

    struct Fake {
        commits: HashMap<Oid, CommitInfo>,
        merges: Vec<(Oid, Oid, Oid)>,
        results: VecDeque<Merge>,
        written: Vec<CommitInfo>,
        next: usize,
    }

    impl Fake {
        fn linear(length: usize) -> Self {
            let mut fake = Self {
                commits: HashMap::new(),
                merges: Vec::new(),
                results: VecDeque::new(),
                written: Vec::new(),
                next: 10000,
            };
            for n in 1..=length {
                fake.commits.insert(
                    oid(n),
                    CommitInfo {
                        tree: oid(n + 1000),
                        parents: if n == 1 { Vec::new() } else { vec![oid(n - 1)] },
                        author: signature(n as i64),
                        committer: signature(n as i64),
                        extra_headers: Vec::new(),
                        message: format!("source {n}\n").into_bytes(),
                    },
                );
            }
            fake
        }

        fn run(&mut self, body: &str) -> Outcome {
            start(
                self,
                &format!("onto={}\n{body}", oid(1)),
                oid(900),
                signature(99),
            )
            .unwrap()
        }

        fn conflict(&mut self) {
            self.results.push_back(Merge {
                tree: oid(444),
                conflicts: Some(b"native\0conflict\0".to_vec()),
            });
        }

        fn paused(&mut self) -> Outcome {
            self.conflict();
            self.run(&format!("pick={}..{}\nbranch=00-feature\n", oid(1), oid(3)))
        }
    }

    impl Backend for Fake {
        fn read_commit(&mut self, oid: &Oid) -> Result<CommitInfo, String> {
            self.commits
                .get(oid)
                .cloned()
                .ok_or_else(|| format!("missing commit {oid}"))
        }
        fn merge_trees(&mut self, base: &Oid, ours: &Oid, theirs: &Oid) -> Result<Merge, String> {
            self.merges
                .push((base.clone(), ours.clone(), theirs.clone()));
            if let Some(result) = self.results.pop_front() {
                return Ok(result);
            }
            let tree = if base == ours {
                theirs
            } else if base == theirs || ours == theirs {
                ours
            } else {
                return Err("test must supply a merge result".into());
            };
            Ok(Merge {
                tree: tree.clone(),
                conflicts: None,
            })
        }
        fn commit_tree(&mut self, commit: &CommitInfo) -> Result<Oid, String> {
            self.next += 1;
            let result = oid(self.next);
            self.commits.insert(result.clone(), commit.clone());
            self.written.push(commit.clone());
            Ok(result)
        }
    }

    #[test]
    fn many_tool_commits_become_two_commits_and_layers() {
        let mut backend = Fake::linear(801);
        let result = backend.run(&format!(
            "pick={}..{}\nbranch=00-first\npick={}..{}\nbranch=01-second\n",
            oid(1),
            oid(501),
            oid(501),
            oid(801),
        ));
        assert!(result.pause.is_none());
        assert_eq!(backend.merges.len(), 2);
        assert_eq!(backend.written.len(), 2);
        let first = &backend.commits[&result.layers[0].tip];
        let second = &backend.commits[&result.layers[1].tip];
        assert_eq!(first.parents, vec![oid(1)]);
        assert_eq!(first.tree, oid(1501));
        assert_eq!(first.author, signature(501));
        assert_eq!(second.parents, vec![result.layers[0].tip.clone()]);
        assert_eq!(second.tree, oid(1801));
        assert_eq!(second.committer, signature(99));
        assert_eq!(result.layers[0].base, oid(1));
        assert_eq!(result.layers[1].base, result.layers[0].tip);
        assert_eq!(Plan::parse(&result.plan).unwrap().render(), result.plan);
    }

    #[test]
    fn documented_example_combines_amends_omits_and_branches() {
        let mut backend = Fake::linear(5);
        let mut base = backend.commits[&oid(1)].clone();
        base.tree = oid(2000);
        backend.commits.insert(oid(10), base);
        for tree in [3001, 3002, 3003] {
            backend.results.push_back(Merge {
                tree: oid(tree),
                conflicts: None,
            });
        }
        let plan = format!(
            "onto={}\npick={}\nsquash={}\namend=Add authentication\namend=\namend=Support session cookies and API tokens\nbranch=00-auth\ndrop={}\npick={}\nbranch=01-logging\n",
            oid(10), oid(2), oid(3), oid(4), oid(5),
        );
        let result = start(&mut backend, &plan, oid(900), signature(99)).unwrap();
        assert_eq!(
            backend.merges,
            vec![
                (oid(1001), oid(2000), oid(1002)),
                (oid(1002), oid(3001), oid(1003)),
                (oid(1004), oid(3002), oid(1005)),
            ]
        );
        let first = &backend.commits[&result.layers[0].tip];
        let last = &backend.commits[&result.layers[1].tip];
        assert_eq!(first.parents, vec![oid(10)]);
        assert_eq!(first.author, signature(2));
        assert_eq!(
            first.message,
            b"Add authentication\n\nSupport session cookies and API tokens"
        );
        assert_eq!(last.parents, vec![result.layers[0].tip.clone()]);
        assert_eq!(last.author, signature(5));
        assert_eq!(result.layers[1].base, result.layers[0].tip);
        assert_eq!(Plan::parse(&result.plan).unwrap().render(), result.plan);
    }

    #[test]
    fn single_and_one_commit_range_reuse_original_commit() {
        for pick in [oid(2).to_string(), format!("{}..{}", oid(1), oid(2))] {
            let mut backend = Fake::linear(2);
            backend.commits.get_mut(&oid(2)).unwrap().extra_headers =
                b"gpgsig signature\n".to_vec();
            let result = backend.run(&format!("pick={pick}\nbranch=00-feature\n"));
            assert_eq!(result.layers[0].tip, oid(2));
            assert!(backend.written.is_empty());
            assert_eq!(backend.merges, vec![(oid(1001), oid(1001), oid(1002))]);
        }
    }

    #[test]
    fn rewritten_pick_preserves_encoded_message_and_removes_multiline_signatures() {
        let mut backend = Fake::linear(3);
        let source = backend.commits.get_mut(&oid(3)).unwrap();
        source.message = b"caf\xe9\n".to_vec();
        source.extra_headers = b"gpgsig signature\n continuation\n \nencoding ISO-8859-1\nx-custom value\n continued\ngpgsig-sha256 signature\n continuation\nmergetag object hash\n type commit\n \n tag message\nx-last raw\xff\n".to_vec();
        let result = backend.run(&format!("pick={}..{}\nbranch=00-feature\n", oid(1), oid(3),));
        let commit = &backend.commits[&result.layers[0].tip];
        assert_eq!(backend.written.len(), 1);
        assert_eq!(commit.message, b"caf\xe9\n");
        assert_eq!(commit.author, signature(3));
        assert_eq!(
            commit.extra_headers,
            b"encoding ISO-8859-1\nx-custom value\n continued\nx-last raw\xff\n"
        );
    }

    #[test]
    fn inline_amend_removes_encoding_and_signatures_but_preserves_other_headers() {
        let mut backend = Fake::linear(2);
        let source = backend.commits.get_mut(&oid(2)).unwrap();
        source.message = b"caf\xe9\n".to_vec();
        source.extra_headers =
            b"encoding ISO-8859-1\ngpgsig signature\n continuation\nx-custom value\n continued\n"
                .to_vec();
        let result = backend.run(&format!("pick={}\namend=café\nbranch=00-feature\n", oid(2),));
        let commit = &backend.commits[&result.layers[0].tip];
        assert_eq!(backend.written.len(), 1, "pick reuses the signed original");
        assert_eq!(commit.message, "café".as_bytes());
        assert_eq!(commit.extra_headers, b"x-custom value\n continued\n");
        assert_eq!(commit.tree, oid(1002));
        assert_eq!(commit.parents, vec![oid(1)]);
        assert_eq!(commit.author, signature(2));
    }

    #[test]
    fn squash_preserves_prior_headers_and_matching_message_encoding() {
        let mut backend = Fake::linear(3);
        let first = backend.commits.get_mut(&oid(2)).unwrap();
        first.extra_headers =
            b"encoding ISO-8859-1\ngpgsig signature\n continuation\nx-custom previous\n".to_vec();
        first.message = b"caf\xe9\n".to_vec();
        let second = backend.commits.get_mut(&oid(3)).unwrap();
        second.extra_headers = b"encoding iso-8859-1\nx-custom source\n".to_vec();
        second.message = b"d\xe9j\xe0\n".to_vec();
        let result = backend.run(&format!(
            "pick={}\nsquash={}\nbranch=00-feature\n",
            oid(2),
            oid(3),
        ));
        let commit = &backend.commits[&result.layers[0].tip];
        assert_eq!(commit.message, b"caf\xe9\n\nd\xe9j\xe0\n");
        assert_eq!(
            commit.extra_headers,
            b"encoding ISO-8859-1\nx-custom previous\n"
        );
        assert_eq!(commit.parents, vec![oid(1)]);
        assert_eq!(commit.author, signature(2));
    }

    #[test]
    fn squash_accepts_implicit_and_explicit_utf8_without_changing_prior_headers() {
        for (previous, next) in [
            ("", "encoding UTF-8\n"),
            ("encoding UTF-8\n", ""),
            ("encoding utf-8\n", "encoding UTF8\n"),
        ] {
            let mut backend = Fake::linear(3);
            backend.commits.get_mut(&oid(2)).unwrap().extra_headers = previous.as_bytes().to_vec();
            backend.commits.get_mut(&oid(3)).unwrap().extra_headers = next.as_bytes().to_vec();
            let result = backend.run(&format!(
                "pick={}\nsquash={}\nbranch=00-feature\n",
                oid(2),
                oid(3),
            ));
            let commit = &backend.commits[&result.layers[0].tip];
            assert_eq!(commit.extra_headers, previous.as_bytes());
            assert_eq!(commit.message, b"source 2\n\nsource 3\n");
        }
    }

    #[test]
    fn squash_rejects_incompatible_encodings_before_merging_or_writing() {
        for (previous, next) in [
            ("encoding ISO-8859-1\n", ""),
            ("", "encoding ISO-8859-1\n"),
            ("encoding ISO-8859-1\n", "encoding ISO-8859-2\n"),
        ] {
            let mut backend = Fake::linear(3);
            backend.commits.get_mut(&oid(2)).unwrap().extra_headers = previous.as_bytes().to_vec();
            backend.commits.get_mut(&oid(3)).unwrap().extra_headers = next.as_bytes().to_vec();
            let plan = format!(
                "onto={}\npick={}\nsquash={}\nbranch=00-feature\n",
                oid(1),
                oid(2),
                oid(3),
            );
            let error = start(&mut backend, &plan, oid(900), signature(99)).unwrap_err();
            assert!(
                error.contains("squash requires compatible message encodings"),
                "{error}"
            );
            assert_eq!(backend.merges.len(), 1, "only the unchanged pick merged");
            assert!(backend.written.is_empty());
        }
    }

    #[test]
    fn single_root_or_merge_pick_and_squash_are_rejected_before_writes() {
        for command in ["pick", "squash"] {
            for invalid in [1, 3] {
                let mut backend = Fake::linear(3);
                backend
                    .commits
                    .get_mut(&oid(3))
                    .unwrap()
                    .parents
                    .push(oid(1));
                let plan = format!(
                    "onto={}\npick={}\n{command}={}\nbranch=00-feature\n",
                    oid(1),
                    oid(2),
                    oid(invalid)
                );
                assert!(start(&mut backend, &plan, oid(900), signature(99))
                    .unwrap_err()
                    .contains("exactly one parent"));
                assert!(backend.merges.is_empty());
                assert!(backend.written.is_empty());
            }
        }
    }

    #[test]
    fn squash_concatenates_messages_and_keeps_preceding_parent_and_author() {
        let mut backend = Fake::linear(3);
        backend.commits.get_mut(&oid(2)).unwrap().message = b"First without newline".to_vec();
        let result = backend.run(&format!(
            "pick={}\nsquash={}\nbranch=00-feature\n",
            oid(2),
            oid(3)
        ));
        let commit = &backend.commits[&result.layers[0].tip];
        assert_eq!(commit.parents, vec![oid(1)]);
        assert_eq!(commit.author, signature(2));
        assert_eq!(commit.tree, oid(1003));
        assert_eq!(commit.message, b"First without newline\n\nsource 3\n");
        assert_eq!(
            squash_messages(b"first\n", b"caf\xe9\n"),
            b"first\n\ncaf\xe9\n"
        );
    }

    #[test]
    fn amend_block_is_one_instruction_with_literal_text_and_no_implicit_newline() {
        let mut backend = Fake::linear(3);
        let text = format!("  title=#value -> {}  ", oid(5));
        let result = backend.run(&format!(
            "pick={}..{}\namend={text}\namend=\namend=# body = value  \nbranch=00-feature\n",
            oid(1),
            oid(3),
        ));
        assert_eq!(
            backend.written.len(),
            2,
            "one range commit and one amend block"
        );
        let before = &backend.written[0];
        let amended = &backend.written[1];
        assert_eq!(amended.tree, before.tree);
        assert_eq!(amended.parents, before.parents);
        assert_eq!(amended.author, before.author);
        assert_eq!(
            amended.message,
            format!("{text}\n\n# body = value  ").as_bytes()
        );
        let parsed = Plan::parse(&result.plan).unwrap();
        assert_eq!(parsed.done.len(), 3);
        assert_eq!(parsed.render(), result.plan);
    }

    #[test]
    fn physical_blank_lines_and_comments_separate_amend_blocks() {
        let mut backend = Fake::linear(2);
        let source = format!(
            "onto={}\npick={}\namend=first\n\namend=second\n# separator\namend=third\namend=\nbranch=00-feature\n", oid(1), oid(2),
        );
        let parsed = Plan::parse(&source).unwrap();
        assert_eq!(parsed.pending.len(), 5);
        assert_eq!(Plan::parse(&parsed.render()).unwrap(), parsed);
        let result = start(&mut backend, &source, oid(900), signature(99)).unwrap();
        assert_eq!(backend.written.len(), 3);
        assert_eq!(backend.commits[&result.layers[0].tip].message, b"third\n");
        assert_eq!(Plan::parse(&result.plan).unwrap().done.len(), 5);
    }

    #[test]
    fn empty_amend_and_empty_ranges_are_preserved() {
        let mut backend = Fake::linear(1);
        let result = backend.run(&format!(
            "pick={}..{}\namend=\nbranch=00-feature\n",
            oid(1),
            oid(1)
        ));
        let commit = &backend.commits[&result.layers[0].tip];
        assert_eq!(commit.tree, oid(1001));
        assert_eq!(commit.parents, vec![oid(1)]);
        assert!(commit.message.is_empty());
        assert_eq!(backend.written.len(), 2);
        let result = backend.run(&format!(
            "pick={}..{}\nsquash={}..{}\nbranch=00-feature\n",
            oid(1),
            oid(1),
            oid(1),
            oid(1)
        ));
        let commit = &backend.commits[&result.layers[0].tip];
        assert_eq!(commit.tree, oid(1001));
        assert_eq!(commit.parents, vec![oid(1)]);
        assert_eq!(commit.message, b"source 1\n\nsource 1\n");
    }

    #[test]
    fn drop_is_noop_even_for_changes_already_in_a_range_and_can_trail_branch() {
        let mut backend = Fake::linear(3);
        let result = backend.run(&format!(
            "drop={}\npick={}..{}\nbranch=00-feature\ndrop={}\n",
            oid(2),
            oid(1),
            oid(3),
            oid(3),
        ));
        assert_eq!(backend.merges.len(), 1);
        assert_eq!(backend.written.len(), 1);
        assert_eq!(backend.commits[&result.layers[0].tip].tree, oid(1003));
        assert_eq!(
            Plan::parse(&result.plan).unwrap().tip(),
            &result.layers[0].tip
        );
    }

    #[test]
    fn empty_layers_branch_the_same_tip() {
        let result = Fake::linear(1).run(&format!(
            "drop={}\nbranch=00-first\nbranch=01-empty\n",
            oid(1)
        ));
        assert_eq!(
            result.layers,
            vec![
                Layer {
                    name: "00-first".into(),
                    tip: oid(1),
                    base: oid(1)
                },
                Layer {
                    name: "01-empty".into(),
                    tip: oid(1),
                    base: oid(1)
                },
            ]
        );
    }

    #[test]
    fn conflicts_preserve_report_and_continue_uses_resolved_tree_once() {
        let mut backend = Fake::linear(3);
        let first = backend.paused();
        let pause = first.pause.as_ref().unwrap();
        assert_eq!(pause.reason, PauseReason::Conflict);
        assert_eq!(
            pause.conflicts.as_deref(),
            Some(b"native\0conflict\0".as_slice())
        );
        assert_eq!(backend.commits[&pause.draft_commit].tree, oid(444));
        let edited = first.plan.replace(
            "branch=00-feature",
            "amend=Resolved message\nbranch=00-feature",
        );
        let result = resume(&mut backend, &edited, &first.plan, Some(&oid(445))).unwrap();
        assert!(result.pause.is_none());
        let commit = &backend.commits[&result.layers[0].tip];
        assert_eq!(commit.tree, oid(445));
        assert_eq!(commit.parents, vec![oid(1)]);
        assert_eq!(commit.author, signature(3));
        assert_eq!(commit.message, b"Resolved message");
        assert_eq!(backend.merges.len(), 1);
    }

    #[test]
    fn conflicted_squash_preserves_parent_and_combines_messages_on_continue() {
        let mut backend = Fake::linear(3);
        backend.results.push_back(Merge {
            tree: oid(1002),
            conflicts: None,
        });
        backend.conflict();
        let first = backend.run(&format!(
            "pick={}\nsquash={}\nbranch=00-feature\n",
            oid(2),
            oid(3)
        ));
        let result = resume(&mut backend, &first.plan, &first.plan, Some(&oid(445))).unwrap();
        let commit = &backend.commits[&result.layers[0].tip];
        assert_eq!(commit.parents, vec![oid(1)]);
        assert_eq!(commit.tree, oid(445));
        assert_eq!(commit.author, signature(2));
        assert_eq!(commit.message, b"source 2\n\nsource 3\n");
        assert_eq!(backend.merges.len(), 2);
    }

    #[test]
    fn replacing_paused_instruction_discards_draft_and_can_split_layers() {
        let mut backend = Fake::linear(3);
        let first = backend.paused();
        let instruction = format!("pick={}..{}", oid(1), oid(3));
        let edited = first
            .plan
            .replace(
                &format!("\n{instruction}\n"),
                &format!("\npick={}\nbranch=00-first\npick={}\n", oid(2), oid(3)),
            )
            .replace("branch=00-feature", "branch=01-second");
        assert!(!Plan::parse(&edited).unwrap().uses_draft());
        let result = resume(&mut backend, &edited, &first.plan, None).unwrap();
        assert_eq!(result.layers[0].tip, oid(2));
        assert_eq!(result.layers[1].tip, oid(3));
        assert_eq!(backend.merges.len(), 3);
    }

    #[test]
    fn removing_paused_instruction_can_complete_an_empty_layer() {
        let mut backend = Fake::linear(3);
        let first = backend.paused();
        let edited = first
            .plan
            .replace(&format!("\npick={}..{}\n", oid(1), oid(3)), "\n");
        assert!(!Plan::parse(&edited).unwrap().uses_draft());
        let result = resume(&mut backend, &edited, &first.plan, None).unwrap();
        assert_eq!(result.layers[0].tip, oid(1));
        assert!(result.pause.is_none());
    }

    #[test]
    fn only_unchanged_paused_instruction_uses_draft() {
        let mut backend = Fake::linear(3);
        let first = backend.paused();
        assert!(Plan::parse(&first.plan).unwrap().uses_draft());
        let later = first
            .plan
            .replace("branch=00-feature", "amend=Later\nbranch=00-renamed");
        assert!(Plan::parse(&later).unwrap().uses_draft());
        assert!(resume(&mut backend, &later, &first.plan, None)
            .unwrap_err()
            .contains("needs its draft"));
        let raw = format!("onto={}\npick={}\nbranch=00-feature\n", oid(1), oid(2));
        assert!(!Plan::parse(&raw).unwrap().uses_draft());
    }

    #[test]
    fn completed_amend_text_and_other_fixed_progress_cannot_change() {
        let mut backend = Fake::linear(3);
        backend.results.push_back(Merge {
            tree: oid(1002),
            conflicts: None,
        });
        backend.conflict();
        let first = backend.run(&format!(
            "pick={}\namend=Keep this = # text  \namend=\nbranch=00-first\npick={}\nbranch=01-second\n", oid(2), oid(3),
        ));
        let variants = [
            first
                .plan
                .replace("amend=Keep this = # text  ", "amend=Keep this = # text "),
            first.plan.replace("done ", "done 0"),
            first
                .plan
                .replace(&format!("onto={}", oid(1)), &format!("onto={}", oid(2))),
            first.plan.replace(
                &format!("original {}", oid(900)),
                &format!("original {}", oid(901)),
            ),
            first.plan.replace("\"time\":99", "\"time\":100"),
            first.plan.replace("branch=00-first", "branch=00-changed"),
            first.plan.replace(
                &format!("here conflict pick={}", oid(3)),
                &format!("here conflict pick={}", oid(2)),
            ),
        ];
        for edited in variants {
            assert!(
                resume(&mut backend, &edited, &first.plan, Some(&oid(999))).is_err(),
                "{edited}"
            );
        }
    }

    #[test]
    fn branch_seals_layer_against_squash_and_amend_but_drop_does_not_seal() {
        for body in [
            "amend=base\nbranch=00-feature".to_owned(),
            format!("squash={}\nbranch=00-feature", oid(2)),
            format!(
                "pick={}\nbranch=00-first\namend=sealed\nbranch=01-second",
                oid(2)
            ),
            format!(
                "pick={}\nbranch=00-first\ndrop={}\nsquash={}\nbranch=01-second",
                oid(2),
                oid(3),
                oid(3)
            ),
        ] {
            assert!(
                Plan::parse(&format!("onto={}\n{body}\n", oid(1))).is_err(),
                "{body}"
            );
        }
        let mut backend = Fake::linear(3);
        let result = backend.run(&format!(
            "pick={}\ndrop={}\namend=still unsaved\nbranch=00-feature\n",
            oid(2),
            oid(3)
        ));
        assert_eq!(
            backend.commits[&result.layers[0].tip].message,
            b"still unsaved"
        );
    }

    #[test]
    fn invalid_range_rejected_before_any_execution_and_pending_changes_validated() {
        let mut backend = Fake::linear(4);
        backend
            .commits
            .get_mut(&oid(3))
            .unwrap()
            .parents
            .push(oid(1));
        let plan = format!(
            "onto={}\npick={}..{}\nbranch=00-feature\n",
            oid(1),
            oid(1),
            oid(4)
        );
        assert!(start(&mut backend, &plan, oid(900), signature(99))
            .unwrap_err()
            .contains("merge commit"));
        assert!(backend.merges.is_empty());
        let mut backend = Fake::linear(3);
        let first = backend.paused();
        let before = backend.written.len();
        let edited = first.plan.replace(
            "branch=00-feature",
            &format!("pick={}..{}\nbranch=00-feature", oid(3), oid(1)),
        );
        assert!(resume(&mut backend, &edited, &first.plan, Some(&oid(445)))
            .unwrap_err()
            .contains("not an ancestor"));
        assert_eq!(backend.written.len(), before);
    }

    #[test]
    fn obsolete_commands_invalid_numbers_and_unsaved_tips_are_rejected() {
        for body in [
            format!("pick={}\n", oid(2)),
            format!(
                "pick={}\nbranch=00-first\npick={}\ndrop={}\n",
                oid(2),
                oid(3),
                oid(3)
            ),
            format!("edit={}\nbranch=00-feature", oid(2)),
            format!("reword={} message\nbranch=00-feature", oid(2)),
            format!("drop={}..{}\nbranch=00-feature", oid(1), oid(2)),
            format!("pick={} messages/title\nbranch=00-feature", oid(2)),
            "branch=01-feature".into(),
            "branch=00-first\nbranch=00-second".into(),
            "branch=00-first\nbranch=02-second".into(),
            "branch=00-../escape".into(),
            "branch=000-first".into(),
            "save 00-feature".into(),
        ] {
            assert!(
                Plan::parse(&format!("onto={}\n{body}\n", oid(1))).is_err(),
                "{body}"
            );
        }
    }
}
