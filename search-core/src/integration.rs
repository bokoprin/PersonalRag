use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Display, Formatter};

use crate::types::{DocumentInput, Generation, LogicalDocId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Upsert,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentChange {
    pub kind: ChangeKind,
    pub key: String,
    pub document: Option<DocumentInput>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeBatch {
    pub expected_base_generation: Generation,
    pub changes: Vec<DocumentChange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub logical_id: LogicalDocId,
    pub key: String,
    pub last_generation: Generation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogSnapshot {
    pub generation: Generation,
    pub next_logical_id: LogicalDocId,
    pub live: HashMap<String, CatalogEntry>,
}

impl Default for CatalogSnapshot {
    fn default() -> Self {
        Self {
            generation: 0,
            next_logical_id: 1,
            live: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedUpsert {
    pub logical_id: LogicalDocId,
    pub is_insert: bool,
    pub document: DocumentInput,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IncrementalPolicy {
    pub compact_after_delta_docs: usize,
    pub compact_after_tombstone_ratio: f64,
}

impl Default for IncrementalPolicy {
    fn default() -> Self {
        Self {
            compact_after_delta_docs: 100_000,
            compact_after_tombstone_ratio: 0.20,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdatePlan {
    pub base_generation: Generation,
    pub next_generation: Generation,
    pub upserts: Vec<PlannedUpsert>,
    pub tombstones: Vec<LogicalDocId>,
    pub live_docs_after: usize,
    pub compaction_recommended: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationError(pub String);

impl Display for IntegrationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for IntegrationError {}

pub fn plan_incremental_update(
    base: &CatalogSnapshot,
    batch: &ChangeBatch,
    policy: IncrementalPolicy,
) -> Result<UpdatePlan, IntegrationError> {
    if batch.expected_base_generation != base.generation {
        return Err(IntegrationError("stale base generation".into()));
    }
    if !policy.compact_after_tombstone_ratio.is_finite()
        || !(0.0..=1.0).contains(&policy.compact_after_tombstone_ratio)
    {
        return Err(IntegrationError(
            "invalid tombstone compaction ratio".into(),
        ));
    }

    let mut last: HashMap<&str, &DocumentChange> = HashMap::new();
    let mut order: Vec<&str> = Vec::with_capacity(batch.changes.len());
    for change in &batch.changes {
        if change.key.is_empty() {
            return Err(IntegrationError("empty document key".into()));
        }
        match change.kind {
            ChangeKind::Upsert => {
                let Some(document) = &change.document else {
                    return Err(IntegrationError("upsert document/key mismatch".into()));
                };
                if document.key != change.key {
                    return Err(IntegrationError("upsert document/key mismatch".into()));
                }
            }
            ChangeKind::Delete if change.document.is_some() => {
                return Err(IntegrationError(
                    "delete must not contain a document".into(),
                ));
            }
            ChangeKind::Delete => {}
        }
        if !last.contains_key(change.key.as_str()) {
            order.push(change.key.as_str());
        }
        last.insert(change.key.as_str(), change);
    }

    let mut scratch = base.clone();
    let next_generation = base
        .generation
        .checked_add(1)
        .ok_or_else(|| IntegrationError("generation overflow".into()))?;
    let mut upserts = Vec::new();
    let mut tombstones = Vec::new();

    for key in order {
        let change = last[key];
        let existing = scratch.live.get(key).cloned();
        match change.kind {
            ChangeKind::Delete => {
                if let Some(entry) = existing {
                    tombstones.push(entry.logical_id);
                    scratch.live.remove(key);
                }
            }
            ChangeKind::Upsert => {
                let document = change
                    .document
                    .clone()
                    .ok_or_else(|| IntegrationError("upsert document/key mismatch".into()))?;
                let (logical_id, is_insert) = if let Some(entry) = existing {
                    tombstones.push(entry.logical_id);
                    (entry.logical_id, false)
                } else {
                    let id = scratch.next_logical_id;
                    scratch.next_logical_id = id
                        .checked_add(1)
                        .ok_or_else(|| IntegrationError("logical id overflow".into()))?;
                    (id, true)
                };
                scratch.live.insert(
                    key.to_owned(),
                    CatalogEntry {
                        logical_id,
                        key: key.to_owned(),
                        last_generation: next_generation,
                    },
                );
                upserts.push(PlannedUpsert {
                    logical_id,
                    is_insert,
                    document,
                });
            }
        }
    }

    tombstones.sort_unstable();
    tombstones.dedup();
    let tombstone_ratio = if base.live.is_empty() {
        0.0
    } else {
        tombstones.len() as f64 / base.live.len() as f64
    };
    let compaction_recommended = upserts.len() >= policy.compact_after_delta_docs
        || tombstone_ratio >= policy.compact_after_tombstone_ratio;

    Ok(UpdatePlan {
        base_generation: base.generation,
        next_generation,
        upserts,
        tombstones,
        live_docs_after: scratch.live.len(),
        compaction_recommended,
    })
}

pub fn apply_update_plan(
    base: &CatalogSnapshot,
    plan: &UpdatePlan,
) -> Result<CatalogSnapshot, IntegrationError> {
    let expected_next = base
        .generation
        .checked_add(1)
        .ok_or_else(|| IntegrationError("generation overflow".into()))?;
    if plan.base_generation != base.generation || plan.next_generation != expected_next {
        return Err(IntegrationError("invalid update plan generation".into()));
    }
    let mut out = base.clone();
    let tombstones: HashSet<LogicalDocId> = plan.tombstones.iter().copied().collect();
    out.live
        .retain(|_, entry| !tombstones.contains(&entry.logical_id));
    for upsert in &plan.upserts {
        out.live.insert(
            upsert.document.key.clone(),
            CatalogEntry {
                logical_id: upsert.logical_id,
                key: upsert.document.key.clone(),
                last_generation: plan.next_generation,
            },
        );
        let next_logical_id = upsert
            .logical_id
            .checked_add(1)
            .ok_or_else(|| IntegrationError("logical id overflow".into()))?;
        out.next_logical_id = out.next_logical_id.max(next_logical_id);
    }
    out.generation = plan.next_generation;
    if out.live.len() != plan.live_docs_after {
        return Err(IntegrationError("plan live-doc count mismatch".into()));
    }
    Ok(out)
}
