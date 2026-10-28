//! Publish a stack directory as branches and chained pull requests in one
//! operation (design/stack-publish.md). Every layer's push is a pinned
//! publication exactly like `publish_source`'s, and all of them are pinned
//! together, with the plan, before the first push: a resumed attempt sends what
//! the first one planned and nothing newer. The GitHub steps are reads before
//! writes, so repeating them is safe.
use std::collections::BTreeSet;

use super::*;
use conversation_protocol::v3::publication::Outcome;
use conversation_protocol::v3::source_trees::validate_branch;
use conversation_protocol::v3::{Descriptor, Evidence, GitStore, PublicationRecord, PublicationStatus};

use crate::publish_source::bool_or_string;

const API: &str = "https://api.github.com";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    stack: String,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    trunk: Option<String>,
    #[serde(default, rename = "branch-prefix", alias = "branch_prefix")]
    branch_prefix: Option<String>,
    #[serde(default, deserialize_with = "bool_or_string")]
    force: bool,
    #[serde(
        default,
        rename = "allow-ignored",
        alias = "allow_ignored",
        deserialize_with = "bool_or_string"
    )]
    allow_ignored: bool,
    #[serde(default = "yes", deserialize_with = "bool_or_string")]
    link: bool,
    #[serde(default, deserialize_with = "bool_or_string")]
    draft: bool,
    #[serde(
        default,
        rename = "dry-run",
        alias = "dry_run",
        deserialize_with = "bool_or_string"
    )]
    dry_run: bool,
}

fn yes() -> bool {
    true
}

fn blank(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

fn parameters(call: &Call) -> Result<Parameters, String> {
    let mut p: Parameters = serde_json::from_value(call.input.clone())
        .map_err(|error| format!("invalid stack-publish arguments: {error}"))?;
    p.stack = p.stack.trim().trim_matches('/').to_string();
    if p.stack.is_empty() {
        return Err("stack must name a directory, such as sources/repo".into());
    }
    paths::validate_source_tree_name(&p.stack)?;
    p.repository = blank(p.repository);
    if let Some(repository) = &p.repository {
        git_locator::import::remote(repository)?;
    }
    p.trunk = blank(p.trunk);
    if let Some(trunk) = &p.trunk {
        validate_branch(trunk)?;
    }
    p.branch_prefix = blank(p.branch_prefix);
    Ok(p)
}

/// A layer's PR text, from its `NN-<name>.pr.md`: the first non-empty line is
/// the title (a leading `#` heading marker is dropped), the rest the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Description {
    pub title: String,
    pub body: String,
}

pub(super) fn parse_description(text: &str) -> Result<Description, String> {
    let mut lines = text.lines();
    let title = loop {
        match lines.next() {
            None => return Err("the file is empty; its first line is the PR title".into()),
            Some(line) if line.trim().is_empty() => continue,
            Some(line) => break line.trim().trim_start_matches('#').trim().to_string(),
        }
    };
    if title.is_empty() {
        return Err("the first line is a bare heading marker; the PR title goes there".into());
    }
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    Ok(Description { title, body })
}

/// `NN-<name>`: two digits, a dash, a name. The number orders the folder and
/// the name is the branch.
pub(super) fn layer_name(entry: &str) -> Option<(&str, &str)> {
    let bytes = entry.as_bytes();
    if bytes.len() < 4 || !bytes[0].is_ascii_digit() || !bytes[1].is_ascii_digit() || bytes[2] != b'-'
    {
        return None;
    }
    Some((&entry[..2], &entry[3..]))
}

#[derive(Clone, Debug)]
pub(super) struct Layer {
    pub name: String,
    pub short: String,
    pub commit: Oid,
    pub description: Option<Description>,
}

pub(super) struct Stack {
    pub base: Oid,
    pub layers: Vec<Layer>,
}

