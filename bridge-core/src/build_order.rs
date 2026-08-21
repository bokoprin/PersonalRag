use std::{thread, vec::IntoIter};

use crate::ScannedFile;

const PARALLEL_SORT_MIN_FILES: usize = 50_000;
const PARALLEL_SORT_MAX_WORKERS: usize = 8;

fn merge_sorted(left: Vec<ScannedFile>, right: Vec<ScannedFile>) -> Vec<ScannedFile> {
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    let mut merged = Vec::with_capacity(left.len().saturating_add(right.len()));

    loop {
        match (left.peek(), right.peek()) {
            (Some(left_file), Some(right_file)) => {
                // Pick the left item on equality so the merge is stable. Chunk sorting is stable,
                // therefore the complete result is equivalent to Vec::sort_by.
                if left_file.path <= right_file.path {
                    merged.push(left.next().expect("peeked left item must exist"));
                } else {
                    merged.push(right.next().expect("peeked right item must exist"));
                }
            }
            (Some(_), None) => {
                merged.extend(left);
                break;
            }
            (None, Some(_)) => {
                merged.extend(right);
                break;
            }
            (None, None) => break,
        }
    }
    merged
}

fn collect_chunks(mut source: IntoIter<ScannedFile>, workers: usize) -> Vec<Vec<ScannedFile>> {
    let len = source.len();
    let chunk_len = len.div_ceil(workers.max(1));
    let mut chunks = Vec::with_capacity(workers.min(len));
    while source.len() != 0 {
        let chunk = source.by_ref().take(chunk_len).collect::<Vec<_>>();
        if chunk.is_empty() {
            break;
        }
        chunks.push(chunk);
    }
    chunks
}

fn parallel_stable_sort(
    source: Vec<ScannedFile>,
    workers: usize,
) -> Result<Vec<ScannedFile>, String> {
    let chunks = collect_chunks(source.into_iter(), workers);
    let mut chunks = thread::scope(|scope| -> Result<Vec<Vec<ScannedFile>>, String> {
        let handles = chunks
            .into_iter()
            .map(|mut chunk| {
                scope.spawn(move || {
                    chunk.sort_by(|left, right| left.path.cmp(&right.path));
                    chunk
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| "build-order sort worker panicked".to_owned())
            })
            .collect()
    })?;

    while chunks.len() > 1 {
        chunks = thread::scope(|scope| -> Result<Vec<Vec<ScannedFile>>, String> {
            let mut pairs = chunks.into_iter();
            let mut handles = Vec::new();
            let mut tail = None;
            while let Some(left) = pairs.next() {
                if let Some(right) = pairs.next() {
                    handles.push(scope.spawn(move || merge_sorted(left, right)));
                } else {
                    tail = Some(left);
                }
            }
            let mut merged = handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| "build-order merge worker panicked".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(tail) = tail {
                merged.push(tail);
            }
            Ok(merged)
        })?;
    }

    Ok(chunks.pop().unwrap_or_default())
}

fn sort_worker_count(file_count: usize, available_workers: usize) -> usize {
    if file_count < PARALLEL_SORT_MIN_FILES {
        return 1;
    }
    available_workers
        .clamp(1, PARALLEL_SORT_MAX_WORKERS)
        .min(file_count.div_ceil(PARALLEL_SORT_MIN_FILES).max(1))
}

pub(crate) fn sort_scanned_files(files: &mut Vec<ScannedFile>) -> Result<usize, String> {
    let workers = sort_worker_count(
        files.len(),
        std::thread::available_parallelism().map_or(1, usize::from),
    );
    if workers <= 1 {
        files.sort_by(|left, right| left.path.cmp(&right.path));
        return Ok(1);
    }
    *files = parallel_stable_sort(std::mem::take(files), workers)?;
    Ok(workers)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn file(path: &str, marker: u64) -> ScannedFile {
        ScannedFile {
            path: PathBuf::from(path),
            display_path: format!("{path}#{marker}"),
            size_bytes: marker,
            modified_ns: marker,
            index_content: marker.is_multiple_of(2),
        }
    }

    #[test]
    fn worker_count_keeps_small_inputs_single_threaded_and_caps_large_inputs() {
        assert_eq!(sort_worker_count(49_999, 8), 1);
        assert_eq!(sort_worker_count(50_000, 8), 1);
        assert_eq!(sort_worker_count(100_000, 8), 2);
        assert_eq!(sort_worker_count(300_000, 8), 6);
        assert_eq!(sort_worker_count(1_000_000, 64), 8);
    }

    #[test]
    fn stable_merge_matches_standard_stable_sort_including_duplicate_paths() {
        let mut input = vec![
            file("z/file", 1),
            file("a/file", 2),
            file("m/file", 3),
            file("a/file", 4),
            file("z/file", 5),
            file("b/file", 6),
        ];
        let mut expected = input.clone();
        expected.sort_by(|left, right| left.path.cmp(&right.path));
        input = parallel_stable_sort(input, 3).unwrap();
        assert_eq!(input, expected);
    }
}
