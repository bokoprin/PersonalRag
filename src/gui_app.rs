use crate::app::{
    AppContentHit, AppMetadataHit, AppPaths, DiscoveredVolume, FederatedContentIndex,
    FederatedMetadataIndex, RuntimeReader, RuntimeSnapshot, VolumeKey,
};
use crate::extraction::{ExtractorConfig, extract_document, is_extractable_document};
use crate::gui::{
    GUI_PREVIEW_CHARS, GuiContentMode, GuiError, GuiFileScope, GuiIndexStatus, GuiMatch,
    GuiResultRow, GuiSearchRequest, GuiSearchResponse, GuiSearchStats, context_preview,
    gui_file_query_matches,
};
use crate::incremental::ContentQueryKind;
use crate::persistent::crc64_ecma;
use crate::SearchLimits;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct AppGuiSearchSession {
    paths: AppPaths,
    volumes: Vec<DiscoveredVolume>,
    extractor: ExtractorConfig,
    runtime: RuntimeReader,
    metadata: FederatedMetadataIndex,
    content: FederatedContentIndex,
    loaded_revision: u64,
}

impl AppGuiSearchSession {
    pub fn load(
        paths: AppPaths,
        volumes: Vec<DiscoveredVolume>,
        extractor: ExtractorConfig,
        runtime: RuntimeReader,
    ) -> Result<Self, GuiError> {
        let loaded_revision = runtime.revision();
        let metadata = FederatedMetadataIndex::load(&paths, &volumes)
            .map_err(|error| GuiError::new(format!("failed to load app metadata: {error}")))?;
        let content = FederatedContentIndex::load(&paths, &volumes, &extractor)
            .map_err(|error| GuiError::new(format!("failed to load app content: {error}")))?;
        Ok(Self {
            paths,
            volumes,
            extractor,
            runtime,
            metadata,
            content,
            loaded_revision,
        })
    }

    pub fn reload(&mut self) -> Result<GuiIndexStatus, GuiError> {
        self.metadata = FederatedMetadataIndex::load(&self.paths, &self.volumes)
            .map_err(|error| GuiError::new(format!("failed to reload app metadata: {error}")))?;
        self.content = FederatedContentIndex::load(&self.paths, &self.volumes, &self.extractor)
            .map_err(|error| GuiError::new(format!("failed to reload app content: {error}")))?;
        self.loaded_revision = self.runtime.revision();
        Ok(self.status())
    }

    pub fn reload_if_changed(&mut self) -> Result<bool, GuiError> {
        let revision = self.runtime.revision();
        if revision == self.loaded_revision {
            return Ok(false);
        }
        self.reload()?;
        Ok(true)
    }

    pub fn runtime_snapshot(&self) -> RuntimeSnapshot {
        self.runtime.snapshot()
    }

    pub fn status(&self) -> GuiIndexStatus {
        let snapshot = self.runtime.snapshot();
        GuiIndexStatus {
            root: PathBuf::from("<all-local-fixed-drives>"),
            store: self.paths.root.clone(),
            bundle_generation: snapshot.revision,
            content_generation: snapshot
                .volumes
                .iter()
                .map(|value| value.content_shards as u64)
                .sum(),
            metadata_generation: snapshot.revision,
            delta_generation: 0,
            state_generation: snapshot.revision,
            metadata_records: snapshot
                .volumes
                .iter()
                .map(|value| value.metadata_records)
                .sum(),
            delta_changes: 0,
        }
    }

    pub fn search(&mut self, request: &GuiSearchRequest) -> Result<GuiSearchResponse, GuiError> {
        self.reload_if_changed()?;
        let started = Instant::now();
        let max_files = request.max_files.max(1);

        if request.content_query.is_empty() {
            let hits = match request.file_scope {
                GuiFileScope::Filename => self.metadata.search(
                    (!request.file_query.is_empty()).then_some(request.file_query.as_str()),
                    None,
                    request.case_sensitive,
                    max_files,
                ),
                GuiFileScope::FullPath => self.metadata.search(
                    None,
                    (!request.file_query.is_empty()).then_some(request.file_query.as_str()),
                    request.case_sensitive,
                    max_files,
                ),
            };
            let rows = hits.into_iter().map(metadata_row).collect::<Vec<_>>();
            return Ok(GuiSearchResponse {
                stats: GuiSearchStats {
                    elapsed: started.elapsed(),
                    candidate_content_hits: 0,
                    returned_files: rows.len(),
                    bundle_generation: self.loaded_revision,
                },
                rows,
            });
        }

        let query = match request.content_mode {
            GuiContentMode::Literal => ContentQueryKind::Literal(&request.content_query),
            GuiContentMode::Regex => ContentQueryKind::Regex(&request.content_query),
            GuiContentMode::Wildcard => ContentQueryKind::Wildcard(&request.content_query),
        };
        let hits = self
            .content
            .search(
                query,
                request.case_sensitive,
                SearchLimits {
                    max_files,
                    max_matches_seen: max_files.saturating_mul(5).max(500),
                    max_snippets_per_file: 3,
                },
            )
            .map_err(|error| GuiError::new(format!("app content search failed: {error}")))?;
        let candidate_content_hits = hits.len();
        let mut rows = Vec::<GuiResultRow>::new();
        let mut row_by_file = HashMap::<(VolumeKey, u64), usize>::new();
        let mut preview_cache = HashMap::<(VolumeKey, u64), Vec<String>>::new();

        for hit in hits {
            if !self.file_query_matches(&hit, request) {
                continue;
            }
            let key = (hit.volume.clone(), hit.record.file_id);
            let matched = self.preview_match(&hit, &mut preview_cache);
            let row_index = if let Some(index) = row_by_file.get(&key).copied() {
                index
            } else {
                if rows.len() >= max_files {
                    break;
                }
                let index = rows.len();
                rows.push(content_row(&hit));
                row_by_file.insert(key.clone(), index);
                index
            };
            rows[row_index].matches.push(matched);
        }

        Ok(GuiSearchResponse {
            stats: GuiSearchStats {
                elapsed: started.elapsed(),
                candidate_content_hits,
                returned_files: rows.len(),
                bundle_generation: self.loaded_revision,
            },
            rows,
        })
    }

