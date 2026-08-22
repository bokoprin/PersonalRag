#[cfg(windows)]
use super::{
    content_index_eligible, relative_path_pruned, standard_pruned_dir, DirectoryTrackingSnapshot,
    ScannedFile, TrackedDirectory,
};
use super::{ScanExclusions, ScanProgress, ScanReport, ScannerMode};
use std::{
    path::Path,
    sync::{atomic::AtomicBool, Arc},
};

const FILE_ID_BOTH_DIR_INFO_HEADER_LEN: usize = 104;
const FILETIME_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
const DIRECTORY_WORK_CLAIM_MAX: usize = 4;
const DIRECTORY_BUFFER_KIB_DEFAULT: usize = 1024;
const DIRECTORY_BUFFER_KIB_MIN: usize = 64;
const DIRECTORY_BUFFER_KIB_MAX: usize = 4096;

fn directory_work_claim(queued: usize, workers: usize) -> usize {
    if queued == 0 {
        return 0;
    }
    queued
        .div_ceil(workers.max(1).saturating_mul(2))
        .clamp(1, DIRECTORY_WORK_CLAIM_MAX)
}

fn pending_after_completion(
    pending: usize,
    completed: usize,
    discovered_children: usize,
) -> Option<usize> {
    pending
        .checked_add(discovered_children)
        .and_then(|value| value.checked_sub(completed))
}

fn normalize_directory_buffer_kib(value: Option<usize>) -> usize {
    value
        .unwrap_or(DIRECTORY_BUFFER_KIB_DEFAULT)
        .clamp(DIRECTORY_BUFFER_KIB_MIN, DIRECTORY_BUFFER_KIB_MAX)
}

#[derive(Debug, Clone, Copy)]
struct DirectoryEntryRef<'a> {
    attributes: u32,
    end_of_file: u64,
    last_write_filetime: u64,
    file_id: u64,
    name_bytes: &'a [u8],
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedDirectoryEntry {
    attributes: u32,
    end_of_file: u64,
    last_write_filetime: u64,
    file_id: u64,
    name_utf16: Vec<u16>,
}

#[inline(always)]
unsafe fn read_u32_unchecked(bytes: *const u8, offset: usize) -> u32 {
    // SAFETY: callers first verify that the complete fixed-size directory record header is in-bounds.
    u32::from_le(unsafe { std::ptr::read_unaligned(bytes.add(offset).cast::<u32>()) })
}

#[inline(always)]
unsafe fn read_u64_unchecked(bytes: *const u8, offset: usize) -> u64 {
    // SAFETY: callers first verify that the complete fixed-size directory record header is in-bounds.
    u64::from_le(unsafe { std::ptr::read_unaligned(bytes.add(offset).cast::<u64>()) })
}

#[inline(always)]
unsafe fn read_i64_unchecked(bytes: *const u8, offset: usize) -> i64 {
    // SAFETY: callers first verify that the complete fixed-size directory record header is in-bounds.
    i64::from_le(unsafe { std::ptr::read_unaligned(bytes.add(offset).cast::<i64>()) })
}

fn visit_directory_buffer<'a>(
    bytes: &'a [u8],
    mut visit: impl FnMut(DirectoryEntryRef<'a>) -> Result<(), String>,
) -> Result<(), String> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut offset = 0usize;
    let base = bytes.as_ptr();
    loop {
        if bytes.len().saturating_sub(offset) < FILE_ID_BOTH_DIR_INFO_HEADER_LEN {
            return Err("truncated FILE_ID_BOTH_DIR_INFO header".to_owned());
        }
        // The fixed fields below all live inside the 104-byte header checked above.
        let next = unsafe { read_u32_unchecked(base, offset) } as usize;
        let entry_end = if next == 0 {
            bytes.len()
        } else {
            let end = offset
                .checked_add(next)
                .ok_or_else(|| "FILE_ID_BOTH_DIR_INFO offset overflow".to_owned())?;
            if next < FILE_ID_BOTH_DIR_INFO_HEADER_LEN || end > bytes.len() {
                return Err("invalid FILE_ID_BOTH_DIR_INFO NextEntryOffset".to_owned());
            }
            end
        };
        let name_len = unsafe { read_u32_unchecked(base, offset + 60) } as usize;
        if !name_len.is_multiple_of(2) {
            return Err("odd FILE_ID_BOTH_DIR_INFO FileNameLength".to_owned());
        }
        let name_start = offset + FILE_ID_BOTH_DIR_INFO_HEADER_LEN;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or_else(|| "FILE_ID_BOTH_DIR_INFO filename overflow".to_owned())?;
        if name_end > entry_end {
            return Err("FILE_ID_BOTH_DIR_INFO filename exceeds record".to_owned());
        }
        visit(DirectoryEntryRef {
            attributes: unsafe { read_u32_unchecked(base, offset + 56) },
            end_of_file: unsafe { read_i64_unchecked(base, offset + 40) }.max(0) as u64,
            last_write_filetime: unsafe { read_u64_unchecked(base, offset + 24) },
            file_id: unsafe { read_u64_unchecked(base, offset + 96) },
            name_bytes: &bytes[name_start..name_end],
        })?;
        if next == 0 {
            break;
        }
        offset += next;
    }
    Ok(())
}

