use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{ScanExclusions, ScannedFile};

const TRACKER_VERSION: u32 = 1;
#[cfg(windows)]
const MAX_USN_RECORDS: usize = 500_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsnCheckpoint {
    pub journal_id: u64,
    pub next_usn: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedDirectory {
    pub file_id: u64,
    pub relative_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryTrackingSnapshot {
    pub version: u32,
    pub complete: bool,
    pub directories: Vec<TrackedDirectory>,
}

impl DirectoryTrackingSnapshot {
    #[must_use]
    pub fn new(complete: bool, directories: Vec<TrackedDirectory>) -> Self {
        Self {
            version: TRACKER_VERSION,
            complete,
            directories,
        }
    }

    #[cfg(any(windows, test))]
    fn validate(&self) -> Result<(), String> {
        if self.version != TRACKER_VERSION || !self.complete {
            return Err("directory tracking snapshot is incomplete or unsupported".to_owned());
        }
        let mut seen = std::collections::HashSet::with_capacity(self.directories.len());
        for directory in &self.directories {
            if !seen.insert(directory.file_id) {
                return Err("directory tracking snapshot contains duplicate file IDs".to_owned());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct UsnChangeSet {
    pub checkpoint: UsnCheckpoint,
    pub directories: DirectoryTrackingSnapshot,
    pub upserts: Vec<ScannedFile>,
    pub deleted_paths: Vec<String>,
    pub journal_records: usize,
}

#[derive(Debug, Clone)]
pub enum UsnScanResult {
    Unsupported { reason: String },
    FullScanRequired { reason: String },
    NoChanges { checkpoint: UsnCheckpoint },
    Changes(UsnChangeSet),
}

#[derive(Debug, Clone)]
#[cfg(any(windows, test))]
struct ParsedUsnRecord {
    file_id: u64,
    parent_file_id: u64,
    reason: u32,
    file_attributes: u32,
    name: String,
}

#[cfg(any(windows, test))]
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset + 2)
        .map(|value| u16::from_le_bytes([value[0], value[1]]))
}

#[cfg(any(windows, test))]
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(value.try_into().ok()?))
}

#[cfg(any(windows, test))]
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let value = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes(value.try_into().ok()?))
}

#[cfg(any(windows, test))]
fn parse_usn_v2_records(bytes: &[u8]) -> Result<Vec<ParsedUsnRecord>, String> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let record_length = read_u32(bytes, offset)
            .ok_or_else(|| "truncated USN record length".to_owned())?
            as usize;
        if record_length < 60 || offset.saturating_add(record_length) > bytes.len() {
            return Err("invalid USN record length".to_owned());
        }
        let major =
            read_u16(bytes, offset + 4).ok_or_else(|| "truncated USN record version".to_owned())?;
        if major != 2 {
            return Err(format!("unsupported USN record major version {major}"));
        }
        let file_id =
            read_u64(bytes, offset + 8).ok_or_else(|| "truncated USN file reference".to_owned())?;
        let parent_file_id = read_u64(bytes, offset + 16)
            .ok_or_else(|| "truncated USN parent reference".to_owned())?;
        let reason =
            read_u32(bytes, offset + 40).ok_or_else(|| "truncated USN reason".to_owned())?;
        let file_attributes = read_u32(bytes, offset + 52)
            .ok_or_else(|| "truncated USN file attributes".to_owned())?;
        let name_length = usize::from(
            read_u16(bytes, offset + 56).ok_or_else(|| "truncated USN name length".to_owned())?,
        );
        let name_offset = usize::from(
            read_u16(bytes, offset + 58).ok_or_else(|| "truncated USN name offset".to_owned())?,
        );
        if name_length % 2 != 0
            || name_offset < 60
            || name_offset.saturating_add(name_length) > record_length
        {
            return Err("invalid USN filename bounds".to_owned());
        }
        let name_bytes = &bytes[offset + name_offset..offset + name_offset + name_length];
        let utf16 = name_bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        records.push(ParsedUsnRecord {
            file_id,
            parent_file_id,
            reason,
            file_attributes,
            name: String::from_utf16_lossy(&utf16),
        });
        offset += record_length;
    }
    Ok(records)
}

