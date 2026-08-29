use std::collections::HashMap;
use std::path::PathBuf;

pub const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
pub const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
pub const USN_REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
pub const USN_REASON_HARD_LINK_CHANGE: u32 = 0x0001_0000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsnCheckpoint {
    pub journal_id: u64,
    pub next_usn: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalBounds {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointStatus {
    Valid,
    ReconcileRequired,
}

pub fn validate_checkpoint(checkpoint: UsnCheckpoint, bounds: JournalBounds) -> CheckpointStatus {
    if checkpoint.journal_id != bounds.journal_id
        || checkpoint.next_usn < bounds.first_usn
        || checkpoint.next_usn > bounds.next_usn
    {
        CheckpointStatus::ReconcileRequired
    } else {
        CheckpointStatus::Valid
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsnRecordV2 {
    pub file_reference: u64,
    pub parent_reference: u64,
    pub usn: i64,
    pub reason: u32,
    pub attributes: u32,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NormalizedFsChange {
    Create {
        file_id: u64,
        parent_id: u64,
        name: String,
        is_directory: bool,
    },
    Modify {
        file_id: u64,
    },
    Delete {
        file_id: u64,
    },
    Rename {
        file_id: u64,
        old_parent_id: u64,
        old_name: String,
        new_parent_id: u64,
        new_name: String,
    },
    ReconcileRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRenameState {
    pub file_id: u64,
    pub parent_id: u64,
    pub name: String,
    pub usn: i64,
}

#[derive(Clone, Debug)]
struct PendingRename {
    parent_id: u64,
    name: String,
    usn: i64,
}

#[derive(Clone, Debug)]
pub struct UsnNormalizer {
    checkpoint: UsnCheckpoint,
    pending_rename: HashMap<u64, PendingRename>,
}

impl UsnNormalizer {
    pub fn new(checkpoint: UsnCheckpoint) -> Self {
        Self {
            checkpoint,
            pending_rename: HashMap::new(),
        }
    }

    pub fn checkpoint(&self) -> UsnCheckpoint {
        self.checkpoint
    }
    pub fn has_pending_rename(&self) -> bool {
        !self.pending_rename.is_empty()
    }

    pub fn from_persisted(checkpoint: UsnCheckpoint, pending: Vec<PendingRenameState>) -> Self {
        let pending_rename = pending
            .into_iter()
            .map(|value| {
                (
                    value.file_id,
                    PendingRename {
                        parent_id: value.parent_id,
                        name: value.name,
                        usn: value.usn,
                    },
                )
            })
            .collect();
        Self {
            checkpoint,
            pending_rename,
        }
    }

    pub fn pending_state(&self) -> Vec<PendingRenameState> {
        let mut values = self
            .pending_rename
            .iter()
            .map(|(file_id, value)| PendingRenameState {
                file_id: *file_id,
                parent_id: value.parent_id,
                name: value.name.clone(),
                usn: value.usn,
            })
            .collect::<Vec<_>>();
        values.sort_unstable_by_key(|value| (value.usn, value.file_id));
        values
    }

    pub fn process_batch(
        &mut self,
        records: &[UsnRecordV2],
        observed_next_usn: i64,
    ) -> Vec<NormalizedFsChange> {
        let mut out = Vec::new();
        let mut safe_next = observed_next_usn;
        for record in records {
            if record.reason & USN_REASON_HARD_LINK_CHANGE != 0 {
                out.push(NormalizedFsChange::ReconcileRequired);
            }
            if record.reason & USN_REASON_RENAME_OLD_NAME != 0 {
                self.pending_rename.insert(
                    record.file_reference,
                    PendingRename {
                        parent_id: record.parent_reference,
                        name: record.name.clone(),
                        usn: record.usn,
                    },
                );
                safe_next = safe_next.min(record.usn);
                continue;
            }
            if record.reason & USN_REASON_RENAME_NEW_NAME != 0 {
                if let Some(old) = self.pending_rename.remove(&record.file_reference) {
                    out.push(NormalizedFsChange::Rename {
                        file_id: record.file_reference,
                        old_parent_id: old.parent_id,
                        old_name: old.name,
                        new_parent_id: record.parent_reference,
                        new_name: record.name.clone(),
                    });
                } else {
                    out.push(NormalizedFsChange::ReconcileRequired);
                }
            } else if record.reason & USN_REASON_FILE_CREATE != 0 {
                out.push(NormalizedFsChange::Create {
                    file_id: record.file_reference,
                    parent_id: record.parent_reference,
                    name: record.name.clone(),
                    is_directory: record.attributes & 0x10 != 0,
                });
            } else if record.reason & USN_REASON_FILE_DELETE != 0 {
                out.push(NormalizedFsChange::Delete {
                    file_id: record.file_reference,
                });
            } else if record.reason
                & (USN_REASON_DATA_OVERWRITE
                    | USN_REASON_DATA_EXTEND
                    | USN_REASON_DATA_TRUNCATION
                    | USN_REASON_BASIC_INFO_CHANGE)
                != 0
            {
                out.push(NormalizedFsChange::Modify {
                    file_id: record.file_reference,
                });
            }
        }
        if let Some(oldest_pending) = self.pending_rename.values().map(|value| value.usn).min() {
            safe_next = safe_next.min(oldest_pending);
        }
        self.checkpoint.next_usn = safe_next;
        out
    }
}

#[derive(Clone, Debug, Default)]
pub struct FrnTree {
    nodes: HashMap<u64, FrnNode>,
}

#[derive(Clone, Debug)]
struct FrnNode {
    parent: u64,
    name: String,
    is_directory: bool,
}

impl FrnTree {
    pub fn insert(&mut self, file_id: u64, parent: u64, name: String, is_directory: bool) {
        self.nodes.insert(
            file_id,
            FrnNode {
                parent,
                name,
                is_directory,
            },
        );
    }

    pub fn remove(&mut self, file_id: u64) {
        self.nodes.remove(&file_id);
    }

    pub fn rename(&mut self, file_id: u64, parent: u64, name: String) {
        if let Some(node) = self.nodes.get_mut(&file_id) {
            node.parent = parent;
            node.name = name;
        }
    }

    pub fn is_directory(&self, file_id: u64) -> bool {
        self.nodes
            .get(&file_id)
            .is_some_and(|node| node.is_directory)
    }

    pub fn path_from_root(&self, file_id: u64) -> Option<PathBuf> {
        let mut parts = Vec::<String>::new();
        let mut current = file_id;
        let mut guard = 0_usize;
        while let Some(node) = self.nodes.get(&current) {
            parts.push(node.name.clone());
            if node.parent == current || node.parent == 0 {
                break;
            }
            current = node.parent;
            guard += 1;
            if guard > self.nodes.len() {
                return None;
            }
        }
        if parts.is_empty() {
            return None;
        }
        parts.reverse();
        let mut path = PathBuf::new();
        for part in parts {
            path.push(part);
        }
        Some(path)
    }

    pub fn descendant_ids(&self, directory_id: u64) -> Vec<u64> {
        let mut result = Vec::new();
        let mut stack = vec![directory_id];
        while let Some(parent) = stack.pop() {
            for (file_id, node) in &self.nodes {
                if node.parent == parent && *file_id != parent {
                    result.push(*file_id);
                    if node.is_directory {
                        stack.push(*file_id);
                    }
                }
            }
        }
        result
    }
}
