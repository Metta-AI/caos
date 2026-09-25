//! Read a scoped plan and return a replacement stack tree.
use crate::rebase_plan;
use conversation_protocol::v3::{CommitInfo, Mode, ObjectStore, Oid, Snapshot, TreeBuilder};
use std::fmt::Write;
use std::process::Command;

#[derive(Debug)]
pub struct Proposal {
    pub tree: Oid,
    pub report: String,
}

fn blob(store: &dyn ObjectStore, root: &Oid, path: &str) -> Result<Vec<u8>, String> {
    let entry = Snapshot::new(store, root.clone())
        .entry(path)?
        .ok_or_else(|| format!("missing file {path}"))?;
    if entry.mode != Mode::Blob {
        return Err(format!("{path} must be a regular file"));
    }
    store.read_blob(&entry.oid).map_err(String::from)
}

pub fn propose(store: &mut dyn ObjectStore, input: &Oid) -> Result<Proposal, String> {
    let plan = String::from_utf8(blob(store, input, "rebase/plan")?)
        .map_err(|_| "rebase/plan must be UTF-8")?;
    let outcome = rebase_plan::run(&mut Backend { store, input }, &plan)?;
    apply(store, input, &plan, outcome)
}

fn numbered(name: &str, mode: Mode) -> bool {
    let digits = name.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0
        && ((mode == Mode::Commit && name[digits..].starts_with('-'))
            || (mode == Mode::Blob && name[digits..] == *".base"))
}

fn apply(
    store: &mut dyn ObjectStore,
    input: &Oid,
    plan: &str,
    outcome: rebase_plan::Outcome,
) -> Result<Proposal, String> {
    let mut replacement = TreeBuilder::from(Some(input.clone()));
    let mut report = String::new();
    if let Some(conflict) = outcome.conflict {
        replacement.put_oid("rebase/work", Mode::Commit, conflict.draft.clone());
        replacement.put("rebase/conflicts", Mode::Blob, conflict.report);
        writeln!(report, "Conflict at rebase/plan line {}", conflict.line).unwrap();
        writeln!(report, "Output tip: {}", conflict.parent).unwrap();
        writeln!(report, "Draft: rebase/work ({})", conflict.draft).unwrap();
        writeln!(report, "Git report: rebase/conflicts").unwrap();
        writeln!(report,
            "Resolve rebase/work, get its commit R, replace the failed pick with pick={}..R, then run the plan again.",
            conflict.parent).unwrap();
    } else {
        let entries = store.read_tree(input).map_err(String::from)?;
        for entry in &entries {
            if numbered(&entry.name, entry.mode) {
                replacement.delete(&entry.name);
            }
        }
        for (number, layer) in outcome.layers.iter().enumerate() {
            let base_name = format!("{number:02}.base");
            for name in [&layer.name, &base_name] {
                if entries
                    .iter()
                    .any(|entry| entry.name == *name && !numbered(&entry.name, entry.mode))
                {
                    return Err(format!(
                        "output {name} would replace an ordinary stack file"
                    ));
                }
            }
            replacement.put_oid(&layer.name, Mode::Commit, layer.tip.clone());
            replacement.put(&base_name, Mode::Blob, layer.base.encode_line());
            writeln!(report, "{} {}", layer.name, layer.tip).unwrap();
        }
        replacement.delete("rebase");
        writeln!(report, "\nCompleted plan:\n{plan}").unwrap();
    }
    Ok(Proposal {
        tree: replacement.build(store)?,
        report,
    })
}

struct Backend<'s> {
    store: &'s mut dyn ObjectStore,
    input: &'s Oid,
}

impl rebase_plan::Backend for Backend<'_> {
    fn read_commit(&mut self, oid: &Oid) -> Result<CommitInfo, String> {
        self.store.read_commit(oid).map_err(String::from)
    }

    fn read_message(&mut self, path: &str) -> Result<Vec<u8>, String> {
        blob(self.store, self.input, path)
    }

    fn merge_trees(
        &mut self,
        base: &Oid,
        ours: &Oid,
        theirs: &Oid,
    ) -> Result<rebase_plan::Merge, String> {
        let output = Command::new("caos")
            .args([
                "git-merge-tree",
                base.as_str(),
                ours.as_str(),
                theirs.as_str(),
            ])
            .output()
            .map_err(|e| format!("running merge-tree: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "merge-tree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("invalid merge-tree result: {e}"))?;
        let tree = Oid::parse(
            value["tree"].as_str().ok_or("merge-tree lacks tree")?,
            "merged tree",
        )?;
        let report = value["conflicts"]
            .as_str()
            .ok_or("merge-tree lacks report")?;
        Ok(rebase_plan::Merge {
            tree,
            conflicts: (!report.is_empty()).then(|| report.as_bytes().to_vec()),
        })
    }

    fn commit_tree(&mut self, commit: &CommitInfo) -> Result<Oid, String> {
        self.store.write_commit(commit).map_err(String::from)
    }
}

#[cfg(test)]
#[path = "rebase_tests.rs"]
mod tests;
