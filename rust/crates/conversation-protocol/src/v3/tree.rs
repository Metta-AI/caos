use std::cell::{Ref, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;

use super::oid::{empty_tree, hex_lower, hex_nibble, object_id, ObjectKind, Oid};
use super::paths::validate_tree_path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Blob,
    Executable,
    Tree,
}

impl Mode {
    pub fn octal(self) -> &'static str {
        match self {
            Mode::Blob => "100644",
            Mode::Executable => "100755",
            Mode::Tree => "40000",
        }
    }

    pub fn parse(octal: &str) -> Result<Mode, String> {
        match octal {
            "100644" => Ok(Mode::Blob),
            "100755" => Ok(Mode::Executable),
            "40000" | "040000" => Ok(Mode::Tree),
            _ => Err(format!("unsupported git tree mode {octal:?}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub mode: Mode,
    pub oid: Oid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: i64,
    pub offset: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    /// Headers following the committer, including continuation lines and LFs.
    /// Ordinary code commits may carry signatures, encoding, and merge tags.
    pub extra_headers: Vec<u8>,
    pub message: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    Missing(Oid),
    WrongType { oid: Oid, expected: &'static str },
    Other(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Missing(oid) => write!(formatter, "missing object {oid}"),
            StoreError::WrongType { oid, expected } => {
                write!(formatter, "object {oid} is not a {expected}")
            }
            StoreError::Other(message) => message.fmt(formatter),
        }
    }
}

impl From<StoreError> for String {
    fn from(error: StoreError) -> String {
        error.to_string()
    }
}

pub trait ObjectStore {
    fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>, StoreError>;
    fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>, StoreError>;
    fn read_commit(&self, oid: &Oid) -> Result<CommitInfo, StoreError>;
    fn write_blob(&mut self, bytes: &[u8]) -> Result<Oid, StoreError>;
    fn write_tree(&mut self, entries: &[TreeEntry]) -> Result<Oid, StoreError>;
    fn write_commit(&mut self, commit: &CommitInfo) -> Result<Oid, StoreError>;
}

#[cfg(any(test, feature = "memory-store"))]
#[macro_export]
macro_rules! delegate_object_store {
    ($type:ty, $field:ident) => {
        impl $crate::v3::ObjectStore for $type {
            fn read_blob(&self, oid: &$crate::v3::Oid) -> Result<Vec<u8>, $crate::v3::StoreError> {
                self.$field.read_blob(oid)
            }

            fn read_tree(
                &self,
                oid: &$crate::v3::Oid,
            ) -> Result<Vec<$crate::v3::TreeEntry>, $crate::v3::StoreError> {
                self.$field.read_tree(oid)
            }

            fn read_commit(
                &self,
                oid: &$crate::v3::Oid,
            ) -> Result<$crate::v3::CommitInfo, $crate::v3::StoreError> {
                self.$field.read_commit(oid)
            }

            fn write_blob(
                &mut self,
                bytes: &[u8],
            ) -> Result<$crate::v3::Oid, $crate::v3::StoreError> {
                self.$field.write_blob(bytes)
            }

            fn write_tree(
                &mut self,
                entries: &[$crate::v3::TreeEntry],
            ) -> Result<$crate::v3::Oid, $crate::v3::StoreError> {
                self.$field.write_tree(entries)
            }

            fn write_commit(
                &mut self,
                commit: &$crate::v3::CommitInfo,
            ) -> Result<$crate::v3::Oid, $crate::v3::StoreError> {
                self.$field.write_commit(commit)
            }
        }
    };
}

pub fn canonical_tree_order(entries: &mut [TreeEntry]) {
    entries.sort_by(compare_entries);
}

fn read_tree_or_empty(store: &dyn ObjectStore, oid: &Oid) -> Result<Vec<TreeEntry>, String> {
    match store.read_tree(oid) {
        Ok(entries) => Ok(entries),
        Err(StoreError::Missing(_)) if oid == &empty_tree() => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn is_canonical_tree_order(entries: &[TreeEntry]) -> bool {
    let mut names = HashSet::new();
    entries
        .iter()
        .all(|entry| names.insert(entry.name.as_str()))
        && entries
            .windows(2)
            .all(|pair| compare_entries(&pair[0], &pair[1]) == Ordering::Less)
}

pub struct Snapshot<'s> {
    store: &'s dyn ObjectStore,
    root: Oid,
    trees: RefCell<HashMap<Oid, Vec<TreeEntry>>>,
}

impl<'s> Snapshot<'s> {
    pub fn new(store: &'s dyn ObjectStore, root: Oid) -> Snapshot<'s> {
        Snapshot {
            store,
            root,
            trees: RefCell::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Oid {
        &self.root
    }

    pub fn entry(&self, path: &str) -> Result<Option<TreeEntry>, String> {
        validate_tree_path(path)?;
        let components: Vec<&str> = path.split('/').collect();
        let mut tree = self.root.clone();
        for (index, component) in components.iter().enumerate() {
            let entry = self
                .tree_entries(&tree)?
                .iter()
                .find(|entry| entry.name == *component)
                .cloned();
            let Some(entry) = entry else {
                return Ok(None);
            };
            if index + 1 == components.len() {
                return Ok(Some(entry));
            }
            if entry.mode != Mode::Tree {
                let prefix = components[..=index].join("/");
                return Err(format!("path {path}: {prefix} is a file"));
            }
            tree = entry.oid;
        }
        unreachable!()
    }

    pub fn read(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        match self.entry(path)? {
            None => Ok(None),
            Some(entry) if entry.mode == Mode::Tree => Err(format!("path {path}: is a directory")),
            Some(entry) => self
                .store
                .read_blob(&entry.oid)
                .map(Some)
                .map_err(String::from),
        }
    }

    pub(crate) fn blob(&self, oid: &Oid) -> Result<Vec<u8>, String> {
        self.store.read_blob(oid).map_err(String::from)
    }

    pub fn list(&self, dir: &str) -> Result<Vec<TreeEntry>, String> {
        if dir.is_empty() {
            return self
                .tree_entries(&self.root)
                .map(|entries| entries.to_vec());
        }
        match self.entry(dir)? {
            None => Err(format!("directory {dir:?} does not exist")),
            Some(entry) if entry.mode != Mode::Tree => Err(format!("path {dir}: is a file")),
            Some(entry) => self
                .tree_entries(&entry.oid)
                .map(|entries| entries.to_vec()),
        }
    }

    pub fn exists(&self, path: &str) -> Result<bool, String> {
        self.entry(path).map(|entry| entry.is_some())
    }

    fn tree_entries(&self, oid: &Oid) -> Result<Ref<'_, [TreeEntry]>, String> {
        if !self.trees.borrow().contains_key(oid) {
            let entries = read_tree_or_empty(self.store, oid)?;
            self.trees.borrow_mut().insert(oid.clone(), entries);
        }
        Ok(Ref::map(self.trees.borrow(), |trees| {
            trees.get(oid).expect("tree was inserted").as_slice()
        }))
    }
}

pub struct TreeBuilder {
    base: Option<Oid>,
    operations: Vec<Operation>,
}

impl TreeBuilder {
    pub fn from(base: Option<Oid>) -> TreeBuilder {
        TreeBuilder {
            base,
            operations: Vec::new(),
        }
    }

    pub fn put(&mut self, path: &str, mode: Mode, bytes: Vec<u8>) {
        debug_assert_ne!(mode, Mode::Tree);
        self.operations.push(Operation::Put {
            path: path.to_string(),
            mode,
            value: PutValue::Bytes(bytes),
        });
    }

    pub fn put_oid(&mut self, path: &str, mode: Mode, oid: Oid) {
        self.operations.push(Operation::Put {
            path: path.to_string(),
            mode,
            value: PutValue::Oid(oid),
        });
    }

    pub fn delete(&mut self, path: &str) {
        self.operations.push(Operation::Delete(path.to_string()));
    }

    pub fn build(self, store: &mut dyn ObjectStore) -> Result<Oid, String> {
        if self.operations.is_empty() {
            return Ok(self.base.unwrap_or_else(empty_tree));
        }
        for operation in &self.operations {
            validate_tree_path(operation.path())?;
        }
        let last_operations: HashMap<String, usize> = self
            .operations
            .iter()
            .enumerate()
            .map(|(index, operation)| (operation.path().to_string(), index))
            .collect();
        let explicitly_deleted: HashSet<String> = self
            .operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::Delete(path) => Some(path.clone()),
                Operation::Put { .. } => None,
            })
            .collect();
        let mut root = Directory::from_oid(self.base);
        for (index, operation) in self.operations.into_iter().enumerate() {
            if last_operations.get(operation.path()) != Some(&index) {
                continue;
            }
            match operation {
                Operation::Put { path, mode, value } => {
                    let components: Vec<&str> = path.split('/').collect();
                    put_path(
                        &mut root,
                        store,
                        &components,
                        &path,
                        mode,
                        value,
                        &explicitly_deleted,
                    )?;
                }
                Operation::Delete(path) => {
                    let components: Vec<&str> = path.split('/').collect();
                    delete_path(&mut root, store, &components)?;
                }
            }
        }
        Ok(write_directory(root, store, true)?.unwrap_or_else(empty_tree))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub before: Option<(Mode, Oid)>,
    pub after: Option<(Mode, Oid)>,
}

pub fn diff(
    store: &dyn ObjectStore,
    before: Option<&Oid>,
    after: &Oid,
) -> Result<Vec<Change>, String> {
    let mut changes = Vec::new();
    match before {
        Some(before) if before == after => {}
        Some(before) => diff_trees(store, Some(before), Some(after), "", &mut changes)?,
        None => diff_trees(store, None, Some(after), "", &mut changes)?,
    }
    changes.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    Ok(changes)
}

enum Operation {
    Put {
        path: String,
        mode: Mode,
        value: PutValue,
    },
    Delete(String),
}

impl Operation {
    fn path(&self) -> &str {
        match self {
            Operation::Put { path, .. } | Operation::Delete(path) => path,
        }
    }
}

enum PutValue {
    Bytes(Vec<u8>),
    Oid(Oid),
}

enum Node {
    Blob { mode: Mode, value: PutValue },
    Tree(Directory),
}

struct Directory {
    oid: Option<Oid>,
    loaded: bool,
    dirty: bool,
    entries: HashMap<String, Node>,
}

impl Directory {
    fn from_oid(oid: Option<Oid>) -> Directory {
        let loaded = oid.is_none();
        Directory {
            oid,
            loaded,
            dirty: false,
            entries: HashMap::new(),
        }
    }

    fn empty() -> Directory {
        Directory::from_oid(None)
    }

    fn load(&mut self, store: &dyn ObjectStore) -> Result<(), String> {
        if self.loaded {
            return Ok(());
        }
        let oid = self.oid.as_ref().expect("unloaded directory has an oid");
        let entries = read_tree_or_empty(store, oid)?;
        if !is_canonical_tree_order(&entries) {
            return Err(format!("tree {oid} is not in canonical order"));
        }
        for entry in entries {
            let node = if entry.mode == Mode::Tree {
                Node::Tree(Directory::from_oid(Some(entry.oid)))
            } else {
                Node::Blob {
                    mode: entry.mode,
                    value: PutValue::Oid(entry.oid),
                }
            };
            self.entries.insert(entry.name, node);
        }
        self.loaded = true;
        Ok(())
    }
}

fn compare_entries(left: &TreeEntry, right: &TreeEntry) -> Ordering {
    let left_bytes = left.name.as_bytes();
    let right_bytes = right.name.as_bytes();
    let common = left_bytes.len().min(right_bytes.len());
    match left_bytes[..common].cmp(&right_bytes[..common]) {
        Ordering::Equal => {
            let left_next = left_bytes
                .get(common)
                .copied()
                .unwrap_or(if left.mode == Mode::Tree { b'/' } else { 0 });
            let right_next = right_bytes
                .get(common)
                .copied()
                .unwrap_or(if right.mode == Mode::Tree { b'/' } else { 0 });
            left_next.cmp(&right_next)
        }
        ordering => ordering,
    }
}

fn put_path(
    directory: &mut Directory,
    store: &dyn ObjectStore,
    components: &[&str],
    full_path: &str,
    mode: Mode,
    value: PutValue,
    explicitly_deleted: &HashSet<String>,
) -> Result<(), String> {
    directory.load(store)?;
    if components.len() == 1 {
        if matches!(directory.entries.get(components[0]), Some(Node::Tree(_)))
            && !explicitly_deleted.contains(full_path)
        {
            return Err(format!("path {full_path}: is a directory"));
        }
        let node = if mode == Mode::Tree {
            let PutValue::Oid(oid) = value else {
                return Err(format!("path {full_path}: tree content requires an oid"));
            };
            Node::Tree(Directory::from_oid(Some(oid)))
        } else {
            Node::Blob { mode, value }
        };
        directory.entries.insert(components[0].to_string(), node);
        directory.dirty = true;
        return Ok(());
    }
    let name = components[0];
    if !directory.entries.contains_key(name) {
        directory
            .entries
            .insert(name.to_string(), Node::Tree(Directory::empty()));
        directory.dirty = true;
    }
    let Some(node) = directory.entries.get_mut(name) else {
        unreachable!();
    };
    match node {
        Node::Blob { .. } => {
            let depth = full_path.split('/').count() - components.len();
            let prefix = full_path
                .split('/')
                .take(depth + 1)
                .collect::<Vec<_>>()
                .join("/");
            Err(format!("path {full_path}: {prefix} is a file"))
        }
        Node::Tree(child) => {
            put_path(
                child,
                store,
                &components[1..],
                full_path,
                mode,
                value,
                explicitly_deleted,
            )?;
            directory.dirty = true;
            Ok(())
        }
    }
}

fn delete_path(
    directory: &mut Directory,
    store: &dyn ObjectStore,
    components: &[&str],
) -> Result<bool, String> {
    directory.load(store)?;
    if components.len() == 1 {
        let changed = directory.entries.remove(components[0]).is_some();
        directory.dirty |= changed;
        return Ok(changed);
    }
    let Some(node) = directory.entries.get_mut(components[0]) else {
        return Ok(false);
    };
    let Node::Tree(child) = node else {
        return Ok(false);
    };
    let changed = delete_path(child, store, &components[1..])?;
    if changed {
        directory.dirty = true;
    }
    Ok(changed)
}

fn write_directory(
    mut directory: Directory,
    store: &mut dyn ObjectStore,
    root: bool,
) -> Result<Option<Oid>, String> {
    if !directory.dirty {
        return Ok(directory.oid.or_else(|| root.then(empty_tree)));
    }
    directory.load(store)?;
    let mut entries = Vec::new();
    for (name, node) in directory.entries {
        match node {
            Node::Blob { mode, value } => {
                let oid = match value {
                    PutValue::Bytes(bytes) => store.write_blob(&bytes).map_err(String::from)?,
                    PutValue::Oid(oid) => oid,
                };
                entries.push(TreeEntry { name, mode, oid });
            }
            Node::Tree(child) => {
                if let Some(oid) = write_directory(child, store, false)? {
                    if oid != empty_tree() {
                        entries.push(TreeEntry {
                            name,
                            mode: Mode::Tree,
                            oid,
                        });
                    }
                }
            }
        }
    }
    if entries.is_empty() {
        return Ok(root.then(empty_tree));
    }
    canonical_tree_order(&mut entries);
    store.write_tree(&entries).map(Some).map_err(String::from)
}

fn diff_trees(
    store: &dyn ObjectStore,
    before: Option<&Oid>,
    after: Option<&Oid>,
    prefix: &str,
    changes: &mut Vec<Change>,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    let before_entries = read_optional_tree(store, before)?;
    let after_entries = read_optional_tree(store, after)?;
    let before_by_name: HashMap<&str, &TreeEntry> = before_entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let after_by_name: HashMap<&str, &TreeEntry> = after_entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let mut names: Vec<&str> = before_by_name
        .keys()
        .chain(after_by_name.keys())
        .copied()
        .collect();
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    names.dedup();
    for name in names {
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        diff_entries(
            store,
            before_by_name.get(name).copied(),
            after_by_name.get(name).copied(),
            &path,
            changes,
        )?;
    }
    Ok(())
}

fn diff_entries(
    store: &dyn ObjectStore,
    before: Option<&TreeEntry>,
    after: Option<&TreeEntry>,
    path: &str,
    changes: &mut Vec<Change>,
) -> Result<(), String> {
    match (before, after) {
        (Some(before), Some(after)) if before.mode == Mode::Tree && after.mode == Mode::Tree => {
            diff_trees(store, Some(&before.oid), Some(&after.oid), path, changes)
        }
        (Some(before), Some(after)) if before.mode != Mode::Tree && after.mode != Mode::Tree => {
            if before.mode != after.mode || before.oid != after.oid {
                changes.push(Change {
                    path: path.to_string(),
                    before: Some((before.mode, before.oid.clone())),
                    after: Some((after.mode, after.oid.clone())),
                });
            }
            Ok(())
        }
        (Some(before), Some(after)) => {
            emit_leaves(store, Some(before), path, false, changes)?;
            emit_leaves(store, Some(after), path, true, changes)
        }
        (Some(before), None) => emit_leaves(store, Some(before), path, false, changes),
        (None, Some(after)) => emit_leaves(store, Some(after), path, true, changes),
        (None, None) => Ok(()),
    }
}

fn emit_leaves(
    store: &dyn ObjectStore,
    entry: Option<&TreeEntry>,
    path: &str,
    added: bool,
    changes: &mut Vec<Change>,
) -> Result<(), String> {
    let Some(entry) = entry else {
        return Ok(());
    };
    if entry.mode != Mode::Tree {
        let value = Some((entry.mode, entry.oid.clone()));
        changes.push(Change {
            path: path.to_string(),
            before: if added { None } else { value.clone() },
            after: if added { value } else { None },
        });
        return Ok(());
    }
    for child in store.read_tree(&entry.oid).map_err(String::from)? {
        emit_leaves(
            store,
            Some(&child),
            &format!("{path}/{}", child.name),
            added,
            changes,
        )?;
    }
    Ok(())
}

fn read_optional_tree(
    store: &dyn ObjectStore,
    oid: Option<&Oid>,
) -> Result<Vec<TreeEntry>, String> {
    match oid {
        None => Ok(Vec::new()),
        Some(oid) => read_tree_or_empty(store, oid),
    }
}

struct ObjectMap {
    objects: HashMap<Oid, (ObjectKind, Vec<u8>)>,
}

impl ObjectMap {
    fn new() -> ObjectMap {
        ObjectMap {
            objects: HashMap::new(),
        }
    }

    fn read_object(&self, oid: &Oid, expected: ObjectKind) -> Result<&[u8], StoreError> {
        let Some((kind, bytes)) = self.objects.get(oid) else {
            return Err(StoreError::Missing(oid.clone()));
        };
        if *kind != expected {
            return Err(StoreError::WrongType {
                oid: oid.clone(),
                expected: expected.as_str(),
            });
        }
        Ok(bytes)
    }

    fn write_object(&mut self, kind: ObjectKind, bytes: Vec<u8>) -> Oid {
        let oid = object_id(kind, &bytes);
        self.objects.insert(oid.clone(), (kind, bytes));
        oid
    }
}

impl ObjectStore for ObjectMap {
    fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>, StoreError> {
        self.read_object(oid, ObjectKind::Blob).map(<[u8]>::to_vec)
    }

    fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>, StoreError> {
        let bytes = self.read_object(oid, ObjectKind::Tree)?;
        parse_tree_bytes(oid, bytes)
    }

    fn read_commit(&self, oid: &Oid) -> Result<CommitInfo, StoreError> {
        let bytes = self.read_object(oid, ObjectKind::Commit)?;
        parse_commit_bytes(oid, bytes)
    }

    fn write_blob(&mut self, bytes: &[u8]) -> Result<Oid, StoreError> {
        Ok(self.write_object(ObjectKind::Blob, bytes.to_vec()))
    }

    fn write_tree(&mut self, entries: &[TreeEntry]) -> Result<Oid, StoreError> {
        if !is_canonical_tree_order(entries) {
            return Err(StoreError::Other(
                "tree entries are not in canonical order".to_string(),
            ));
        }
        Ok(self.write_object(ObjectKind::Tree, encode_tree_bytes(entries)))
    }

    fn write_commit(&mut self, commit: &CommitInfo) -> Result<Oid, StoreError> {
        Ok(self.write_object(ObjectKind::Commit, encode_commit_bytes(commit)))
    }
}

pub(crate) struct DryRunStore<'s> {
    inner: &'s dyn ObjectStore,
    written: ObjectMap,
}

impl<'s> DryRunStore<'s> {
    pub(crate) fn new(inner: &'s dyn ObjectStore) -> Self {
        Self {
            inner,
            written: ObjectMap::new(),
        }
    }
}

impl ObjectStore for DryRunStore<'_> {
    fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>, StoreError> {
        match self.written.read_blob(oid) {
            Err(StoreError::Missing(_)) => self.inner.read_blob(oid),
            result => result,
        }
    }

    fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>, StoreError> {
        match self.written.read_tree(oid) {
            Err(StoreError::Missing(_)) => self.inner.read_tree(oid),
            result => result,
        }
    }

    fn read_commit(&self, oid: &Oid) -> Result<CommitInfo, StoreError> {
        match self.written.read_commit(oid) {
            Err(StoreError::Missing(_)) => self.inner.read_commit(oid),
            result => result,
        }
    }

    fn write_blob(&mut self, bytes: &[u8]) -> Result<Oid, StoreError> {
        self.written.write_blob(bytes)
    }

    fn write_tree(&mut self, entries: &[TreeEntry]) -> Result<Oid, StoreError> {
        self.written.write_tree(entries)
    }

    fn write_commit(&mut self, commit: &CommitInfo) -> Result<Oid, StoreError> {
        self.written.write_commit(commit)
    }
}

#[cfg(any(test, feature = "memory-store"))]
pub struct MemoryStore {
    map: ObjectMap,
}

#[cfg(any(test, feature = "memory-store"))]
impl MemoryStore {
    pub fn new() -> MemoryStore {
        MemoryStore {
            map: ObjectMap::new(),
        }
    }

    pub fn contains(&self, oid: &Oid) -> bool {
        self.map.objects.contains_key(oid)
    }
}

#[cfg(any(test, feature = "memory-store"))]
impl Default for MemoryStore {
    fn default() -> MemoryStore {
        MemoryStore::new()
    }
}

#[cfg(any(test, feature = "memory-store"))]
crate::delegate_object_store!(MemoryStore, map);

pub fn parse_tree_bytes(oid: &Oid, bytes: &[u8]) -> Result<Vec<TreeEntry>, StoreError> {
    let mut entries = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let space = bytes[offset..]
            .iter()
            .position(|byte| *byte == b' ')
            .map(|position| position + offset)
            .ok_or_else(|| StoreError::Other(format!("invalid tree object {oid}")))?;
        let nul = bytes[space + 1..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|position| position + space + 1)
            .ok_or_else(|| StoreError::Other(format!("invalid tree object {oid}")))?;
        if nul + 21 > bytes.len() {
            return Err(StoreError::Other(format!("invalid tree object {oid}")));
        }
        let mode = std::str::from_utf8(&bytes[offset..space])
            .map_err(|_| StoreError::Other(format!("invalid tree object {oid}")))?;
        let name = std::str::from_utf8(&bytes[space + 1..nul])
            .map_err(|_| StoreError::Other(format!("invalid tree object {oid}")))?;
        entries.push(TreeEntry {
            name: name.to_string(),
            mode: Mode::parse(mode).map_err(StoreError::Other)?,
            oid: Oid::parse(&hex_lower(&bytes[nul + 1..nul + 21]), "tree entry")
                .map_err(StoreError::Other)?,
        });
        offset = nul + 21;
    }
    Ok(entries)
}

pub fn encode_tree_bytes(entries: &[TreeEntry]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in entries {
        bytes.extend_from_slice(entry.mode.octal().as_bytes());
        bytes.push(b' ');
        bytes.extend_from_slice(entry.name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&decode_oid(&entry.oid));
    }
    bytes
}

pub fn parse_commit_bytes(oid: &Oid, bytes: &[u8]) -> Result<CommitInfo, StoreError> {
    let separator = bytes
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .ok_or_else(|| StoreError::Other(format!("invalid commit object {oid}")))?;
    let mut headers = bytes[..separator].split(|byte| *byte == b'\n').peekable();
    let invalid = || StoreError::Other(format!("invalid commit object {oid}"));
    let value = |line: &[u8], prefix: &[u8]| -> Result<String, StoreError> {
        let value = line.strip_prefix(prefix).ok_or_else(invalid)?;
        Ok(std::str::from_utf8(value)
            .map_err(|_| invalid())?
            .to_string())
    };
    let tree = Oid::parse(
        &value(headers.next().ok_or_else(invalid)?, b"tree ")?,
        "commit tree",
    )
    .map_err(StoreError::Other)?;
    let mut parents = Vec::new();
    while headers
        .peek()
        .is_some_and(|line| line.starts_with(b"parent "))
    {
        parents.push(
            Oid::parse(
                &value(headers.next().unwrap(), b"parent ")?,
                "commit parent",
            )
            .map_err(StoreError::Other)?,
        );
    }
    let author = parse_signature(&value(headers.next().ok_or_else(invalid)?, b"author ")?)
        .map_err(StoreError::Other)?;
    let committer = parse_signature(&value(headers.next().ok_or_else(invalid)?, b"committer ")?)
        .map_err(StoreError::Other)?;
    let mut extra_headers = Vec::new();
    for line in headers {
        if line.starts_with(b" ") {
            if extra_headers.is_empty() {
                return Err(invalid());
            }
        } else {
            let space = line
                .iter()
                .position(|byte| *byte == b' ')
                .ok_or_else(invalid)?;
            let name = &line[..space];
            if name.is_empty() || matches!(name, b"tree" | b"parent" | b"author" | b"committer") {
                return Err(invalid());
            }
        }
        extra_headers.extend_from_slice(line);
        extra_headers.push(b'\n');
    }
    Ok(CommitInfo {
        tree,
        parents,
        author,
        committer,
        extra_headers,
        message: bytes[separator + 2..].to_vec(),
    })
}

pub fn encode_commit_bytes(commit: &CommitInfo) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(format!("tree {}\n", commit.tree).as_bytes());
    for parent in &commit.parents {
        bytes.extend_from_slice(format!("parent {parent}\n").as_bytes());
    }
    write_signature(&mut bytes, "author", &commit.author);
    write_signature(&mut bytes, "committer", &commit.committer);
    bytes.extend_from_slice(&commit.extra_headers);
    bytes.push(b'\n');
    bytes.extend_from_slice(&commit.message);
    bytes
}

fn decode_oid(oid: &Oid) -> [u8; 20] {
    let mut bytes = [0; 20];
    for (index, pair) in oid.as_str().as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    bytes
}

fn write_signature(output: &mut Vec<u8>, kind: &str, signature: &Signature) {
    output.extend_from_slice(
        format!(
            "{kind} {} <{}> {} {}\n",
            signature.name, signature.email, signature.time, signature.offset
        )
        .as_bytes(),
    );
}

fn parse_signature(value: &str) -> Result<Signature, String> {
    let (name, rest) = value
        .rsplit_once(" <")
        .ok_or_else(|| format!("invalid git signature {value:?}"))?;
    let (email, rest) = rest
        .split_once("> ")
        .ok_or_else(|| format!("invalid git signature {value:?}"))?;
    let (time, offset) = rest
        .split_once(' ')
        .ok_or_else(|| format!("invalid git signature {value:?}"))?;
    Ok(Signature {
        name: name.to_string(),
        email: email.to_string(),
        time: time
            .parse::<i64>()
            .map_err(|_| format!("invalid git signature {value:?}"))?,
        offset: offset.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn oid(byte: char) -> Oid {
        Oid::parse(&byte.to_string().repeat(40), "test oid").unwrap()
    }

    #[test]
    fn modes_and_tree_order_are_git_canonical() {
        assert_eq!(Mode::parse("040000"), Ok(Mode::Tree));
        assert_eq!(Mode::parse("40000"), Ok(Mode::Tree));
        assert!(Mode::parse("120000").is_err());
        assert!(Mode::parse("160000").is_err());
        let mut entries = vec![
            TreeEntry {
                name: "a".to_string(),
                mode: Mode::Tree,
                oid: oid('a'),
            },
            TreeEntry {
                name: "b".to_string(),
                mode: Mode::Blob,
                oid: oid('b'),
            },
            TreeEntry {
                name: "a.txt".to_string(),
                mode: Mode::Blob,
                oid: oid('c'),
            },
            TreeEntry {
                name: "a-b".to_string(),
                mode: Mode::Blob,
                oid: oid('d'),
            },
        ];
        canonical_tree_order(&mut entries);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a-b", "a.txt", "a", "b"]
        );
        assert!(is_canonical_tree_order(&entries));
        entries.push(entries[0].clone());
        assert!(!is_canonical_tree_order(&entries));
    }

    #[test]
    fn builder_and_snapshot_cover_nested_updates_and_deletes() {
        let mut store = MemoryStore::new();
        let mut builder = TreeBuilder::from(None);
        builder.put("a/one", Mode::Blob, b"one".to_vec());
        builder.put("a/two", Mode::Executable, b"two".to_vec());
        builder.put("sibling/keep", Mode::Blob, b"keep".to_vec());
        let first = builder.build(&mut store).unwrap();
        let snapshot = Snapshot::new(&store, first.clone());
        assert_eq!(snapshot.read("a/one").unwrap(), Some(b"one".to_vec()));
        assert_eq!(snapshot.list("a").unwrap().len(), 2);
        let sibling = snapshot.entry("sibling").unwrap().unwrap().oid;

        let mut builder = TreeBuilder::from(Some(first));
        builder.put("a/one", Mode::Blob, b"changed".to_vec());
        builder.delete("a/two");
        let second = builder.build(&mut store).unwrap();
        let snapshot = Snapshot::new(&store, second.clone());
        assert_eq!(snapshot.read("a/one").unwrap(), Some(b"changed".to_vec()));
        assert!(!snapshot.exists("a/two").unwrap());
        assert_eq!(snapshot.entry("sibling").unwrap().unwrap().oid, sibling);

        let mut builder = TreeBuilder::from(Some(second));
        builder.delete("a/one");
        let third = builder.build(&mut store).unwrap();
        assert!(!Snapshot::new(&store, third.clone()).exists("a").unwrap());
        let mut builder = TreeBuilder::from(Some(third));
        builder.delete("sibling");
        assert_eq!(builder.build(&mut store).unwrap(), empty_tree());
    }

    #[test]
    fn builder_reports_file_directory_conflicts() {
        let mut store = MemoryStore::new();
        let mut builder = TreeBuilder::from(None);
        builder.put("a", Mode::Blob, b"file".to_vec());
        builder.put("a/b", Mode::Blob, b"nested".to_vec());
        assert_eq!(
            builder.build(&mut store),
            Err("path a/b: a is a file".to_string())
        );

        let mut builder = TreeBuilder::from(None);
        builder.put("a/b", Mode::Blob, b"nested".to_vec());
        builder.put("a", Mode::Blob, b"file".to_vec());
        assert_eq!(
            builder.build(&mut store),
            Err("path a: is a directory".to_string())
        );

        let mut builder = TreeBuilder::from(None);
        builder.put("a/b", Mode::Blob, b"nested".to_vec());
        builder.delete("a");
        builder.put("a", Mode::Blob, b"file".to_vec());
        assert!(builder.build(&mut store).is_ok());

        let mut builder = TreeBuilder::from(None);
        builder.put_oid("a", Mode::Tree, empty_tree());
        builder.put("a", Mode::Blob, b"last operation wins".to_vec());
        assert!(builder.build(&mut store).is_ok());
    }

    #[test]
    fn diff_reports_sorted_leaf_changes() {
        let mut store = MemoryStore::new();
        let mut builder = TreeBuilder::from(None);
        builder.put("delete", Mode::Blob, b"old".to_vec());
        builder.put("mode", Mode::Blob, b"mode".to_vec());
        builder.put("modify", Mode::Blob, b"old".to_vec());
        builder.put("same/file", Mode::Blob, b"same".to_vec());
        builder.put("swap", Mode::Blob, b"leaf".to_vec());
        let before = builder.build(&mut store).unwrap();

        let mut builder = TreeBuilder::from(Some(before.clone()));
        builder.put("add", Mode::Blob, b"new".to_vec());
        builder.delete("delete");
        let mode_oid = Snapshot::new(&store, before.clone())
            .entry("mode")
            .unwrap()
            .unwrap()
            .oid;
        builder.put_oid("mode", Mode::Executable, mode_oid);
        builder.put("modify", Mode::Blob, b"new".to_vec());
        builder.delete("swap");
        builder.put("swap/nested", Mode::Blob, b"tree".to_vec());
        let after = builder.build(&mut store).unwrap();
        let changes = diff(&store, Some(&before), &after).unwrap();
        assert_eq!(
            changes
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>(),
            vec!["add", "delete", "mode", "modify", "swap", "swap/nested"]
        );
        assert!(changes.iter().all(|change| change.path != "same/file"));
    }

    struct CountingStore {
        inner: MemoryStore,
        tree_reads: Cell<usize>,
    }

    impl ObjectStore for CountingStore {
        fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>, StoreError> {
            self.inner.read_blob(oid)
        }
        fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>, StoreError> {
            self.tree_reads.set(self.tree_reads.get() + 1);
            self.inner.read_tree(oid)
        }
        fn read_commit(&self, oid: &Oid) -> Result<CommitInfo, StoreError> {
            self.inner.read_commit(oid)
        }
        fn write_blob(&mut self, bytes: &[u8]) -> Result<Oid, StoreError> {
            self.inner.write_blob(bytes)
        }
        fn write_tree(&mut self, entries: &[TreeEntry]) -> Result<Oid, StoreError> {
            self.inner.write_tree(entries)
        }
        fn write_commit(&mut self, commit: &CommitInfo) -> Result<Oid, StoreError> {
            self.inner.write_commit(commit)
        }
    }

    #[test]
    fn snapshot_and_diff_do_not_descend_into_unneeded_subtrees() {
        let mut store = CountingStore {
            inner: MemoryStore::new(),
            tree_reads: Cell::new(0),
        };
        let mut builder = TreeBuilder::from(None);
        builder.put("left/a/file", Mode::Blob, b"left".to_vec());
        builder.put("right/b/file", Mode::Blob, b"right".to_vec());
        let before = builder.build(&mut store).unwrap();
        store.tree_reads.set(0);
        assert_eq!(
            Snapshot::new(&store, before.clone())
                .read("left/a/file")
                .unwrap(),
            Some(b"left".to_vec())
        );
        assert_eq!(store.tree_reads.get(), 3);

        let mut builder = TreeBuilder::from(Some(before.clone()));
        builder.put("left/a/file", Mode::Blob, b"changed".to_vec());
        let after = builder.build(&mut store).unwrap();
        store.tree_reads.set(0);
        let changes = diff(&store, Some(&before), &after).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "left/a/file");
        assert_eq!(store.tree_reads.get(), 6);
    }
}
