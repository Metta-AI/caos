//! Numbered stack pointers and bases; ordinary files stay outside the stack.
use conversation_protocol::v3::paths;
use conversation_protocol::v3::tree::canonical_tree_order;
use conversation_protocol::v3::{Mode, ObjectStore, Oid, Snapshot, TreeEntry};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    pub number: usize,
    pub name: String,
    pub commit: Oid,
    pub base: Oid,
}

#[derive(Clone, Debug)]
pub struct Stack {
    pub entries: Vec<TreeEntry>,
    pub layers: Vec<Layer>,
}

pub fn directory(store: &dyn ObjectStore, root: &Oid, path: &str) -> Result<Oid, String> {
    paths::validate_source_tree_name(path)?;
    match Snapshot::new(store, root.clone()).entry(path)? {
        Some(entry) if entry.mode == Mode::Tree => Ok(entry.oid),
        _ => Err(format!("{path} must be a conversation directory")),
    }
}

pub fn read_stack(store: &dyn ObjectStore, root: &Oid, path: &str) -> Result<Stack, String> {
    let directory = directory(store, root, path)?;
    let entries = store.read_tree(&directory).map_err(String::from)?;
    let layers = validate_stack(store, &numbered_entries(&entries))?;
    Ok(Stack { entries, layers })
}

pub fn numbered(name: &str) -> bool {
    let digits = name.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && (name[digits..] == *".base" || name[digits..].starts_with('-'))
}

pub fn numbered_entries(entries: &[TreeEntry]) -> Vec<TreeEntry> {
    let mut entries: Vec<_> = entries
        .iter()
        .filter(|entry| numbered(&entry.name))
        .cloned()
        .collect();
    canonical_tree_order(&mut entries);
    entries
}

pub fn validate_stack(
    store: &dyn ObjectStore,
    entries: &[TreeEntry],
) -> Result<Vec<Layer>, String> {
    let mut pairs: BTreeMap<usize, (Option<&TreeEntry>, Option<Oid>)> = BTreeMap::new();
    for entry in entries {
        let n = entry.name.bytes().take_while(u8::is_ascii_digit).count();
        let number: usize = entry.name[..n]
            .parse()
            .map_err(|_| format!("invalid stack entry {:?}", entry.name))?;
        let prefix = format!("{number:02}");
        if entry.name[..n] != prefix {
            return Err(format!(
                "stack numbers must use {prefix}, not {:?}",
                &entry.name[..n]
            ));
        }
        let pair = pairs.entry(number).or_default();
        if entry.name[n..] == *".base" {
            if entry.mode != Mode::Blob || pair.1.is_some() {
                return Err(format!("{} must be one regular base file", entry.name));
            }
            let bytes = store.read_blob(&entry.oid).map_err(String::from)?;
            let text =
                std::str::from_utf8(&bytes).map_err(|_| format!("{} must be UTF-8", entry.name))?;
            pair.1 = Some(Oid::parse(text.trim(), "layer base")?);
        } else if entry.name[n..].starts_with('-') && entry.name.len() > n + 1 {
            paths::validate_component(&entry.name)?;
            if entry.name.chars().any(char::is_whitespace) {
                return Err(format!("invalid layer name: {}", entry.name));
            }
            if entry.mode != Mode::Commit || pair.0.replace(entry).is_some() {
                return Err(format!("layer {prefix} must have one source gitlink"));
            }
        } else {
            return Err(format!(
                "unrecognized numbered stack entry {:?}",
                entry.name
            ));
        }
    }
    if pairs.is_empty() {
        return Err("stack needs 00-name and 00.base".into());
    }
    pairs
        .into_iter()
        .enumerate()
        .map(|(expected, (number, (entry, base)))| {
            if number != expected || entry.is_none() || base.is_none() {
                return Err(format!(
                    "stack needs consecutive layer/base pairs; missing pair {expected:02}"
                ));
            }
            let entry = entry.unwrap();
            Ok(Layer {
                number,
                name: entry.name.clone(),
                commit: entry.oid.clone(),
                base: base.unwrap(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversation_protocol::v3::{MemoryStore, TreeBuilder};

    fn oid(byte: char) -> Oid {
        Oid::parse(&byte.to_string().repeat(40), "test").unwrap()
    }

    fn root(store: &mut MemoryStore) -> Oid {
        let mut b = TreeBuilder::from(None);
        // These commits intentionally aren't loaded into the store.
        b.put_oid("feature/00-work", Mode::Commit, oid('a'));
        b.put("feature/00.base", Mode::Blob, oid('b').encode_line());
        b.put("feature/notes", Mode::Blob, b"notes".to_vec());
        b.put("feature/2026.md", Mode::Blob, b"dated notes".to_vec());
        b.build(store).unwrap()
    }

    #[test]
    fn stack_snapshot_reads_pointers_without_fetching_source_commits() {
        let mut store = MemoryStore::new();
        let root = root(&mut store);
        let stack = read_stack(&store, &root, "feature").unwrap();
        assert_eq!(stack.layers[0].commit, oid('a'));
        assert_eq!(stack.layers[0].base, oid('b'));
        assert_eq!(stack.layers.len(), 1);
    }

    #[test]
    fn malformed_stack_pairs_are_rejected() {
        for bad in ["01.base", "0.base", "00-second", "02-work"] {
            let mut store = MemoryStore::new();
            let root = root(&mut store);
            let mut changed = TreeBuilder::from(Some(root));
            if bad.ends_with("base") {
                changed.delete("feature/00.base");
                changed.put(
                    &format!("feature/{bad}"),
                    Mode::Blob,
                    oid('b').encode_line(),
                );
            } else {
                changed.put_oid(&format!("feature/{bad}"), Mode::Commit, oid('c'));
            }
            let root = changed.build(&mut store).unwrap();
            assert!(read_stack(&store, &root, "feature").is_err(), "{bad}");
        }
    }
}