    fn file_query_matches(&self, hit: &AppContentHit, request: &GuiSearchRequest) -> bool {
        if request.file_query.is_empty() {
            return true;
        }
        let path = match request.file_scope {
            GuiFileScope::Filename => hit.record.path.as_path(),
            GuiFileScope::FullPath => hit.absolute_path.as_path(),
        };
        gui_file_query_matches(
            path,
            &request.file_query,
            request.file_scope,
            request.case_sensitive,
        )
    }

    fn preview_match(
        &self,
        hit: &AppContentHit,
        cache: &mut HashMap<(VolumeKey, u64), Vec<String>>,
    ) -> GuiMatch {
        let key = (hit.volume.clone(), hit.record.file_id);
        let lines = cache.entry(key).or_insert_with(|| {
            load_logical_units(&hit.absolute_path, &self.extractor)
                .unwrap_or_else(|error| vec![format!("[preview unavailable: {error}]")])
        });
        let text = lines
            .get(hit.line_number.saturating_sub(1) as usize)
            .map(String::as_str)
            .unwrap_or("");
        let preview = context_preview(
            text,
            hit.byte_offset_in_line as usize,
            GUI_PREVIEW_CHARS,
        );
        let location = if is_extractable_document(&hit.record.path) {
            format!(
                "Unit {} · byte {}",
                hit.line_number, hit.byte_offset_in_line
            )
        } else {
            format!(
                "Line {} · byte {}",
                hit.line_number, hit.byte_offset_in_line
            )
        };
        GuiMatch {
            line_number: hit.line_number,
            byte_offset: hit.byte_offset_in_line,
            location,
            preview,
        }
    }
}

fn metadata_row(hit: AppMetadataHit) -> GuiResultRow {
    let id = global_file_id(&hit.volume, hit.record.file_id);
    let name = hit
        .record
        .path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| hit.record.path.to_string_lossy().into_owned());
    GuiResultRow {
        file_id: id,
        name,
        relative_path: hit.absolute_path.clone(),
        absolute_path: hit.absolute_path,
        size: hit.record.size,
        modified_ns: hit.record.modified_ns,
        kind: hit.record.kind,
        matches: Vec::new(),
    }
}

fn content_row(hit: &AppContentHit) -> GuiResultRow {
    let name = hit
        .record
        .path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| hit.record.path.to_string_lossy().into_owned());
    GuiResultRow {
        file_id: global_file_id(&hit.volume, hit.record.file_id),
        name,
        relative_path: hit.absolute_path.clone(),
        absolute_path: hit.absolute_path.clone(),
        size: hit.record.size,
        modified_ns: hit.record.modified_ns,
        kind: hit.record.kind,
        matches: Vec::new(),
    }
}

fn global_file_id(volume: &VolumeKey, file_id: u64) -> u64 {
    let mut bytes = Vec::with_capacity(volume.0.len() + 8);
    bytes.extend_from_slice(volume.0.as_bytes());
    bytes.extend_from_slice(&file_id.to_le_bytes());
    crc64_ecma(&bytes)
}

fn load_logical_units(path: &Path, extractor: &ExtractorConfig) -> Result<Vec<String>, String> {
    if is_extractable_document(path) {
        return extract_document(path, extractor)
            .map(|document| document.units)
            .map_err(|error| error.to_string());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.lines().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        AppRuntimeHandle, DiscoveredVolume, VolumeKey,
    };
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "personalrag-gui-app-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn zero_config_session_becomes_searchable_as_runtime_publishes() {
        let base = temp_dir("runtime");
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("alpha-target.txt"), "content-target").unwrap();

        let paths = AppPaths::for_root(&app_root);
        let volume = DiscoveredVolume {
            key: VolumeKey("gui-app-test".to_string()),
            mount: root.clone(),
            serial: 1,
        };
        let extractor = ExtractorConfig::discover();
        let mut runtime = AppRuntimeHandle::start_with(
            paths.clone(),
            vec![volume.clone()],
            extractor.clone(),
            false,
        )
        .unwrap();
        let reader = runtime.reader();
        let mut session =
            AppGuiSearchSession::load(paths, vec![volume], extractor, reader).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let response = session
                .search(&GuiSearchRequest {
                    file_query: "alpha-target".to_string(),
                    ..GuiSearchRequest::default()
                })
                .unwrap();
            if response.rows.len() == 1 {
                assert!(response.rows[0].absolute_path.ends_with("alpha-target.txt"));
                break;
            }
            assert!(Instant::now() < deadline, "metadata did not become searchable");
            std::thread::sleep(Duration::from_millis(20));
        }

        runtime.join();
        fs::remove_dir_all(base).unwrap();
    }
}