/// Read the stack directory: `00-base`, the layers in name order, and each
/// layer's description. Refused: a layer-named entry that is not a gitlink, a
/// description with no layer, and a described layer above an undescribed one.
fn read_stack(view: &Conversation<'_>, dir: &str) -> Result<Stack, String> {
    let entries = view
        .snapshot()
        .list(dir)
        .map_err(|error| format!("stack {dir}: {error}"))?;
    let mut base = None;
    let mut layers: Vec<Layer> = Vec::new();
    let mut descriptions: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in entries {
        if let Some(stem) = entry.name.strip_suffix(".pr.md") {
            if !matches!(entry.mode, Mode::Blob | Mode::Executable) {
                return Err(format!("{dir}/{} is not a file", entry.name));
            }
            let path = format!("{dir}/{}", entry.name);
            let bytes = view
                .snapshot()
                .read(&path)?
                .ok_or_else(|| format!("{path} vanished while reading the stack"))?;
            descriptions.push((stem.to_string(), bytes));
            continue;
        }
        let Some((number, short)) = layer_name(&entry.name) else {
            continue;
        };
        if number == "00" {
            if entry.name == "00-base" && entry.mode == Mode::Commit {
                base = Some(entry.oid.clone());
            }
            continue;
        }
        if entry.mode != Mode::Commit {
            return Err(format!(
                "{dir}/{0} is named like a layer but is not a gitlink. A layer is a source tree; its description is {0}.pr.md beside it.",
                entry.name
            ));
        }
        layers.push(Layer {
            name: entry.name.clone(),
            short: short.to_string(),
            commit: entry.oid.clone(),
            description: None,
        });
    }
    let base = base.ok_or_else(|| {
        format!("{dir} has no 00-base gitlink: import the upstream tip there first (std/GIT.md)")
    })?;
    for (stem, bytes) in descriptions {
        let text =
            String::from_utf8(bytes).map_err(|_| format!("{dir}/{stem}.pr.md is not UTF-8"))?;
        let description =
            parse_description(&text).map_err(|error| format!("{dir}/{stem}.pr.md: {error}"))?;
        match layers.iter_mut().find(|layer| layer.name == stem) {
            Some(layer) => layer.description = Some(description),
            None => {
                return Err(format!(
                    "{dir}/{stem}.pr.md describes no layer: there is no gitlink {dir}/{stem}"
                ))
            }
        }
    }
    let mut undescribed: Option<&str> = None;
    for layer in &layers {
        match (&layer.description, undescribed) {
            (None, _) => undescribed = Some(layer.name.as_str()),
            (Some(_), Some(below)) => {
                return Err(format!(
                    "{dir}/{} has a description but {dir}/{below} below it has none, so its PR would have no branch to target. Write {dir}/{below}.pr.md, or take {below} out of the stack.",
                    layer.name
                ))
            }
            (Some(_), None) => {}
        }
    }
    Ok(Stack { base, layers })
}

/// What `00-base.source.json` says about where the base came from.
#[derive(Default)]
pub(super) struct Provenance {
    pub repository: Option<String>,
    pub requested: Option<String>,
    pub default_branch: Option<String>,
}

fn provenance(view: &Conversation<'_>, dir: &str) -> Result<Provenance, String> {
    let path = format!("{dir}/00-base.source.json");
    let Some(bytes) = view.snapshot().read(&path)? else {
        return Ok(Provenance::default());
    };
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| format!("{path} is not JSON"))?;
    let text = |key: &str| value[key].as_str().map(str::to_string);
    Ok(Provenance {
        repository: text("repository"),
        requested: text("requested_revision"),
        default_branch: text("default_branch"),
    })
}

/// The trunk the first layer targets, from the import: the branch that was
/// requested, or the remote's default branch when none was. A requested commit
/// hash or a tag names no trunk.
pub(super) fn infer_trunk(provenance: &Provenance) -> Option<String> {
    match &provenance.requested {
        Some(requested) if git_locator::import::commit(requested) => None,
        Some(requested) => match requested.strip_prefix("refs/") {
            Some(rest) => rest.strip_prefix("heads/").map(str::to_string),
            None => Some(requested.clone()),
        },
        None => provenance.default_branch.clone(),
    }
}

/// One layer as it will be published. Pinned with the publications, so a
/// resumed attempt publishes the same branches with the same text.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct PlannedLayer {
    name: String,
    path: String,
    branch: String,
    base: String,
    title: String,
    body: String,
    commit: Oid,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Plan {
    repository: String,
    trunk: String,
    link: bool,
    draft: bool,
    layers: Vec<PlannedLayer>,
}

