use std::collections::{HashMap, HashSet};

use personalrag_portable_search::{CatalogEntry, CatalogSnapshot};

use crate::ScannedFile;

#[derive(Clone, Debug)]
pub struct IncrementalCatalogState {
    pub generation: u64,
    pub next_logical_id: u64,
    pub paths: Vec<String>,
    pub logical_ids: Vec<u64>,
    pub size_bytes: Vec<u64>,
    pub modified_ns: Vec<u64>,
}

impl IncrementalCatalogState {
    pub fn validate(&self) -> Result<(), String> {
        let len = self.paths.len();
        if self.logical_ids.len() != len
            || self.size_bytes.len() != len
            || self.modified_ns.len() != len
        {
            return Err("incremental catalog arrays are not aligned".to_owned());
        }
        let mut ids = HashSet::with_capacity(len);
        let mut keys = HashSet::with_capacity(len);
        for (&logical_id, path) in self.logical_ids.iter().zip(&self.paths) {
            if logical_id == 0 || !ids.insert(logical_id) {
                return Err(
                    "incremental catalog logical IDs must be unique and non-zero".to_owned(),
                );
            }
            if path.is_empty() || !keys.insert(path.as_str()) {
                return Err("incremental catalog paths must be unique and non-empty".to_owned());
            }
        }
        let required_next = self
            .logical_ids
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        if self.next_logical_id < required_next.max(1) {
            return Err("incremental catalog next logical ID is stale".to_owned());
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<CatalogSnapshot, String> {
        self.validate()?;
        let live = self
            .paths
            .iter()
            .zip(&self.logical_ids)
            .map(|(path, &logical_id)| {
                (
                    path.clone(),
                    CatalogEntry {
                        logical_id,
                        key: path.clone(),
                        last_generation: self.generation,
                    },
                )
            })
            .collect();
        Ok(CatalogSnapshot {
            generation: self.generation,
            next_logical_id: self.next_logical_id,
            live,
        })
    }
}

#[derive(Debug)]
pub struct ExistingChange {
    pub logical_id: u64,
    pub file: ScannedFile,
}

#[derive(Debug)]
pub struct DeletedChange {
    pub logical_id: u64,
    pub path: String,
}

#[derive(Debug, Default)]
pub struct CatalogDiff {
    pub unchanged: usize,
    pub added: Vec<ScannedFile>,
    pub modified: Vec<ExistingChange>,
    pub deleted: Vec<DeletedChange>,
}

impl CatalogDiff {
    #[must_use]
    pub fn changed_files(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }
}

pub fn diff_catalog(
    previous: &IncrementalCatalogState,
    scanned: &[ScannedFile],
) -> Result<CatalogDiff, String> {
    previous.validate()?;
    let mut ordered = scanned.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    if ordered
        .windows(2)
        .any(|pair| pair[0].display_path == pair[1].display_path)
    {
        return Err("scanner returned duplicate display paths".to_owned());
    }

    let old = previous
        .paths
        .iter()
        .enumerate()
        .map(|(row, path)| (path.as_str(), row))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::with_capacity(ordered.len());
    let mut diff = CatalogDiff::default();
    for file in ordered {
        seen.insert(file.display_path.clone());
        let Some(&row) = old.get(file.display_path.as_str()) else {
            diff.added.push(file.clone());
            continue;
        };
        if previous.size_bytes[row] == file.size_bytes
            && previous.modified_ns[row] == file.modified_ns
        {
            diff.unchanged += 1;
        } else {
            diff.modified.push(ExistingChange {
                logical_id: previous.logical_ids[row],
                file: file.clone(),
            });
        }
    }
    for (row, path) in previous.paths.iter().enumerate() {
        if !seen.contains(path) {
            diff.deleted.push(DeletedChange {
                logical_id: previous.logical_ids[row],
                path: path.clone(),
            });
        }
    }
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(path: &str, size: u64, modified: u64) -> ScannedFile {
        ScannedFile {
            path: PathBuf::from(path),
            display_path: path.to_owned(),
            size_bytes: size,
            modified_ns: modified,
            index_content: true,
        }
    }

    #[test]
    fn diff_classifies_added_modified_deleted_and_unchanged() {
        let previous = IncrementalCatalogState {
            generation: 4,
            next_logical_id: 14,
            paths: vec!["a.txt".into(), "b.txt".into(), "gone.txt".into()],
            logical_ids: vec![2, 7, 13],
            size_bytes: vec![10, 20, 30],
            modified_ns: vec![100, 200, 300],
        };
        let diff = diff_catalog(
            &previous,
            &[
                file("new.txt", 1, 1),
                file("a.txt", 10, 100),
                file("b.txt", 21, 201),
            ],
        )
        .unwrap();
        assert_eq!(diff.unchanged, 1);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.modified.len(), 1);
        assert_eq!(diff.modified[0].logical_id, 7);
        assert_eq!(diff.deleted.len(), 1);
        assert_eq!(diff.deleted[0].logical_id, 13);
    }
}