fn append_name_utf16(name_bytes: &[u8], out: &mut Vec<u16>) {
    out.clear();
    out.reserve(name_bytes.len() / 2);
    out.extend(
        name_bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]])),
    );
}

#[cfg(test)]
fn parse_directory_buffer(bytes: &[u8]) -> Result<Vec<ParsedDirectoryEntry>, String> {
    let mut entries = Vec::new();
    visit_directory_buffer(bytes, |entry| {
        let mut name_utf16 = Vec::new();
        append_name_utf16(entry.name_bytes, &mut name_utf16);
        entries.push(ParsedDirectoryEntry {
            attributes: entry.attributes,
            end_of_file: entry.end_of_file,
            last_write_filetime: entry.last_write_filetime,
            file_id: entry.file_id,
            name_utf16,
        });
        Ok(())
    })?;
    Ok(entries)
}

fn filetime_to_unix_ns(filetime: u64) -> u64 {
    filetime
        .saturating_sub(FILETIME_UNIX_EPOCH_100NS)
        .saturating_mul(100)
}

pub(super) fn native_batch_compatible(mode: ScannerMode, config: &ScanExclusions) -> bool {
    matches!(mode, ScannerMode::Auto | ScannerMode::WindowsNative)
        && !config.use_gitignore
        && config.custom_globs.is_empty()
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        collections::VecDeque,
        ffi::{c_void, OsString},
        fs,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::PathBuf,
        ptr,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Condvar, Mutex,
        },
        thread,
        time::{Duration, Instant},
    };

    type Handle = isize;

    const INVALID_HANDLE_VALUE: Handle = -1isize;
    const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_ID_BOTH_DIRECTORY_INFO: i32 = 10;
    const ERROR_NO_MORE_FILES: u32 = 18;
    const DIRECTORY_TRACKING_BATCH: usize = 1_024;
    const PROGRESS_COUNTER_BATCH: usize = 1_024;
    const PROGRESS_REPORT_STEP: usize = 1_024;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: Handle,
        ) -> Handle;
        fn GetFileInformationByHandleEx(
            file: Handle,
            information_class: i32,
            information: *mut c_void,
            buffer_size: u32,
        ) -> i32;
        fn GetFileInformationByHandle(
            file: Handle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    struct OwnedHandle(Handle);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    #[derive(Debug, Clone)]
    struct DirectoryTask {
        open_path: PathBuf,
        scan_path: PathBuf,
        relative_path: String,
        file_id: Option<u64>,
        is_root: bool,
    }

    struct QueueState {
        directories: VecDeque<DirectoryTask>,
        pending: usize,
    }

    struct DirectoryQueue {
        state: Mutex<QueueState>,
        ready: Condvar,
    }

    impl DirectoryQueue {
        fn new(root: DirectoryTask) -> Self {
            Self {
                state: Mutex::new(QueueState {
                    directories: VecDeque::from([root]),
                    pending: 1,
                }),
                ready: Condvar::new(),
            }
        }

        fn pop_many(
            &self,
            cancel: &AtomicBool,
            stop: &AtomicBool,
            workers: usize,
            batch: &mut Vec<DirectoryTask>,
        ) -> Result<bool, String> {
            batch.clear();
            let mut state = self
                .state
                .lock()
                .map_err(|_| "native scanner directory queue poisoned".to_owned())?;
            loop {
                if cancel.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
                    return Ok(false);
                }
                let claim = directory_work_claim(state.directories.len(), workers);
                while batch.len() < claim {
                    let Some(directory) = state.directories.pop_front() else {
                        break;
                    };
                    batch.push(directory);
                }
                if !batch.is_empty() {
                    return Ok(true);
                }
                if state.pending == 0 {
                    return Ok(false);
                }
                let (next, _) = self
                    .ready
                    .wait_timeout(state, Duration::from_millis(25))
                    .map_err(|_| "native scanner directory queue poisoned".to_owned())?;
                state = next;
            }
        }

        fn push_many(&self, directories: &mut Vec<DirectoryTask>) -> Result<(), String> {
            if directories.is_empty() {
                return Ok(());
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| "native scanner directory queue poisoned".to_owned())?;
            state.pending = state
                .pending
                .checked_add(directories.len())
                .ok_or_else(|| "native scanner pending-directory overflow".to_owned())?;
            state.directories.extend(directories.drain(..));
            self.ready.notify_all();
            Ok(())
        }

        fn complete_many(&self, completed: usize) -> Result<(), String> {
            if completed == 0 {
                return Ok(());
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| "native scanner directory queue poisoned".to_owned())?;
            state.pending = pending_after_completion(state.pending, completed, 0)
                .ok_or_else(|| "native scanner pending-directory underflow".to_owned())?;
            if state.pending == 0 {
                self.ready.notify_all();
            }
            Ok(())
        }

        fn complete_many_with_children(
            &self,
            completed: usize,
            children: &mut Vec<DirectoryTask>,
        ) -> Result<(), String> {
            if completed == 0 && children.is_empty() {
                return Ok(());
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| "native scanner directory queue poisoned".to_owned())?;
            state.pending = pending_after_completion(state.pending, completed, children.len())
                .ok_or_else(|| "native scanner pending-directory accounting overflow".to_owned())?;
            state.directories.extend(children.drain(..));
            if state.pending == 0 || !state.directories.is_empty() {
                self.ready.notify_all();
            }
            Ok(())
        }

        fn wake_all(&self) {
            self.ready.notify_all();
        }
    }

    struct FileBatch {
        local: Vec<ScannedFile>,
        shared: Arc<Mutex<Vec<ScannedFile>>>,
    }

    struct DirectoryTrackingBatch {
        local: Vec<TrackedDirectory>,
        shared: Arc<Mutex<Vec<TrackedDirectory>>>,
    }

    impl DirectoryTrackingBatch {
        fn new(shared: Arc<Mutex<Vec<TrackedDirectory>>>) -> Self {
            Self {
                local: Vec::with_capacity(DIRECTORY_TRACKING_BATCH),
                shared,
            }
        }

        fn push(&mut self, directory: TrackedDirectory) -> Result<(), String> {
            self.local.push(directory);
            if self.local.len() >= DIRECTORY_TRACKING_BATCH {
                self.flush()?;
            }
            Ok(())
        }

        fn flush(&mut self) -> Result<(), String> {
            if self.local.is_empty() {
                return Ok(());
            }
            self.shared
                .lock()
                .map_err(|_| "native scanner directory tracking poisoned".to_owned())?
                .append(&mut self.local);
            Ok(())
        }
    }

    impl Drop for DirectoryTrackingBatch {
        fn drop(&mut self) {
            let _ = self.flush();
        }
    }

    #[derive(Default)]
    struct LocalProgressCounters {
        discovered: usize,
        file_entries: usize,
        directory_entries: usize,
        selected: usize,
        pruned: usize,
        selected_bytes: u64,
    }

    impl LocalProgressCounters {
        fn record_discovered(&mut self) {
            self.discovered += 1;
        }

        fn record_file_entry(&mut self) {
            self.file_entries += 1;
        }

        fn record_directory_entry(&mut self) {
            self.directory_entries += 1;
        }

        fn record_selected(&mut self, bytes: u64) {
            self.selected += 1;
            self.selected_bytes = self.selected_bytes.saturating_add(bytes);
        }

        fn record_pruned(&mut self) {
            self.pruned += 1;
        }

        fn should_flush(&self) -> bool {
            self.discovered >= PROGRESS_COUNTER_BATCH
        }

        fn flush(&mut self, shared: &NativeShared) {
            let discovered = self.discovered;
            if discovered != 0 {
                shared.discovered.fetch_add(discovered, Ordering::Relaxed);
                self.discovered = 0;
            }
            if self.file_entries != 0 {
                shared
                    .file_entries
                    .fetch_add(self.file_entries, Ordering::Relaxed);
                self.file_entries = 0;
            }
            if self.directory_entries != 0 {
                shared
                    .directory_entries
                    .fetch_add(self.directory_entries, Ordering::Relaxed);
                self.directory_entries = 0;
            }
            if self.selected != 0 {
                shared.selected.fetch_add(self.selected, Ordering::Relaxed);
                self.selected = 0;
            }
            if self.pruned != 0 {
                shared.pruned.fetch_add(self.pruned, Ordering::Relaxed);
                self.pruned = 0;
            }
            if self.selected_bytes != 0 {
                shared
                    .selected_bytes
                    .fetch_add(self.selected_bytes, Ordering::Relaxed);
                self.selected_bytes = 0;
            }
        }
    }

    impl FileBatch {
        fn new(shared: Arc<Mutex<Vec<ScannedFile>>>) -> Self {
            Self {
                local: Vec::with_capacity(4_096),
                shared,
            }
        }

        fn push(&mut self, file: ScannedFile) -> Result<(), String> {
            self.local.push(file);
            if self.local.len() >= 4_096 {
                self.flush()?;
            }
            Ok(())
        }

        fn flush(&mut self) -> Result<(), String> {
            if self.local.is_empty() {
                return Ok(());
            }
            self.shared
                .lock()
                .map_err(|_| "native scanner file list poisoned".to_owned())?
                .append(&mut self.local);
            Ok(())
        }
    }

    impl Drop for FileBatch {
        fn drop(&mut self) {
            let _ = self.flush();
        }
    }

    #[derive(Clone)]
    struct NativeShared {
        files: Arc<Mutex<Vec<ScannedFile>>>,
        directories: Arc<Mutex<Vec<TrackedDirectory>>>,
        tracking_complete: Arc<AtomicBool>,
        discovered: Arc<AtomicUsize>,
        file_entries: Arc<AtomicUsize>,
        directory_entries: Arc<AtomicUsize>,
        other_entries: Arc<AtomicUsize>,
        selected: Arc<AtomicUsize>,
        pruned: Arc<AtomicUsize>,
        errors: Arc<AtomicUsize>,
        selected_bytes: Arc<AtomicU64>,
        current_path: Arc<Mutex<Option<PathBuf>>>,
        batch_calls: Arc<AtomicUsize>,
        opened_directories: Arc<AtomicUsize>,
        next_progress_report: Arc<AtomicUsize>,
    }

    struct WorkerScratch {
        file_batch: FileBatch,
        directory_tracking: DirectoryTrackingBatch,
        progress: LocalProgressCounters,
        directory_buffer: Vec<u8>,
        path_utf16: Vec<u16>,
        name_utf16: Vec<u16>,
        child_directories: Vec<DirectoryTask>,
        batch_calls: usize,
        opened_directories: usize,
    }

    impl WorkerScratch {
        fn new(
            files: Arc<Mutex<Vec<ScannedFile>>>,
            directories: Arc<Mutex<Vec<TrackedDirectory>>>,
            directory_buffer_bytes: usize,
        ) -> Self {
            Self {
                file_batch: FileBatch::new(files),
                directory_tracking: DirectoryTrackingBatch::new(directories),
                progress: LocalProgressCounters::default(),
                directory_buffer: vec![0u8; directory_buffer_bytes],
                path_utf16: Vec::with_capacity(512),
                name_utf16: Vec::with_capacity(260),
                child_directories: Vec::with_capacity(64),
                batch_calls: 0,
                opened_directories: 0,
            }
        }
    }

    struct NativeScanContext<'a> {
        max_file_bytes: u64,
        config: &'a ScanExclusions,
        queue: &'a DirectoryQueue,
        shared: &'a NativeShared,
        cancel: &'a AtomicBool,
        on_progress: &'a (dyn Fn(ScanProgress) + Send + Sync),
        worker_count: usize,
    }

    fn open_directory(path: &Path, path_utf16: &mut Vec<u16>) -> Result<OwnedHandle, String> {
        path_utf16.clear();
        path_utf16.extend(path.as_os_str().encode_wide());
        path_utf16.push(0);
        let handle = unsafe {
            CreateFileW(
                path_utf16.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(OwnedHandle(handle))
    }

    fn directory_file_id(handle: Handle) -> Option<u64> {
        let mut info = ByHandleFileInformation::default();
        if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
            return None;
        }
        Some((u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low))
    }

    fn is_dot_entry(name: &[u16]) -> bool {
        name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')]
    }

    fn relative_child(parent: &str, name: &str) -> String {
        if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}/{name}")
        }
    }

    fn progress_snapshot(shared: &NativeShared) -> ScanProgress {
        ScanProgress {
            discovered_entries: shared.discovered.load(Ordering::Relaxed),
            file_entries: shared.file_entries.load(Ordering::Relaxed),
            directory_entries: shared.directory_entries.load(Ordering::Relaxed),
            other_entries: shared.other_entries.load(Ordering::Relaxed),
            selected_files: shared.selected.load(Ordering::Relaxed),
            pruned_entries: shared.pruned.load(Ordering::Relaxed),
            error_entries: shared.errors.load(Ordering::Relaxed),
            selected_bytes: shared.selected_bytes.load(Ordering::Relaxed),
            current_path: shared
                .current_path
                .lock()
                .ok()
                .and_then(|value| value.clone()),
        }
    }

    fn flush_progress_and_maybe_report(
        progress: &mut LocalProgressCounters,
        shared: &NativeShared,
        current_path: &Path,
        on_progress: &(dyn Fn(ScanProgress) + Send + Sync),
    ) {
        if !progress.should_flush() {
            return;
        }
        progress.flush(shared);
        let discovered = shared.discovered.load(Ordering::Relaxed);
        let mut next = shared.next_progress_report.load(Ordering::Relaxed);
        while discovered >= next {
            let advanced = discovered
                .saturating_div(PROGRESS_REPORT_STEP)
                .saturating_add(1)
                .saturating_mul(PROGRESS_REPORT_STEP);
            match shared.next_progress_report.compare_exchange_weak(
                next,
                advanced,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if let Ok(mut path) = shared.current_path.lock() {
                        *path = Some(current_path.to_path_buf());
                    }
                    on_progress(progress_snapshot(shared));
                    break;
                }
                Err(actual) => next = actual,
            }
        }
    }

    fn enumerate_directory(
        task: &DirectoryTask,
        context: &NativeScanContext<'_>,
        scratch: &mut WorkerScratch,
    ) -> Result<(), String> {
        let max_file_bytes = context.max_file_bytes;
        let config = context.config;
        let queue = context.queue;
        let shared = context.shared;
        let cancel = context.cancel;
        let on_progress = context.on_progress;
        let handle = open_directory(&task.open_path, &mut scratch.path_utf16)?;
        scratch.opened_directories = scratch.opened_directories.saturating_add(1);
        match task.file_id.or_else(|| directory_file_id(handle.0)) {
            Some(file_id) => scratch.directory_tracking.push(TrackedDirectory {
                file_id,
                relative_path: task.relative_path.clone(),
            })?,
            None => shared.tracking_complete.store(false, Ordering::Release),
        }

        loop {
            if cancel.load(Ordering::Acquire) {
                return Err("cancelled".to_owned());
            }
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    handle.0,
                    FILE_ID_BOTH_DIRECTORY_INFO,
                    scratch.directory_buffer.as_mut_ptr().cast(),
                    scratch.directory_buffer.len() as u32,
                )
            };
            if ok == 0 {
                let error = unsafe { GetLastError() };
                if error == ERROR_NO_MORE_FILES {
                    return Ok(());
                }
                return Err(format!(
                    "GetFileInformationByHandleEx failed for {}: OS error {error}",
                    task.scan_path.display()
                ));
            }
            scratch.batch_calls = scratch.batch_calls.saturating_add(1);
            let has_relative_pruning = !config.custom_relative_paths.is_empty();
            visit_directory_buffer(&scratch.directory_buffer, |record| {
                let is_directory = record.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;

                // Oversized regular files can be rejected directly from the directory record.
                // Avoid UTF-16 decoding, String creation and PathBuf joins for these entries.
                if !is_directory && max_file_bytes != 0 && record.end_of_file > max_file_bytes {
                    scratch.progress.record_discovered();
                    scratch.progress.record_file_entry();
                    scratch.progress.record_pruned();
                    flush_progress_and_maybe_report(
                        &mut scratch.progress,
                        shared,
                        &task.scan_path,
                        on_progress,
                    );
                    return Ok(());
                }

                append_name_utf16(record.name_bytes, &mut scratch.name_utf16);
                if is_directory && is_dot_entry(&scratch.name_utf16) {
                    return Ok(());
                }
                let name = OsString::from_wide(&scratch.name_utf16);
                let display_name = name.to_string_lossy();
                scratch.progress.record_discovered();
                if is_directory {
                    scratch.progress.record_directory_entry();
                } else {
                    scratch.progress.record_file_entry();
                }

                if is_directory {
                    if record.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        // Preserve follow_links(false): never descend into directory reparse points.
                        scratch.progress.record_pruned();
                        flush_progress_and_maybe_report(
                            &mut scratch.progress,
                            shared,
                            &task.scan_path,
                            on_progress,
                        );
                        return Ok(());
                    }
                    // Name-only pruning is deliberately checked before relative/path construction.
                    if standard_pruned_dir(&display_name, config) {
                        scratch.progress.record_pruned();
                        flush_progress_and_maybe_report(
                            &mut scratch.progress,
                            shared,
                            &task.scan_path,
                            on_progress,
                        );
                        return Ok(());
                    }
                    let relative = relative_child(&task.relative_path, &display_name);
                    if has_relative_pruning && relative_path_pruned(&relative, config) {
                        scratch.progress.record_pruned();
                        flush_progress_and_maybe_report(
                            &mut scratch.progress,
                            shared,
                            &task.scan_path,
                            on_progress,
                        );
                        return Ok(());
                    }
                    scratch.child_directories.push(DirectoryTask {
                        open_path: task.open_path.join(&name),
                        scan_path: task.scan_path.join(&name),
                        relative_path: relative,
                        file_id: Some(record.file_id),
                        is_root: false,
                    });
                } else {
                    let relative = relative_child(&task.relative_path, &display_name);
                    if has_relative_pruning && relative_path_pruned(&relative, config) {
                        scratch.progress.record_pruned();
                        flush_progress_and_maybe_report(
                            &mut scratch.progress,
                            shared,
                            &task.scan_path,
                            on_progress,
                        );
                        return Ok(());
                    }
                    // Regular files only need the scan/output path. The old native path built a
                    // second open_path PathBuf for every file and immediately discarded it.
                    let scan_path = task.scan_path.join(&name);
                    scratch.progress.record_selected(record.end_of_file);
                    let index_content = content_index_eligible(&scan_path);
                    scratch.file_batch.push(ScannedFile {
                        path: scan_path,
                        display_path: relative,
                        size_bytes: record.end_of_file,
                        modified_ns: filetime_to_unix_ns(record.last_write_filetime),
                        index_content,
                    })?;
                }

                flush_progress_and_maybe_report(
                    &mut scratch.progress,
                    shared,
                    &task.scan_path,
                    on_progress,
                );
                Ok(())
            })?;
            // Keep the common small-child case local so enqueue + parent completion share one
            // queue lock. Wide roots still seed idle workers as soon as one worker-batch exists.
            if scratch.child_directories.len() >= context.worker_count.max(1) {
                queue.push_many(&mut scratch.child_directories)?;
            }
        }
    }

    fn directory_buffer_bytes_from_env() -> usize {
        let kib = std::env::var("PR_NATIVE_DIR_BUFFER_KIB")
            .ok()
            .and_then(|value| value.parse::<usize>().ok());
        normalize_directory_buffer_kib(kib).saturating_mul(1024)
    }

    pub(super) fn scan_files_native(
        root: &Path,
        max_file_bytes: u64,
        mode: ScannerMode,
        config: &ScanExclusions,
        cancel: Arc<AtomicBool>,
        on_progress: Arc<dyn Fn(ScanProgress) + Send + Sync>,
    ) -> Result<Option<ScanReport>, String> {
        if !native_batch_compatible(mode, config) {
            return Ok(None);
        }
        let started = Instant::now();
        let open_root = match fs::canonicalize(root) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        };
        let root_task = DirectoryTask {
            open_path: open_root,
            scan_path: root.to_path_buf(),
            relative_path: String::new(),
            file_id: None,
            is_root: true,
        };
        let queue = Arc::new(DirectoryQueue::new(root_task));
        let stop = Arc::new(AtomicBool::new(false));
        let root_failed = Arc::new(AtomicBool::new(false));
        let root_error = Arc::new(Mutex::new(None::<String>));
        let fatal = Arc::new(Mutex::new(None::<String>));
        let shared = NativeShared {
            files: Arc::new(Mutex::new(Vec::new())),
            directories: Arc::new(Mutex::new(Vec::new())),
            tracking_complete: Arc::new(AtomicBool::new(true)),
            discovered: Arc::new(AtomicUsize::new(1)), // Match WalkBuilder's root entry.
            file_entries: Arc::new(AtomicUsize::new(0)),
            directory_entries: Arc::new(AtomicUsize::new(1)),
            other_entries: Arc::new(AtomicUsize::new(0)),
            selected: Arc::new(AtomicUsize::new(0)),
            pruned: Arc::new(AtomicUsize::new(0)),
            errors: Arc::new(AtomicUsize::new(0)),
            selected_bytes: Arc::new(AtomicU64::new(0)),
            current_path: Arc::new(Mutex::new(None)),
            batch_calls: Arc::new(AtomicUsize::new(0)),
            opened_directories: Arc::new(AtomicUsize::new(0)),
            next_progress_report: Arc::new(AtomicUsize::new(PROGRESS_REPORT_STEP)),
        };
        let worker_count = mode.traversal_threads().clamp(1, 8);
        let directory_buffer_bytes = directory_buffer_bytes_from_env();

        thread::scope(|scope| {
            for _ in 0..worker_count {
                let queue = Arc::clone(&queue);
                let stop = Arc::clone(&stop);
                let root_failed = Arc::clone(&root_failed);
                let root_error = Arc::clone(&root_error);
                let fatal = Arc::clone(&fatal);
                let cancel = Arc::clone(&cancel);
                let on_progress = Arc::clone(&on_progress);
                let files = Arc::clone(&shared.files);
                let directories = Arc::clone(&shared.directories);
                let local_shared = shared.clone();
                scope.spawn(move || {
                    let mut scratch =
                        WorkerScratch::new(files, directories, directory_buffer_bytes);
                    let context = NativeScanContext {
                        max_file_bytes,
                        config,
                        queue: &queue,
                        shared: &local_shared,
                        cancel: &cancel,
                        on_progress: on_progress.as_ref(),
                        worker_count,
                    };
                    let mut work_batch = Vec::with_capacity(DIRECTORY_WORK_CLAIM_MAX);
                    loop {
                        let has_work =
                            match queue.pop_many(&cancel, &stop, worker_count, &mut work_batch) {
                                Ok(has_work) => has_work,
                                Err(error) => {
                                    if let Ok(mut slot) = fatal.lock() {
                                        *slot = Some(error);
                                    }
                                    stop.store(true, Ordering::Release);
                                    queue.wake_all();
                                    break;
                                }
                            };
                        if !has_work {
                            break;
                        }
                        let claimed = work_batch.len();
                        scratch.child_directories.clear();
                        for task in work_batch.drain(..) {
                            if stop.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
                                break;
                            }
                            let result = enumerate_directory(&task, &context, &mut scratch);
                            if let Err(error) = result {
                                if error == "cancelled" {
                                    stop.store(true, Ordering::Release);
                                } else if task.is_root {
                                    if let Ok(mut slot) = root_error.lock() {
                                        *slot = Some(error);
                                    }
                                    root_failed.store(true, Ordering::Release);
                                    stop.store(true, Ordering::Release);
                                } else {
                                    local_shared.errors.fetch_add(1, Ordering::Relaxed);
                                    local_shared
                                        .tracking_complete
                                        .store(false, Ordering::Release);
                                }
                            }
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                        }
                        let completion = if stop.load(Ordering::Acquire) {
                            scratch.child_directories.clear();
                            queue.complete_many(claimed)
                        } else {
                            queue.complete_many_with_children(
                                claimed,
                                &mut scratch.child_directories,
                            )
                        };
                        if let Err(error) = completion {
                            if let Ok(mut slot) = fatal.lock() {
                                *slot = Some(error);
                            }
                            stop.store(true, Ordering::Release);
                        }
                        if stop.load(Ordering::Acquire) {
                            queue.wake_all();
                        }
                    }
                    scratch.progress.flush(&local_shared);
                    if scratch.batch_calls != 0 {
                        local_shared
                            .batch_calls
                            .fetch_add(scratch.batch_calls, Ordering::Relaxed);
                    }
                    if scratch.opened_directories != 0 {
                        local_shared
                            .opened_directories
                            .fetch_add(scratch.opened_directories, Ordering::Relaxed);
                    }
                    let flush_result = scratch
                        .file_batch
                        .flush()
                        .and_then(|_| scratch.directory_tracking.flush());
                    if let Err(error) = flush_result {
                        if let Ok(mut slot) = fatal.lock() {
                            *slot = Some(error);
                        }
                        stop.store(true, Ordering::Release);
                        queue.wake_all();
                    }
                });
            }
        });

        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }
        if let Some(error) = fatal
            .lock()
            .map_err(|_| "native scanner fatal-state lock poisoned".to_owned())?
            .take()
        {
            return Err(error);
        }
        if root_failed.load(Ordering::Acquire) {
            let error = root_error
                .lock()
                .map_err(|_| "native scanner root-error lock poisoned".to_owned())?
                .take()
                .unwrap_or_else(|| "Windows native batch enumeration failed".to_owned());
            let native_started = shared.batch_calls.load(Ordering::Relaxed) != 0
                || shared.selected.load(Ordering::Relaxed) != 0
                || shared.discovered.load(Ordering::Relaxed) > 1;
            let native_required = std::env::var_os("PR_NATIVE_SCANNER_REQUIRE").is_some();
            if !native_started && !native_required {
                // Unsupported filesystem/API: fall back before any native result was observed.
                return Ok(None);
            }
            return Err(format!("Windows native batch scanner failed: {error}"));
        }

        let progress = progress_snapshot(&shared);
        on_progress(progress.clone());
        let files = Arc::try_unwrap(shared.files)
            .map_err(|_| "native scanner file list still shared".to_owned())?
            .into_inner()
            .map_err(|_| "native scanner file list poisoned".to_owned())?;
        let directory_tracking = if shared.tracking_complete.load(Ordering::Acquire) {
            let mut directories = Arc::try_unwrap(shared.directories)
                .map_err(|_| "native scanner directory tracking still shared".to_owned())?
                .into_inner()
                .map_err(|_| "native scanner directory tracking poisoned".to_owned())?;
            directories.sort_by_key(|directory| directory.file_id);
            directories.dedup_by_key(|directory| directory.file_id);
            directories.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            Some(DirectoryTrackingSnapshot::new(true, directories))
        } else {
            None
        };

        if std::env::var_os("PR_PROFILE_SCANNER").is_some() {
            eprintln!(
                "WINDOWS_NATIVE_SCAN elapsed_ms={:.3} workers={} buffer_kib={} directory_handles={} batch_calls={} discovered={} files={} bytes={} errors={}",
                started.elapsed().as_secs_f64() * 1000.0,
                worker_count,
                directory_buffer_bytes / 1024,
                shared.opened_directories.load(Ordering::Relaxed),
                shared.batch_calls.load(Ordering::Relaxed),
                progress.discovered_entries,
                progress.selected_files,
                progress.selected_bytes,
                progress.error_entries,
            );
        }

        Ok(Some(ScanReport {
            files,
            progress,
            directory_tracking,
        }))
    }
}