/// Everything checked and decided before anything is observed or pushed.
fn prepare(view: &Conversation<'_>, store: &GitStore, p: &Parameters) -> Result<Plan, String> {
    let dir = &p.stack;
    let stack = read_stack(view, dir)?;
    let provenance = provenance(view, dir)?;
    let repository = p
        .repository
        .clone()
        .or_else(|| provenance.repository.clone())
        .ok_or_else(|| {
            format!("no repository: {dir}/00-base.source.json names none, so pass repository")
        })?;
    git_locator::import::remote(&repository)?;
    let trunk = p
        .trunk
        .clone()
        .or_else(|| infer_trunk(&provenance))
        .ok_or_else(|| {
            format!("no trunk: {dir}/00-base.source.json names no branch, so pass trunk")
        })?;
    validate_branch(&trunk)?;
    let published: Vec<&Layer> = stack
        .layers
        .iter()
        .filter(|layer| layer.description.is_some())
        .collect();
    if published.is_empty() {
        return Err(format!(
            "no layer in {dir} has a .pr.md description, so there is nothing to publish"
        ));
    }
    let mut below = ("00-base".to_string(), stack.base.clone());
    for layer in &published {
        if !store.is_ancestor(&below.1, &layer.commit)? {
            return Err(format!(
                "{dir}/{0} does not contain {dir}/{1} ({2} is not an ancestor of {3}). Merge it first: run_tool(path=\"caos-std/merge\", arguments={{\"source-tree\": \"{dir}/{0}\", \"theirs\": \"{dir}/{1}\"}})",
                layer.name, below.0, below.1, layer.commit
            ));
        }
        below = (layer.name.clone(), layer.commit.clone());
    }
    let prefix = p.branch_prefix.clone().unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut layers = Vec::new();
    let mut base = trunk.clone();
    for layer in published {
        let branch = format!("{prefix}{}", layer.short);
        validate_branch(&branch).map_err(|error| format!("layer {}: {error}", layer.name))?;
        if branch == trunk {
            return Err(format!(
                "layer {} would publish to the trunk {trunk} itself; rename it or pass branch-prefix",
                layer.name
            ));
        }
        if !seen.insert(branch.clone()) {
            return Err(format!(
                "two layers share the branch name {branch}: layers differ only in their numbers, rename one"
            ));
        }
        let description = layer.description.as_ref().expect("published layers are described");
        layers.push(PlannedLayer {
            name: layer.name.clone(),
            path: format!("{dir}/{}", layer.name),
            branch: branch.clone(),
            base: base.clone(),
            title: description.title.clone(),
            body: description.body.clone(),
            commit: layer.commit.clone(),
        });
        base = branch;
    }
    Ok(Plan {
        repository,
        trunk,
        link: p.link,
        draft: p.draft,
        layers,
    })
}

fn short(oid: &Oid) -> &str {
    &oid.as_str()[..12]
}

fn dry_run_report(plan: &Plan) -> String {
    let mut text = format!(
        "dry run: nothing observed or pushed.\nrepository {}  trunk {}\n",
        plan.repository, plan.trunk
    );
    for layer in &plan.layers {
        text.push_str(&format!(
            "{}  {}  {}  base {}  PR title: {}\n",
            layer.name,
            layer.branch,
            short(&layer.commit),
            layer.base,
            layer.title
        ));
    }
    text
}

pub(super) fn pin_path(site: &CallSite<'_>) -> String {
    format!(
        "{}/stack-publish.json",
        paths::call_payload_dir(site.request.as_str(), site.round, &site.call.id)
    )
}