#[cfg(any(windows, test))]
fn relative_child(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.replace('\\', "/")
    } else {
        format!(
            "{}/{}",
            parent.trim_end_matches('/'),
            name.replace('\\', "/")
        )
    }
}

#[cfg(any(windows, test))]
fn relative_allowed(relative: &str, exclusions: &ScanExclusions) -> bool {
    if crate::relative_path_pruned(relative, exclusions) {
        return false;
    }
    let path = Path::new(relative);
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        let name = component.as_os_str().to_string_lossy();
        if crate::standard_pruned_dir(&name, exclusions) {
            return false;
        }
    }
    true
}

#[cfg(windows)]
mod windows {
    use std::{
        collections::{BTreeMap, BTreeSet, HashMap},
        ffi::{c_void, OsStr},
        fs,
        os::windows::ffi::OsStrExt,
        path::{Component, Prefix},
        ptr,
    };

    use super::*;
    use crate::{content_index_eligible, modified_ns};

    type Handle = isize;
    const INVALID_HANDLE_VALUE: Handle = -1isize;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;
    const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;

    const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
    const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
    const USN_REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
    const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
    const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
    const USN_REASON_SECURITY_CHANGE: u32 = 0x0000_0800;
    const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
    const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
    const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
    const USN_REASON_HARD_LINK_CHANGE: u32 = 0x0001_0000;
    const USN_REASON_COMPRESSION_CHANGE: u32 = 0x0002_0000;
    const USN_REASON_ENCRYPTION_CHANGE: u32 = 0x0004_0000;
    const USN_REASON_REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
    const RELEVANT_REASON_MASK: u32 = USN_REASON_DATA_OVERWRITE
        | USN_REASON_DATA_EXTEND
        | USN_REASON_DATA_TRUNCATION
        | USN_REASON_FILE_CREATE
        | USN_REASON_FILE_DELETE
        | USN_REASON_SECURITY_CHANGE
        | USN_REASON_RENAME_OLD_NAME
        | USN_REASON_RENAME_NEW_NAME
        | USN_REASON_BASIC_INFO_CHANGE
        | USN_REASON_HARD_LINK_CHANGE
        | USN_REASON_COMPRESSION_CHANGE
        | USN_REASON_ENCRYPTION_CHANGE
        | USN_REASON_REPARSE_POINT_CHANGE;

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

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct UsnJournalDataV0 {
        journal_id: u64,
        first_usn: i64,
        next_usn: i64,
        lowest_valid_usn: i64,
        max_usn: i64,
        maximum_size: u64,
        allocation_delta: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ReadUsnJournalDataV0 {
        start_usn: i64,
        reason_mask: u32,
        return_only_on_close: u32,
        timeout: u64,
        bytes_to_wait_for: u64,
        journal_id: u64,
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
        fn DeviceIoControl(
            device: Handle,
            control_code: u32,
            input_buffer: *mut c_void,
            input_size: u32,
            output_buffer: *mut c_void,
            output_size: u32,
            bytes_returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn GetFileInformationByHandle(
            file: Handle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
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

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn open_path(path: &Path, directory: bool) -> Result<OwnedHandle, String> {
        let path = wide(path.as_os_str());
        let flags = if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null_mut(),
                OPEN_EXISTING,
                flags,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(OwnedHandle(handle))
    }

    fn drive_letter(root: &Path) -> Option<u8> {
        root.components().find_map(|component| match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter),
                _ => None,
            },
            _ => None,
        })
    }

    fn open_volume(root: &Path) -> Result<OwnedHandle, String> {
        let canonical = fs::canonicalize(root).map_err(|error| error.to_string())?;
        let letter = drive_letter(&canonical)
            .ok_or_else(|| "USN fast path requires a local drive-letter volume".to_owned())?;
        let volume = format!(r"\\.\{}:", char::from(letter));
        let wide_volume = wide(OsStr::new(&volume));
        let handle = unsafe {
            CreateFileW(
                wide_volume.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(OwnedHandle(handle))
    }

    fn query_journal(volume: Handle) -> Result<UsnJournalDataV0, String> {
        let mut data = UsnJournalDataV0::default();
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                volume,
                FSCTL_QUERY_USN_JOURNAL,
                ptr::null_mut(),
                0,
                (&mut data as *mut UsnJournalDataV0).cast(),
                std::mem::size_of::<UsnJournalDataV0>() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(data)
    }

    pub(super) fn directory_file_id(path: &Path) -> Option<u64> {
        let handle = open_path(path, true).ok()?;
        let mut info = ByHandleFileInformation::default();
        if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
            return None;
        }
        Some((u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low))
    }

    pub(super) fn capture_checkpoint(root: &Path) -> Result<Option<UsnCheckpoint>, String> {
        let volume = match open_volume(root) {
            Ok(volume) => volume,
            Err(_) => return Ok(None),
        };
        let journal = match query_journal(volume.0) {
            Ok(journal) => journal,
            Err(_) => return Ok(None),
        };
        Ok(Some(UsnCheckpoint {
            journal_id: journal.journal_id,
            next_usn: journal.next_usn,
        }))
    }

    fn read_records(
        volume: Handle,
        checkpoint: UsnCheckpoint,
        journal: UsnJournalDataV0,
    ) -> Result<(Vec<ParsedUsnRecord>, i64), String> {
        if journal.journal_id != checkpoint.journal_id {
            return Err("USN journal identifier changed".to_owned());
        }
        if checkpoint.next_usn < journal.first_usn || checkpoint.next_usn < journal.lowest_valid_usn
        {
            return Err("USN journal cursor was truncated".to_owned());
        }
        if checkpoint.next_usn > journal.next_usn {
            return Err("USN journal cursor is ahead of the current journal".to_owned());
        }
        if checkpoint.next_usn == journal.next_usn {
            return Ok((Vec::new(), journal.next_usn));
        }
        let target = journal.next_usn;
        let mut start = checkpoint.next_usn;
        let mut records = Vec::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        while start < target {
            let mut request = ReadUsnJournalDataV0 {
                start_usn: start,
                reason_mask: RELEVANT_REASON_MASK,
                return_only_on_close: 0,
                timeout: 0,
                bytes_to_wait_for: 0,
                journal_id: checkpoint.journal_id,
            };
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    volume,
                    FSCTL_READ_USN_JOURNAL,
                    (&mut request as *mut ReadUsnJournalDataV0).cast(),
                    std::mem::size_of::<ReadUsnJournalDataV0>() as u32,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(format!(
                    "USN journal read failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let returned = returned as usize;
            if returned < 8 {
                return Err("USN journal returned a truncated cursor".to_owned());
            }
            let next = i64::from_le_bytes(buffer[..8].try_into().expect("8-byte USN cursor"));
            records.extend(parse_usn_v2_records(&buffer[8..returned])?);
            if records.len() > MAX_USN_RECORDS {
                return Err(format!(
                    "USN journal delta exceeds the safe record limit ({MAX_USN_RECORDS})"
                ));
            }
            if next <= start {
                return Err("USN journal cursor did not advance".to_owned());
            }
            start = next;
        }
        Ok((records, start))
    }

    pub(super) fn scan_changes(
        root: &Path,
        checkpoint: UsnCheckpoint,
        directories: &DirectoryTrackingSnapshot,
        max_file_bytes: u64,
        exclusions: &ScanExclusions,
    ) -> Result<UsnScanResult, String> {
        if exclusions.use_gitignore || !exclusions.custom_globs.is_empty() {
            return Ok(UsnScanResult::FullScanRequired {
                reason: "USN fast path does not evaluate gitignore/custom glob changes".to_owned(),
            });
        }
        if let Err(reason) = directories.validate() {
            return Ok(UsnScanResult::FullScanRequired { reason });
        }
        let volume = match open_volume(root) {
            Ok(volume) => volume,
            Err(error) => return Ok(UsnScanResult::Unsupported { reason: error }),
        };
        let journal = match query_journal(volume.0) {
            Ok(journal) => journal,
            Err(error) => return Ok(UsnScanResult::Unsupported { reason: error }),
        };
        let (records, next_usn) = match read_records(volume.0, checkpoint, journal) {
            Ok(result) => result,
            Err(reason) => return Ok(UsnScanResult::FullScanRequired { reason }),
        };
        let next_checkpoint = UsnCheckpoint {
            journal_id: journal.journal_id,
            next_usn,
        };
        if records.is_empty() {
            return Ok(UsnScanResult::NoChanges {
                checkpoint: next_checkpoint,
            });
        }

        let mut directory_map = directories
            .directories
            .iter()
            .map(|directory| (directory.file_id, directory.relative_path.clone()))
            .collect::<HashMap<_, _>>();
        let mut upserts = BTreeMap::<String, ScannedFile>::new();
        let mut deleted = BTreeSet::<String>::new();
        let mut relevant_records = 0usize;

        for record in &records {
            if let Some(tracked_directory) = directory_map.get(&record.file_id) {
                let namespace_change = record.reason
                    & (USN_REASON_RENAME_OLD_NAME
                        | USN_REASON_RENAME_NEW_NAME
                        | USN_REASON_FILE_DELETE
                        | USN_REASON_HARD_LINK_CHANGE
                        | USN_REASON_REPARSE_POINT_CHANGE)
                    != 0;
                if namespace_change {
                    return Ok(UsnScanResult::FullScanRequired {
                        reason: format!(
                            "tracked directory namespace changed: {}",
                            tracked_directory
                        ),
                    });
                }
            }
            let Some(parent) = directory_map.get(&record.parent_file_id).cloned() else {
                continue;
            };
            let relative = relative_child(&parent, &record.name);
            let path = root.join(&relative);
            let is_directory = record.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0
                || directory_map.contains_key(&record.file_id)
                || (record.reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) != 0
                    && fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()));
            let unsafe_namespace_change = record.reason
                & (USN_REASON_RENAME_OLD_NAME
                    | USN_REASON_RENAME_NEW_NAME
                    | USN_REASON_FILE_DELETE
                    | USN_REASON_HARD_LINK_CHANGE
                    | USN_REASON_REPARSE_POINT_CHANGE)
                != 0;
            if is_directory {
                relevant_records += 1;
                if unsafe_namespace_change {
                    return Ok(UsnScanResult::FullScanRequired {
                        reason: format!("directory namespace changed: {relative}"),
                    });
                }
                if record.reason & USN_REASON_FILE_CREATE != 0
                    && record.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                    && relative_allowed(&relative, exclusions)
                {
                    directory_map.insert(record.file_id, relative);
                }
                continue;
            }
            if record.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                || record.reason & (USN_REASON_HARD_LINK_CHANGE | USN_REASON_REPARSE_POINT_CHANGE)
                    != 0
            {
                return Ok(UsnScanResult::FullScanRequired {
                    reason: format!("file namespace/reparse semantics changed: {relative}"),
                });
            }
            relevant_records += 1;
            if record.reason & (USN_REASON_FILE_DELETE | USN_REASON_RENAME_OLD_NAME) != 0 {
                upserts.remove(&relative);
                deleted.insert(relative.clone());
            }
            let may_exist = record.reason
                & (USN_REASON_DATA_OVERWRITE
                    | USN_REASON_DATA_EXTEND
                    | USN_REASON_DATA_TRUNCATION
                    | USN_REASON_FILE_CREATE
                    | USN_REASON_RENAME_NEW_NAME
                    | USN_REASON_BASIC_INFO_CHANGE
                    | USN_REASON_SECURITY_CHANGE
                    | USN_REASON_COMPRESSION_CHANGE
                    | USN_REASON_ENCRYPTION_CHANGE)
                != 0;
            if may_exist {
                match fs::metadata(&path) {
                    Ok(metadata)
                        if metadata.is_file()
                            && (max_file_bytes == 0 || metadata.len() <= max_file_bytes)
                            && relative_allowed(&relative, exclusions) =>
                    {
                        deleted.remove(&relative);
                        upserts.insert(
                            relative.clone(),
                            ScannedFile {
                                path: path.clone(),
                                display_path: relative,
                                size_bytes: metadata.len(),
                                modified_ns: modified_ns(&metadata),
                                index_content: content_index_eligible(&path),
                            },
                        );
                    }
                    Ok(_) | Err(_) => {
                        upserts.remove(&relative);
                        deleted.insert(relative);
                    }
                }
            }
        }

        if relevant_records == 0 {
            return Ok(UsnScanResult::NoChanges {
                checkpoint: next_checkpoint,
            });
        }
        let mut tracked_directories = directory_map
            .into_iter()
            .map(|(file_id, relative_path)| TrackedDirectory {
                file_id,
                relative_path,
            })
            .collect::<Vec<_>>();
        tracked_directories.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(UsnScanResult::Changes(UsnChangeSet {
            checkpoint: next_checkpoint,
            directories: DirectoryTrackingSnapshot::new(true, tracked_directories),
            upserts: upserts.into_values().collect(),
            deleted_paths: deleted.into_iter().collect(),
            journal_records: records.len(),
        }))
    }
}

#[cfg(not(windows))]
mod windows {
    use super::*;

    pub(super) fn capture_checkpoint(_root: &Path) -> Result<Option<UsnCheckpoint>, String> {
        Ok(None)
    }

    pub(super) fn scan_changes(
        _root: &Path,
        _checkpoint: UsnCheckpoint,
        _directories: &DirectoryTrackingSnapshot,
        _max_file_bytes: u64,
        _exclusions: &ScanExclusions,
    ) -> Result<UsnScanResult, String> {
        Ok(UsnScanResult::Unsupported {
            reason: "USN change tracking is available only on Windows NTFS volumes".to_owned(),
        })
    }
}

#[cfg(windows)]
pub(crate) fn directory_file_id(path: &Path) -> Option<u64> {
    windows::directory_file_id(path)
}

pub fn capture_usn_checkpoint(root: &Path) -> Result<Option<UsnCheckpoint>, String> {
    windows::capture_checkpoint(root)
}

pub fn scan_usn_changes(
    root: &Path,
    checkpoint: UsnCheckpoint,
    directories: &DirectoryTrackingSnapshot,
    max_file_bytes: u64,
    exclusions: &ScanExclusions,
) -> Result<UsnScanResult, String> {
    windows::scan_changes(root, checkpoint, directories, max_file_bytes, exclusions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usn_record(file_id: u64, parent: u64, reason: u32, attributes: u32, name: &str) -> Vec<u8> {
        let name = name.encode_utf16().collect::<Vec<_>>();
        let raw_len = 60 + name.len() * 2;
        let record_len = raw_len.div_ceil(8) * 8;
        let mut out = vec![0u8; record_len];
        out[0..4].copy_from_slice(&(record_len as u32).to_le_bytes());
        out[4..6].copy_from_slice(&2u16.to_le_bytes());
        out[8..16].copy_from_slice(&file_id.to_le_bytes());
        out[16..24].copy_from_slice(&parent.to_le_bytes());
        out[40..44].copy_from_slice(&reason.to_le_bytes());
        out[52..56].copy_from_slice(&attributes.to_le_bytes());
        out[56..58].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        out[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (index, unit) in name.into_iter().enumerate() {
            out[60 + index * 2..62 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
        out
    }

    #[test]
    fn parses_v2_records_and_unicode_names() {
        let mut bytes = usn_record(11, 7, 0x101, 0, "日本語.txt");
        bytes.extend(usn_record(12, 7, 0x2000, 0x10, "renamed"));
        let records = parse_usn_v2_records(&bytes).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].file_id, 11);
        assert_eq!(records[0].parent_file_id, 7);
        assert_eq!(records[0].reason, 0x101);
        assert_eq!(records[0].file_attributes, 0);
        assert_eq!(records[0].name, "日本語.txt");
        assert_eq!(records[1].name, "renamed");
    }

    #[test]
    fn rejects_unsupported_usn_record_versions() {
        let mut bytes = usn_record(1, 2, 1, 0, "a.txt");
        bytes[4..6].copy_from_slice(&3u16.to_le_bytes());
        assert!(parse_usn_v2_records(&bytes).is_err());
    }

    #[test]
    fn relative_filter_matches_standard_exclusions() {
        let exclusions = ScanExclusions {
            node_modules: true,
            ..ScanExclusions::default()
        };
        assert!(relative_allowed("src/main.rs", &exclusions));
        assert!(!relative_allowed("node_modules/pkg/index.js", &exclusions));
        assert_eq!(relative_child("src", "main.rs"), "src/main.rs");
        assert!(DirectoryTrackingSnapshot::new(
            true,
            vec![TrackedDirectory {
                file_id: 1,
                relative_path: String::new()
            }]
        )
        .validate()
        .is_ok());
    }
}