#[cfg(windows)]
pub(super) fn scan_files_native(
    root: &Path,
    max_file_bytes: u64,
    mode: ScannerMode,
    config: &ScanExclusions,
    cancel: Arc<AtomicBool>,
    on_progress: Arc<dyn Fn(ScanProgress) + Send + Sync>,
) -> Result<Option<ScanReport>, String> {
    windows::scan_files_native(root, max_file_bytes, mode, config, cancel, on_progress)
}

#[cfg(not(windows))]
pub(super) fn scan_files_native(
    _root: &Path,
    _max_file_bytes: u64,
    _mode: ScannerMode,
    _config: &ScanExclusions,
    _cancel: Arc<AtomicBool>,
    _on_progress: Arc<dyn Fn(ScanProgress) + Send + Sync>,
) -> Result<Option<ScanReport>, String> {
    Ok(None)
}

#[cfg(all(test, windows))]
fn scan_files_native_for_test_or_public(
    root: &Path,
    mode: ScannerMode,
    config: &ScanExclusions,
) -> ScanReport {
    scan_files_native(
        root,
        1024 * 1024,
        mode,
        config,
        Arc::new(AtomicBool::new(false)),
        Arc::new(|_| {}),
    )
    .unwrap()
    .expect("native scanner should be available")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_record(
        out: &mut Vec<u8>,
        name: &str,
        attributes: u32,
        size: u64,
        filetime: u64,
        file_id: u64,
        has_next: bool,
    ) {
        let name = name.encode_utf16().collect::<Vec<_>>();
        let raw_len = FILE_ID_BOTH_DIR_INFO_HEADER_LEN + name.len() * 2;
        let record_len = raw_len.next_multiple_of(8);
        let start = out.len();
        out.resize(start + record_len, 0);
        if has_next {
            out[start..start + 4].copy_from_slice(&(record_len as u32).to_le_bytes());
        }
        out[start + 24..start + 32].copy_from_slice(&filetime.to_le_bytes());
        out[start + 40..start + 48].copy_from_slice(&size.to_le_bytes());
        out[start + 56..start + 60].copy_from_slice(&attributes.to_le_bytes());
        out[start + 60..start + 64].copy_from_slice(&((name.len() * 2) as u32).to_le_bytes());
        out[start + 96..start + 104].copy_from_slice(&file_id.to_le_bytes());
        for (index, unit) in name.into_iter().enumerate() {
            let offset = start + FILE_ID_BOTH_DIR_INFO_HEADER_LEN + index * 2;
            out[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        if !has_next {
            out.truncate(start + raw_len);
        }
    }

    #[test]
    fn filetime_conversion_matches_unix_epoch_and_100ns_units() {
        assert_eq!(filetime_to_unix_ns(FILETIME_UNIX_EPOCH_100NS), 0);
        assert_eq!(
            filetime_to_unix_ns(FILETIME_UNIX_EPOCH_100NS + 12_345),
            1_234_500
        );
        assert_eq!(filetime_to_unix_ns(0), 0);
    }

    #[test]
    fn file_id_both_directory_batch_parser_handles_multiple_utf16_entries() {
        let mut bytes = Vec::new();
        append_record(
            &mut bytes,
            "alpha.txt",
            0x20,
            123,
            FILETIME_UNIX_EPOCH_100NS + 77,
            0x1111_2222_3333_4444,
            true,
        );
        append_record(
            &mut bytes,
            "日本語",
            0x10,
            456,
            FILETIME_UNIX_EPOCH_100NS + 88,
            0x5555_6666_7777_8888,
            false,
        );
        let parsed = parse_directory_buffer(&bytes).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            String::from_utf16(&parsed[0].name_utf16).unwrap(),
            "alpha.txt"
        );
        assert_eq!(parsed[0].end_of_file, 123);
        assert_eq!(parsed[0].attributes, 0x20);
        assert_eq!(parsed[0].file_id, 0x1111_2222_3333_4444);
        assert_eq!(String::from_utf16(&parsed[1].name_utf16).unwrap(), "日本語");
        assert_eq!(parsed[1].attributes, 0x10);
        assert_eq!(parsed[1].file_id, 0x5555_6666_7777_8888);
    }

    #[test]
    fn file_id_both_directory_batch_parser_rejects_invalid_layout_and_clamps_negative_size() {
        let mut negative = Vec::new();
        append_record(
            &mut negative,
            "negative.bin",
            0x20,
            i64::MAX as u64 + 1,
            FILETIME_UNIX_EPOCH_100NS,
            0,
            false,
        );
        assert_eq!(parse_directory_buffer(&negative).unwrap()[0].end_of_file, 0);

        let mut odd_name = negative.clone();
        odd_name[60..64].copy_from_slice(&3u32.to_le_bytes());
        assert!(parse_directory_buffer(&odd_name).is_err());

        let mut invalid_next = Vec::new();
        append_record(
            &mut invalid_next,
            "x",
            0x20,
            1,
            FILETIME_UNIX_EPOCH_100NS,
            0,
            false,
        );
        invalid_next[..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(parse_directory_buffer(&invalid_next).is_err());
    }

    #[test]
    fn directory_work_claim_batches_only_when_queue_is_deep_enough() {
        assert_eq!(directory_work_claim(0, 8), 0);
        assert_eq!(directory_work_claim(1, 8), 1);
        assert_eq!(directory_work_claim(8, 8), 1);
        assert_eq!(directory_work_claim(16, 8), 1);
        assert_eq!(directory_work_claim(17, 8), 2);
        assert_eq!(directory_work_claim(64, 8), 4);
        assert_eq!(directory_work_claim(1_000_000, 8), 4);
    }

    #[test]
    fn native_batch_requires_semantics_supported_without_walkbuilder_rules() {
        let plain = ScanExclusions::default();
        assert!(native_batch_compatible(ScannerMode::WindowsNative, &plain));
        assert!(native_batch_compatible(ScannerMode::Auto, &plain));
        assert!(!native_batch_compatible(ScannerMode::WalkDir, &plain));

        let mut gitignore = plain.clone();
        gitignore.use_gitignore = true;
        assert!(!native_batch_compatible(
            ScannerMode::WindowsNative,
            &gitignore
        ));

        let mut globs = plain;
        globs.custom_globs.push("*.tmp".to_owned());
        assert!(!native_batch_compatible(ScannerMode::WindowsNative, &globs));
    }
    #[cfg(windows)]
    #[test]
    fn native_scanner_matches_walkdir_oracle_on_real_windows_filesystem() {
        use std::{
            fs,
            sync::{atomic::AtomicBool, Arc},
        };

        let root = std::env::temp_dir().join(format!(
            "personalrag-native-scanner-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("src/a.txt"), b"alpha").unwrap();
        fs::write(root.join("src/nested/b.md"), b"beta").unwrap();
        fs::write(root.join("image.png"), b"not indexed as text").unwrap();
        fs::write(root.join("node_modules/pkg/skip.js"), b"skip").unwrap();

        let config = ScanExclusions {
            node_modules: true,
            ..ScanExclusions::default()
        };
        let scan = |mode| super::scan_files_native_for_test_or_public(&root, mode, &config);
        let walk = crate::scan_files(
            &root,
            1024 * 1024,
            ScannerMode::WalkDir,
            &config,
            Arc::new(AtomicBool::new(false)),
            Arc::new(|_| {}),
        )
        .unwrap();
        let native = scan(ScannerMode::WindowsNative);

        let normalize = |files: Vec<ScannedFile>| {
            let mut files = files
                .into_iter()
                .map(|file| (file.display_path, file.size_bytes, file.index_content))
                .collect::<Vec<_>>();
            files.sort();
            files
        };
        assert_eq!(normalize(walk.files), normalize(native.files));
        let normalize_directories = |snapshot: DirectoryTrackingSnapshot| {
            let mut directories = snapshot
                .directories
                .into_iter()
                .map(|directory| (directory.relative_path, directory.file_id))
                .collect::<Vec<_>>();
            directories.sort();
            directories
        };
        assert_eq!(
            normalize_directories(walk.directory_tracking.expect("walk tracking")),
            normalize_directories(native.directory_tracking.expect("native tracking")),
            "FILE_ID_BOTH_DIR_INFO FileId must match the existing handle-based USN directory ID",
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pending_completion_combines_child_enqueue_without_changing_accounting() {
        assert_eq!(pending_after_completion(1, 1, 0), Some(0));
        assert_eq!(pending_after_completion(8, 4, 3), Some(7));
        assert_eq!(pending_after_completion(4, 4, 9), Some(9));
        assert_eq!(pending_after_completion(0, 1, 0), None);
    }

    #[test]
    fn directory_buffer_size_defaults_to_one_mib_and_stays_bounded() {
        assert_eq!(normalize_directory_buffer_kib(None), 1024);
        assert_eq!(normalize_directory_buffer_kib(Some(1)), 64);
        assert_eq!(normalize_directory_buffer_kib(Some(256)), 256);
        assert_eq!(normalize_directory_buffer_kib(Some(1024)), 1024);
        assert_eq!(normalize_directory_buffer_kib(Some(16_384)), 4096);
    }
}