/// The pinned plan and publications of a started call, or None before the pin.
fn pinned(
    view: &Conversation<'_>,
    site: &CallSite<'_>,
) -> Result<Option<(Plan, Vec<PublicationRecord>)>, String> {
    if view
        .tool(site.request, site.round, &site.call.id)?
        .is_none()
    {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(&view.payload(&pin_path(site))?)
        .map_err(|_| "invalid stack-publish pin")?;
    let plan: Plan =
        serde_json::from_value(value["plan"].clone()).map_err(|_| "invalid stack-publish pin")?;
    let ids: Vec<String> = serde_json::from_value(value["publications"].clone())
        .map_err(|_| "invalid stack-publish pin")?;
    let records = ids
        .iter()
        .map(|id| {
            view.publication(id)?
                .ok_or_else(|| format!("pinned publication {id} is missing"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Some((plan, records)))
}

/// Pin the plan and every layer's publication in one append with the call's
/// start. A competing attempt's pin wins, as `publish_source::pin`.
fn pin_all(
    state: &mut progress::State,
    site: &CallSite<'_>,
    plan: &Plan,
    records: &[PublicationRecord],
) -> Result<Option<Vec<PublicationRecord>>, String> {
    let payload = canonical_payload_bytes(&json!({
        "plan": serde_json::to_value(plan).map_err(|e| e.to_string())?,
        "publications": records.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
    }))?;
    for _ in 0..32 {
        state.reload()?;
        let view = state.conversation()?;
        if view
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(None);
        }
        if let Some((_, records)) = pinned(&view, site)? {
            return Ok(Some(records));
        }
        let mut tool = site.stub(None);
        tool.status = CallStatus::Started;
        let mut transitions: Vec<Transition> = records
            .iter()
            .map(|record| Transition::PublicationPending {
                record: record.clone(),
            })
            .collect();
        transitions.push(Transition::ToolStart {
            record: tool,
            payloads: vec![("stack-publish.json".into(), payload.clone())],
        });
        let expected = state.head().clone();
        if matches!(
            state.try_append_many_at(&expected, transitions)?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(Some(records.to_vec()));
        }
    }
    Err("conversation kept moving while pinning the stack's publications".into())
}

/// Record one layer's push outcome as its publication's terminal transition.
fn settle(state: &mut progress::State, id: &str, outcome: &Outcome) -> Result<(), String> {
    for _ in 0..32 {
        state.reload()?;
        let view = state.conversation()?;
        let record = view
            .publication(id)?
            .ok_or("publication disappeared")?;
        if record.status != PublicationStatus::Pending {
            return Ok(());
        }
        let expected = state.head().clone();
        if matches!(
            state.try_append_at(
                &expected,
                Transition::PublicationTerminal {
                    publication: id.to_string(),
                    status: outcome.status,
                    evidence: outcome.evidence.clone(),
                    observed: outcome.observed.clone(),
                }
            )?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(());
        }
    }
    Err("conversation kept moving while recording a publication".into())
}

/// Complete the started call with its report.
fn complete(
    state: &mut progress::State,
    site: &CallSite<'_>,
    text: &str,
    is_error: bool,
) -> Result<(), String> {
    for _ in 0..32 {
        state.reload()?;
        if state
            .conversation()?
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(());
        }
        let block = result_block(&site.call.id, text, is_error);
        let stub = site.stub(None);
        let tool = completed_record(
            &stub,
            ToolResult::Complete {
                observation: observation_path(&stub),
                proposal: None,
            },
            None,
        );
        let expected = state.head().clone();
        if matches!(
            state.try_append_at(&expected, tool_complete_transition(tool, &block, Vec::new())?)?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(());
        }
    }
    Err("conversation kept moving while completing stack-publish".into())
}

/// The owner and repository of a github.com HTTPS URL, or None for any other
/// remote, which gets its branches pushed and no pull requests.
pub(super) fn github_repo(repository: &str) -> Option<(String, String)> {
    let (host, path) = git_locator::import::remote(repository).ok()?;
    if !(host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("github.com:443")) {
        return None;
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

struct Github {
    owner: String,
    repo: String,
    token: Option<String>,
}

/// A pull request's membership in a GitHub stack: the stack's number, this
/// PR's 1-based position from the bottom, and the stack's size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StackRef {
    pub number: u64,
    pub position: u64,
    pub size: u64,
}

struct Pr {
    number: u64,
    base: String,
    title: String,
    body: String,
    stack: Option<StackRef>,
}

fn pr_from(value: &Value) -> Option<Pr> {
    let stack = value["stack"].as_object().and_then(|stack| {
        Some(StackRef {
            number: stack.get("number")?.as_u64()?,
            position: stack.get("position")?.as_u64()?,
            size: stack.get("size")?.as_u64()?,
        })
    });
    Some(Pr {
        number: value["number"].as_u64()?,
        base: value["base"]["ref"].as_str()?.to_string(),
        title: value["title"].as_str().unwrap_or("").to_string(),
        body: value["body"].as_str().unwrap_or("").to_string(),
        stack,
    })
}

impl Github {
    fn repo_path(&self, rest: &str) -> String {
        format!("/repos/{}/{}{rest}", self.owner, self.repo)
    }

    /// One API call. Only a request that got no response is an error here; a
    /// status is an answer, and `expect` decides which ones are good.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(i32, Value), String> {
        let url = format!("{API}{path}");
        let verb = match method {
            "GET" => minreq::Method::Get,
            "POST" => minreq::Method::Post,
            "PATCH" => minreq::Method::Patch,
            other => return Err(format!("unsupported GitHub method {other}")),
        };
        let mut request = minreq::Request::new(verb, url)
            .with_header("Accept", "application/vnd.github+json")
        .with_header("X-GitHub-Api-Version", "2022-11-28")
        .with_header("User-Agent", "caos-stack-publish")
        .with_timeout(60);
        if let Some(token) = &self.token {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        if let Some(body) = body {
            request = request
                .with_header("Content-Type", "application/json")
                .with_body(body.to_string());
        }
        let response = request
            .send()
            .map_err(|error| format!("{method} {path}: no response: {error}"))?;
        let text = response
            .as_str()
            .map_err(|error| format!("{method} {path}: response is not UTF-8: {error}"))?;
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
        };
        Ok((response.status_code, value))
    }

    fn expect(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        ok: &[i32],
    ) -> Result<Value, String> {
        let (status, value) = self.call(method, path, body)?;
        if ok.contains(&status) {
            return Ok(value);
        }
        let mut why = value["message"].as_str().unwrap_or("").to_string();
        if let Some(errors) = value["errors"].as_array() {
            for error in errors {
                if let Some(message) = error["message"].as_str() {
                    why.push_str(": ");
                    why.push_str(message);
                }
            }
        }
        if status == 401 && self.token.is_none() {
            why.push_str(" (no github-token secret is granted to llm-step, so only public reads work)");
        }
        Err(format!("{method} {path}: HTTP {status} {why}")
            .trim_end()
            .to_string())
    }
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

/// A layer's PR after `pull_requests`: its number, the GitHub stack it is in,
/// and the base branch it still needs, when a stack membership deferred that.
type PrState = (u64, Option<StackRef>, Option<String>);

/// Find or open each layer's PR, bottom to top, and bring its text and base in
/// line with the plan. A base change for a PR that is in a GitHub stack is
/// deferred to the stack step, which has to unstack it first. Appends each
/// PR's state to its layer's report line.
fn pull_requests(
    github: &Github,
    plan: &Plan,
    lines: &mut [String],
) -> Result<Vec<PrState>, String> {
    let mut out = Vec::new();
    for (i, layer) in plan.layers.iter().enumerate() {
        let found = github.expect(
            "GET",
            &github.repo_path(&format!(
                "/pulls?head={}:{}&state=open",
                github.owner, layer.branch
            )),
            None,
            &[200],
        )?;
        let existing = found.as_array().and_then(|prs| prs.first()).and_then(pr_from);
        let (number, stack, deferred, note) = match existing {
            None => {
                let created = github.expect(
                    "POST",
                    &github.repo_path("/pulls"),
                    Some(&json!({
                        "title": layer.title, "head": layer.branch, "base": layer.base,
                        "body": layer.body, "draft": plan.draft,
                    })),
                    &[201],
                )?;
                let pr = pr_from(&created).ok_or("GitHub answered a PR create with no PR")?;
                (
                    pr.number,
                    None,
                    None,
                    format!("PR #{} created, base {}", pr.number, layer.base),
                )
            }
            Some(pr) => {
                let mut notes = Vec::new();
                let mut patch = serde_json::Map::new();
                if pr.title.trim() != layer.title.trim() || normalize(&pr.body) != normalize(&layer.body) {
                    patch.insert("title".into(), json!(layer.title));
                    patch.insert("body".into(), json!(layer.body));
                    notes.push("description updated from the .pr.md".to_string());
                }
                let mut deferred = None;
                if pr.base != layer.base {
                    if pr.stack.is_some() {
                        deferred = Some(layer.base.clone());
                        notes.push(format!(
                            "base {} -> {} waits for the stack step",
                            pr.base, layer.base
                        ));
                    } else {
                        patch.insert("base".into(), json!(layer.base));
                        notes.push(format!("base {} -> {}", pr.base, layer.base));
                    }
                }
                if !patch.is_empty() {
                    github.expect(
                        "PATCH",
                        &github.repo_path(&format!("/pulls/{}", pr.number)),
                        Some(&Value::Object(patch)),
                        &[200],
                    )?;
                }
                let note = if notes.is_empty() {
                    format!("PR #{} unchanged", pr.number)
                } else {
                    format!("PR #{}: {}", pr.number, notes.join(", "))
                };
                (pr.number, pr.stack, deferred, note)
            }
        };
        if let Some(line) = lines.get_mut(i) {
            line.push_str("  ");
            line.push_str(&note);
        }
        out.push((number, stack, deferred));
    }
    Ok(out)
}

/// What the stack step does, from where each PR is now. The PRs are bottom to
/// top; `position` is 1-based from the bottom.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StackPlan {
    Unchanged(u64),
    Create,
    Extend { stack: u64, add: Vec<u64> },
    Recreate { unstack: Vec<u64> },
}

pub(super) fn plan_stack(prs: &[(u64, Option<StackRef>)]) -> StackPlan {
    let in_order = |first: &StackRef, slice: &[(u64, Option<StackRef>)]| {
        slice.iter().enumerate().all(|(i, (_, stack))| {
            stack
                .as_ref()
                .is_some_and(|s| s.number == first.number && s.position == i as u64 + 1)
        })
    };
    match prs.first() {
        Some((_, Some(first))) => {
            if first.size == prs.len() as u64 && in_order(first, prs) {
                return StackPlan::Unchanged(first.number);
            }
            let k = first.size as usize;
            if k < prs.len()
                && in_order(first, &prs[..k])
                && prs[k..].iter().all(|(_, stack)| stack.is_none())
            {
                return StackPlan::Extend {
                    stack: first.number,
                    add: prs[k..].iter().map(|(number, _)| *number).collect(),
                };
            }
        }
        _ if prs.iter().all(|(_, stack)| stack.is_none()) => return StackPlan::Create,
        _ => {}
    }
    let mut unstack: Vec<u64> = prs
        .iter()
        .filter_map(|(_, stack)| stack.as_ref().map(|s| s.number))
        .collect();
    unstack.sort_unstable();
    unstack.dedup();
    StackPlan::Recreate { unstack }
}

/// Link the PRs as a GitHub stack, or bring the one they are in up to date.
fn link_stack(github: &Github, plan: &Plan, prs: &[PrState]) -> Result<String, String> {
    let numbers: Vec<u64> = prs.iter().map(|(number, _, _)| *number).collect();
    let deferred: Vec<(u64, String)> = prs
        .iter()
        .filter_map(|(number, _, base)| base.clone().map(|base| (*number, base)))
        .collect();
    let stacks: Vec<u64> = {
        let mut all: Vec<u64> = prs
            .iter()
            .filter_map(|(_, stack, _)| stack.as_ref().map(|s| s.number))
            .collect();
        all.sort_unstable();
        all.dedup();
        all
    };
    if !plan.link {
        if deferred.is_empty() {
            return Ok("stack: not linked (link=false)".into());
        }
        return Err(format!(
            "the base of PR(s) {:?} was not changed: they are in GitHub stack(s) {:?}, which refuse base changes. Publish with link=true to restack them.",
            deferred.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            stacks
        ));
    }
    let refs: Vec<(u64, Option<StackRef>)> = prs
        .iter()
        .map(|(number, stack, _)| (*number, stack.clone()))
        .collect();
    let mut decision = plan_stack(&refs);
    if !deferred.is_empty() && !matches!(decision, StackPlan::Recreate { .. }) {
        decision = StackPlan::Recreate {
            unstack: stacks.clone(),
        };
    }
    if numbers.len() < 2 && !matches!(decision, StackPlan::Recreate { .. }) {
        return Ok("stack: one layer, nothing to link".into());
    }
    match decision {
        StackPlan::Unchanged(number) => Ok(format!("stack {number}: unchanged")),
        StackPlan::Create => {
            let created = github.expect(
                "POST",
                &github.repo_path("/stacks"),
                Some(&json!({"pull_requests": numbers})),
                &[201],
            )?;
            Ok(format!("stack {}: linked {:?}", created["number"], numbers))
        }
        StackPlan::Extend { stack, add } => {
            github.expect(
                "POST",
                &github.repo_path(&format!("/stacks/{stack}/add")),
                Some(&json!({"pull_requests": add})),
                &[200],
            )?;
            Ok(format!("stack {stack}: added {add:?}"))
        }
        StackPlan::Recreate { unstack } => {
            for stack in &unstack {
                github.expect(
                    "POST",
                    &github.repo_path(&format!("/stacks/{stack}/unstack")),
                    None,
                    &[200, 204],
                )?;
            }
            for (number, base) in &deferred {
                github.expect(
                    "PATCH",
                    &github.repo_path(&format!("/pulls/{number}")),
                    Some(&json!({"base": base})),
                    &[200],
                )?;
            }
            if numbers.len() < 2 {
                return Ok(format!("stack: unstacked {unstack:?}; one layer, nothing to link"));
            }
            let created = github.expect(
                "POST",
                &github.repo_path("/stacks"),
                Some(&json!({"pull_requests": numbers})),
                &[201],
            )?;
            Ok(format!(
                "stack {}: recreated with {:?} (unstacked {:?})",
                created["number"], numbers, unstack
            ))
        }
    }
}

pub(super) fn execute(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let p = match parameters(site.call) {
        Ok(p) => p,
        Err(error) => return site.fail(state, &error),
    };
    // Bound before the match: a temporary in the scrutinee would hold the
    // conversation borrowed through every arm, and the arms write to it.
    let already = pinned(&state.conversation()?, site)?;
    let (plan, records) = match already {
        Some(pinned) => pinned,
        None => {
            // Everything that reads the conversation, before anything that
            // writes it or reaches the remote.
            let prepared = {
                let view = state.conversation()?;
                prepare(&view, state.store(), &p).and_then(|plan| {
                    let bases = plan
                        .layers
                        .iter()
                        .map(|layer| view.reference_start(&layer.path))
                        .collect::<Result<Vec<_>, String>>()?;
                    Ok((plan, bases, view.identity()?.id))
                })
            };
            let (plan, bases, conversation) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    return site.fail(
                        state,
                        &format!("stack-publish refused; nothing was pushed: {error}"),
                    )
                }
            };
            if p.dry_run {
                let block = result_block(&site.call.id, &dry_run_report(&plan), false);
                return site.complete(state, block, None, None, None);
            }
            let token_file = import_source::token_file(&plan.repository);
            let token = token_file
                .map(fs::read_to_string)
                .transpose()
                .map_err(|_| "reading GitHub token")?;
            let token = token.as_deref().map(str::trim_end);
            let key = publish_source::invocation(&conversation, site)?[..32].to_string();
            let mut records = Vec::new();
            for (layer, base) in plan.layers.iter().zip(bases) {
                let old = match git_locator::publish::read_branch(&plan.repository, &layer.branch, token)
                    .and_then(|head| head.map(|h| Oid::parse(&h, "remote head")).transpose())
                {
                    Ok(old) => old,
                    Err(error) => {
                        return site.fail(
                            state,
                            &format!("{error} for branch {}; nothing was pushed", layer.branch),
                        )
                    }
                };
                let descriptor = Descriptor {
                    source_base: base.clone(),
                    source_head: layer.commit.clone(),
                    target_base: base,
                    policy: if p.force { "force" } else { "preserve" }.into(),
                    implementation: "caos/server-push".into(),
                    commit_policy: "preserve".into(),
                };
                let refname = format!("refs/heads/{}", layer.branch);
                let id = ids::publication_id(
                    &conversation,
                    &key,
                    &ids::projection_id(&descriptor.to_value())?,
                    &layer.commit,
                    &plan.repository,
                    &refname,
                    old.as_ref(),
                )?;
                records.push(PublicationRecord {
                    id,
                    key: key.clone(),
                    descriptor,
                    planned_head: layer.commit.clone(),
                    repository: plan.repository.clone(),
                    refname,
                    expected_old: old,
                    source_tree_name: layer.path.clone(),
                    status: PublicationStatus::Pending,
                    evidence: None,
                    observed: None,
                });
            }
            match pin_all(state, site, &plan, &records)? {
                Some(records) => (plan, records),
                None => return Ok(()),
            }
        }
    };

    let token_file = import_source::token_file(&plan.repository);
    let token = token_file
        .map(fs::read_to_string)
        .transpose()
        .map_err(|_| "reading GitHub token")?;
    let token = token.as_deref().map(str::trim_end);
    let mut lines: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut pushed = 0;
    for (layer, record) in plan.layers.iter().zip(&records) {
        let (status, evidence) = if record.status == PublicationStatus::Pending {
            let observe = || {
                git_locator::publish::read_branch(&plan.repository, &layer.branch, token)
                    .and_then(|head| head.map(|h| Oid::parse(&h, "remote head")).transpose())
            };
            let outcome =
                publish_source::send(record, &layer.branch, p.allow_ignored, token_file, observe)?;
            settle(state, &record.id, &outcome)?;
            (outcome.status, outcome.evidence)
        } else {
            (
                record.status,
                record.evidence.clone().unwrap_or(Evidence {
                    kind: "recorded".into(),
                    diagnostic: None,
                }),
            )
        };
        let what = match status {
            PublicationStatus::Complete if record.expected_old.as_ref() == Some(&record.planned_head) => {
                "unchanged"
            }
            PublicationStatus::Complete if record.expected_old.is_none() => "created",
            PublicationStatus::Complete => "pushed (fast-forward)",
            PublicationStatus::Conflict => "REJECTED",
            PublicationStatus::Uncertain => "UNCERTAIN",
            PublicationStatus::Pending => "pending",
        };
        let mut line = format!(
            "{}  {}  {}  {what}",
            layer.name,
            layer.branch,
            short(&layer.commit)
        );
        if status != PublicationStatus::Complete {
            let why = evidence.diagnostic.clone().unwrap_or_else(|| evidence.kind.clone());
            line.push_str(&format!(" ({why})"));
            lines.push(line);
            problems.push(format!(
                "the push of {} to {} was {}: {why}. The layers above it were not pushed.",
                layer.name,
                layer.branch,
                what.to_lowercase()
            ));
            break;
        }
        lines.push(line);
        pushed += 1;
    }
    for layer in plan.layers.iter().skip(pushed + 1) {
        lines.push(format!(
            "{}  {}  {}  not attempted",
            layer.name,
            layer.branch,
            short(&layer.commit)
        ));
    }

    let mut stack_line = String::new();
    if pushed == plan.layers.len() {
        match github_repo(&plan.repository) {
            None => {
                stack_line = format!(
                    "{} is not on github.com: branches pushed, no pull requests opened",
                    plan.repository
                )
            }
            Some((owner, repo)) => {
                let github = Github {
                    owner,
                    repo,
                    token: token.map(str::to_string),
                };
                match pull_requests(&github, &plan, &mut lines) {
                    Ok(prs) => match link_stack(&github, &plan, &prs) {
                        Ok(line) => stack_line = line,
                        Err(error) => problems.push(error),
                    },
                    Err(error) => problems.push(format!(
                        "the branches are pushed, but GitHub refused while handling the pull requests: {error}"
                    )),
                }
            }
        }
    }

    let mut text = format!("repository {}  trunk {}\n", plan.repository, plan.trunk);
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    if !stack_line.is_empty() {
        text.push_str(&stack_line);
        text.push('\n');
    }
    for problem in &problems {
        text.push_str("FAILED: ");
        text.push_str(problem);
        text.push('\n');
    }
    complete(state, site, &text, !problems.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_take_the_first_line_as_title() {
        let d = parse_description("\n# Add the parser\n\nWhy it exists,\nover two lines.\n\n").unwrap();
        assert_eq!(d.title, "Add the parser");
        assert_eq!(d.body, "Why it exists,\nover two lines.");
        let d = parse_description("Just a title").unwrap();
        assert_eq!((d.title.as_str(), d.body.as_str()), ("Just a title", ""));
        assert!(parse_description("\n\n").is_err());
        assert!(parse_description("#\nbody").is_err());
    }

    #[test]
    fn layer_names_are_two_digits_a_dash_and_a_name() {
        assert_eq!(layer_name("01-parser"), Some(("01", "parser")));
        assert_eq!(layer_name("00-base"), Some(("00", "base")));
        assert_eq!(layer_name("00-base.source.json"), Some(("00", "base.source.json")));
        assert_eq!(layer_name("1-parser"), None);
        assert_eq!(layer_name("01-"), None);
        assert_eq!(layer_name("notes.txt"), None);
    }

    #[test]
    fn only_github_remotes_get_pull_requests() {
        assert_eq!(
            github_repo("https://github.com/Metta-AI/caos"),
            Some(("Metta-AI".into(), "caos".into()))
        );
        assert_eq!(
            github_repo("https://GitHub.com/owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
        assert_eq!(github_repo("https://example.com/owner/repo"), None);
        assert_eq!(github_repo("https://github.com/owner"), None);
        assert_eq!(github_repo("https://github.com/owner/repo/extra"), None);
    }

    #[test]
    fn trunk_comes_from_the_import_when_it_names_a_branch() {
        let p = |requested: Option<&str>, default: Option<&str>| Provenance {
            repository: None,
            requested: requested.map(str::to_string),
            default_branch: default.map(str::to_string),
        };
        assert_eq!(infer_trunk(&p(Some("main"), None)), Some("main".into()));
        assert_eq!(infer_trunk(&p(Some("refs/heads/dev"), None)), Some("dev".into()));
        assert_eq!(infer_trunk(&p(Some("refs/tags/v1"), Some("main"))), None);
        assert_eq!(infer_trunk(&p(Some(&"a".repeat(40)), Some("main"))), None);
        assert_eq!(infer_trunk(&p(None, Some("main"))), Some("main".into()));
        assert_eq!(infer_trunk(&p(None, None)), None);
    }

    fn at(number: u64, stack: u64, position: u64, size: u64) -> (u64, Option<StackRef>) {
        (
            number,
            Some(StackRef {
                number: stack,
                position,
                size,
            }),
        )
    }

    #[test]
    fn stack_decisions() {
        assert_eq!(plan_stack(&[(1, None), (2, None)]), StackPlan::Create);
        assert_eq!(
            plan_stack(&[at(1, 9, 1, 2), at(2, 9, 2, 2)]),
            StackPlan::Unchanged(9)
        );
        assert_eq!(
            plan_stack(&[at(1, 9, 1, 2), at(2, 9, 2, 2), (3, None)]),
            StackPlan::Extend {
                stack: 9,
                add: vec![3]
            }
        );
        // Out of order, a foreign PR in the stack, two stacks, or a stacked PR
        // above an unstacked one: unstack and recreate.
        assert_eq!(
            plan_stack(&[at(1, 9, 2, 2), at(2, 9, 1, 2)]),
            StackPlan::Recreate { unstack: vec![9] }
        );
        assert_eq!(
            plan_stack(&[at(1, 9, 1, 3), at(2, 9, 2, 3)]),
            StackPlan::Recreate { unstack: vec![9] }
        );
        assert_eq!(
            plan_stack(&[at(1, 9, 1, 1), at(2, 4, 1, 1)]),
            StackPlan::Recreate { unstack: vec![4, 9] }
        );
        assert_eq!(
            plan_stack(&[(1, None), at(2, 9, 1, 1)]),
            StackPlan::Recreate { unstack: vec![9] }
        );
    }

    #[test]
    fn parameters_are_strings_with_defaults() {
        let call = |input: Value| Call {
            id: "a".into(),
            name: "run_tool".into(),
            input,
        };
        let p = parameters(&call(json!({"stack": "sources/repo/"}))).unwrap();
        assert_eq!(p.stack, "sources/repo");
        assert!(p.link && !p.force && !p.dry_run && !p.draft);
        assert_eq!(p.branch_prefix, None);
        let p = parameters(&call(json!({
            "stack": "sources/repo", "link": "false", "dry-run": "true",
            "branch-prefix": "me/", "repository": "", "trunk": "main"
        })))
        .unwrap();
        assert!(!p.link && p.dry_run);
        assert_eq!(p.branch_prefix.as_deref(), Some("me/"));
        assert_eq!(p.repository, None);
        assert_eq!(p.trunk.as_deref(), Some("main"));
        for input in [
            json!({}),
            json!({"stack": ""}),
            json!({"stack": ".caos/x"}),
            json!({"stack": "sources/repo", "extra": "x"}),
            json!({"stack": "sources/repo", "repository": "git@github.com:a/b"}),
            json!({"stack": "sources/repo", "trunk": "bad..name"}),
            json!({"stack": "sources/repo", "force": "maybe"}),
        ] {
            assert!(parameters(&call(input.clone())).is_err(), "{input}");
        }
    }
}
