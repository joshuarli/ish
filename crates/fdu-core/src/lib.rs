use std::ops::Range;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModifiedTime(i64);

impl ModifiedTime {
    pub const fn from_unix_seconds(seconds: i64) -> Option<Self> {
        if seconds == i64::MIN { None } else { Some(Self(seconds)) }
    }

    pub const fn unix_seconds(self) -> Option<i64> {
        if self.0 == i64::MIN { None } else { Some(self.0) }
    }
}

impl Default for ModifiedTime {
    fn default() -> Self {
        Self(i64::MIN)
    }
}



#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NodeId(std::num::NonZeroU32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0.get() as usize - 1
    }

    pub fn from_index(index: usize) -> Option<Self> {
        let stored = u32::try_from(index).ok()?.checked_add(1)?;
        std::num::NonZeroU32::new(stored).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DirectoryToken(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryType {
    Root,
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExclusionReason {
    MountBoundary,
    UnsupportedAlias,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeState {
    Scanning,
    Complete,
    Incomplete,
    Excluded(ExclusionReason),
    Stale,
    Tombstone,
}



#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeStatus {
    Scanning,
    Complete,
    Incomplete,
    Excluded(ExclusionReason),
    Stale,
}

impl From<NodeStatus> for NodeState {
    fn from(status: NodeStatus) -> Self {
        match status {
            NodeStatus::Scanning => Self::Scanning,
            NodeStatus::Complete => Self::Complete,
            NodeStatus::Incomplete => Self::Incomplete,
            NodeStatus::Excluded(reason) => Self::Excluded(reason),
            NodeStatus::Stale => Self::Stale,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScanEntry {
    
    pub parent: DirectoryToken,
    pub name: Range<u32>,
    pub directory_token: Option<DirectoryToken>,
    pub entry_type: EntryType,
    pub identity: FileIdentity,
    pub modified_time: ModifiedTime,
    pub link_count: u64,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
    pub state: NodeStatus,
}




#[derive(Debug)]
pub struct EntryBatch {
    pub entries: Vec<ScanEntry>,
    pub names: Vec<u8>,
}

impl EntryBatch {
    pub fn with_capacity(entry_capacity: usize, name_capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(entry_capacity),
            names: Vec::with_capacity(name_capacity),
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.names.clear();
    }
}

#[derive(Debug)]
pub enum ScanEvent {
    Started {
        root_name: Vec<u8>,
        identity: FileIdentity,
        link_count: u64,
    },
    Entries(EntryBatch),
    DirectoryFinished {
        directory: DirectoryToken,
        complete: bool,
    },
    
    DirectoriesFinished(Vec<(DirectoryToken, bool)>),
    DirectoryExcluded {
        directory: DirectoryToken,
        reason: ExclusionReason,
    },
    DirectoryFailed {
        directory: DirectoryToken,
        message: String,
    },
    Failed {
        message: String,
    },
    Cancelled,
    Finished,
}

#[derive(Clone, Debug)]
pub struct NodeRecord {
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    next_sibling: Option<NodeId>,
    name: Range<u32>,
    pub entry_type: EntryType,
    pub identity: FileIdentity,
    modified_time: ModifiedTime,
    pub link_count: u64,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
    pub state: NodeState,
}

impl NodeRecord {
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    pub fn first_child(&self) -> Option<NodeId> {
        self.first_child
    }

    pub fn next_sibling(&self) -> Option<NodeId> {
        self.next_sibling
    }

    pub fn name_range(&self) -> Range<u32> {
        self.name.clone()
    }

    pub fn modified_time(&self) -> ModifiedTime {
        self.modified_time
    }
}

pub struct Tree {
    nodes: Vec<NodeRecord>,
    names: Vec<u8>,
    root: NodeId,
}

impl Tree {
    pub fn new(
        root_name: &[u8],
        identity: FileIdentity,
        link_count: u64,
    ) -> Option<Self> {
        let root = NodeId::from_index(0).expect("index zero fits");
        let name_end = u32::try_from(root_name.len()).ok()?;
        let mut names = Vec::with_capacity(root_name.len());
        names.extend_from_slice(root_name);
        Some(Self {
            nodes: vec![NodeRecord {
                parent: None,
                first_child: None,
                last_child: None,
                next_sibling: None,
                name: 0..name_end,
                entry_type: EntryType::Root,
                identity,
                modified_time: ModifiedTime::default(),
                link_count,
                apparent_bytes: 0,
                allocated_bytes: 0,
                state: NodeState::Scanning,
            }],
            names,
            root,
        })
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    
    pub fn shrink_to_fit(&mut self) {
        self.nodes.shrink_to_fit();
        self.names.shrink_to_fit();
    }

    pub fn retained_arena_bytes(&self) -> usize {
        self.nodes
            .capacity()
            .saturating_mul(std::mem::size_of::<NodeRecord>())
            .saturating_add(self.names.capacity())
    }

    pub fn record(&self, id: NodeId) -> Option<&NodeRecord> {
        self.nodes.get(id.index())
    }

    pub fn name(&self, id: NodeId) -> Option<&[u8]> {
        let range = self.record(id)?.name.clone();
        self.names.get(range.start as usize..range.end as usize)
    }

    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            tree: self,
            next: self.record(id).and_then(NodeRecord::first_child),
        }
    }

    
    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &mut self,
        parent: NodeId,
        name: &[u8],
        entry_type: EntryType,
        identity: FileIdentity,
        link_count: u64,
        apparent_bytes: u64,
        allocated_bytes: u64,
        state: NodeStatus,
    ) -> Option<NodeId> {
        let index = self.nodes.len();
        let id = NodeId::from_index(index)?;
        let name_start = u32::try_from(self.names.len()).ok()?;
        let name_end = name_start.checked_add(u32::try_from(name.len()).ok()?)?;
        self.record(parent)?;
        self.names.extend_from_slice(name);
        self.nodes.push(NodeRecord {
            parent: Some(parent),
            first_child: None,
            last_child: None,
            next_sibling: None,
            name: name_start..name_end,
            entry_type,
            identity,
            modified_time: ModifiedTime::default(),
            link_count,
            apparent_bytes,
            allocated_bytes,
            state: state.into(),
        });
        if let Some(last) = self.nodes[parent.index()].last_child {
            self.nodes[last.index()].next_sibling = Some(id);
        } else {
            self.nodes[parent.index()].first_child = Some(id);
        }
        self.nodes[parent.index()].last_child = Some(id);
        Some(id)
    }

    pub fn set_state(&mut self, id: NodeId, state: NodeStatus) -> bool {
        let Some(record) = self.nodes.get_mut(id.index()) else {
            return false;
        };
        record.state = state.into();
        true
    }

    pub fn set_modified_time(&mut self, id: NodeId, modified_time: ModifiedTime) -> bool {
        let Some(record) = self.nodes.get_mut(id.index()) else {
            return false;
        };
        record.modified_time = modified_time;
        true
    }

    
    
    pub fn add_to_ancestors(&mut self, id: NodeId, apparent: u64, allocated: u64) -> bool {
        let mut fits = true;
        let mut current = Some(id);
        while let Some(node) = current {
            let Some(record) = self.nodes.get(node.index()) else {
                return false;
            };
            if record.apparent_bytes.checked_add(apparent).is_none()
                || record.allocated_bytes.checked_add(allocated).is_none()
            {
                fits = false;
                break;
            }
            current = record.parent;
        }
        let mut current = Some(id);
        while let Some(node) = current {
            let record = &mut self.nodes[node.index()];
            if fits {
                record.apparent_bytes += apparent;
                record.allocated_bytes += allocated;
            } else {
                record.state = NodeState::Incomplete;
            }
            current = record.parent;
        }
        fits
    }

    pub fn mark_incomplete_to_root(&mut self, id: NodeId) {
        let mut current = Some(id);
        while let Some(node) = current {
            let Some(record) = self.nodes.get_mut(node.index()) else {
                break;
            };
            if record.state == NodeState::Scanning || record.state == NodeState::Complete {
                record.state = NodeState::Incomplete;
            }
            current = record.parent;
        }
    }

    pub fn materialize_path(&self, id: NodeId) -> Option<Vec<&[u8]>> {
        let mut names = Vec::new();
        let mut current = Some(id);
        while let Some(node) = current {
            let record = self.record(node)?;
            if node != self.root {
                names.push(self.name(node)?);
            }
            current = record.parent;
        }
        names.reverse();
        Some(names)
    }

    pub fn tombstone_subtree(&mut self, id: NodeId) -> Option<(u64, u64, usize)> {
        if id == self.root {
            return None;
        }
        let (apparent, allocated, parent) = {
            let record = self.record(id)?;
            if record.state == NodeState::Tombstone {
                return Some((0, 0, 0));
            }
            (record.apparent_bytes, record.allocated_bytes, record.parent?)
        };
        let mut current = Some(parent);
        let mut lineage = Vec::new();
        while let Some(node) = current {
            let record = self.nodes.get(node.index())?;
            if record.apparent_bytes < apparent || record.allocated_bytes < allocated {
                return None;
            }
            lineage.push(node);
            current = record.parent;
        }
        let mut child = self.record(id)?.first_child;
        let mut all_children_tombstoned = true;
        while let Some(child_id) = child {
            let child_record = self.record(child_id)?;
            if child_record.state != NodeState::Tombstone {
                all_children_tombstoned = false;
                break;
            }
            child = child_record.next_sibling;
        }

        let mut tombstoned = 0usize;
        let mut current = Some(id);
        while let Some(node) = current {
            let record = self.record(node)?;
            if record.state == NodeState::Tombstone {
                current = next_sibling_after_subtree(self, node, id);
                continue;
            }
            let next = if node == id && all_children_tombstoned {
                None
            } else if let Some(child) = record.first_child {
                Some(child)
            } else {
                next_sibling_after_subtree(self, node, id)
            };
            self.nodes.get_mut(node.index())?.state = NodeState::Tombstone;
            tombstoned = tombstoned.saturating_add(1);
            current = next;
        }
        for node in lineage {
            let record = &mut self.nodes[node.index()];
            record.apparent_bytes -= apparent;
            record.allocated_bytes -= allocated;
        }
        Some((apparent, allocated, tombstoned))
    }

    pub fn mark_stale_to_root(&mut self, id: NodeId) {
        let mut current = Some(id);
        while let Some(node) = current {
            let Some(record) = self.nodes.get_mut(node.index()) else {
                break;
            };
            if record.state != NodeState::Tombstone {
                record.state = NodeState::Stale;
            }
            current = record.parent;
        }
    }

    pub fn mark_scanning_incomplete(&mut self) -> usize {
        let mut changed = 0;
        for record in &mut self.nodes {
            if record.state == NodeState::Scanning {
                record.state = NodeState::Incomplete;
                changed += 1;
            }
        }
        changed
    }
}

fn next_sibling_after_subtree(tree: &Tree, node: NodeId, root: NodeId) -> Option<NodeId> {
    let mut current = node;
    loop {
        if current == root {
            return None;
        }
        let record = tree.record(current)?;
        if let Some(sibling) = record.next_sibling {
            return Some(sibling);
        }
        current = record.parent?;
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    next: Option<NodeId>,
}

impl<'a> Iterator for Children<'a> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(id) = self.next {
            let record = self.tree.record(id)?;
            self.next = record.next_sibling;
            if record.state != NodeState::Tombstone {
                return Some(id);
            }
        }
        None
    }
}

