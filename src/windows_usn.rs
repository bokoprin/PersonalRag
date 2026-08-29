use crate::usn::UsnRecordV2;
use std::io;

pub fn parse_usn_records_v2(mut bytes: &[u8]) -> io::Result<Vec<UsnRecordV2>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < 60 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short USN_RECORD_V2",
            ));
        }
        let record_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        if record_len < 60 || record_len > bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid USN record length",
            ));
        }
        let major = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        if major != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported USN major version",
            ));
        }
        let file_reference = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let parent_reference = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let usn = i64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let reason = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        let attributes = u32::from_le_bytes(bytes[52..56].try_into().unwrap());
        let name_len = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as usize;
        let name_offset = u16::from_le_bytes(bytes[58..60].try_into().unwrap()) as usize;
        let name_end = name_offset
            .checked_add(name_len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "USN name overflow"))?;
        if name_end > record_len || !name_len.is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid USN name range",
            ));
        }
        let units = bytes[name_offset..name_end]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        let name = String::from_utf16(&units)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid USN UTF-16"))?;
        out.push(UsnRecordV2 {
            file_reference,
            parent_reference,
            usn,
            reason,
            attributes,
            name,
        });
        bytes = &bytes[record_len..];
    }
    Ok(out)
}

#[cfg(windows)]
pub mod live {
    use super::parse_usn_records_v2;
    use crate::usn::{JournalBounds, UsnRecordV2};
    use std::ffi::c_void;
    use std::io;
    use std::ptr;

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1_isize as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
    const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;
    const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00b3;

    #[repr(C)]
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
    struct ReadUsnJournalDataV0 {
        start_usn: i64,
        reason_mask: u32,
        return_only_on_close: u32,
        timeout: u64,
        bytes_to_wait_for: u64,
        journal_id: u64,
    }

    #[repr(C)]
    struct MftEnumDataV0 {
        start_file_reference_number: u64,
        low_usn: i64,
        high_usn: i64,
    }

    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            creation: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn DeviceIoControl(
            handle: Handle,
            code: u32,
            input: *const c_void,
            input_len: u32,
            output: *mut c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    pub struct VolumeHandle(Handle);
    impl Drop for VolumeHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    impl VolumeHandle {
        pub fn open(volume: &str) -> io::Result<Self> {
            let mut wide = volume.encode_utf16().collect::<Vec<_>>();
            wide.push(0);
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        pub fn query_journal(&self) -> io::Result<JournalBounds> {
            let mut data = UsnJournalDataV0 {
                journal_id: 0,
                first_usn: 0,
                next_usn: 0,
                lowest_valid_usn: 0,
                max_usn: 0,
                maximum_size: 0,
                allocation_delta: 0,
            };
            let mut returned = 0_u32;
            let ok = unsafe {
                DeviceIoControl(
                    self.0,
                    FSCTL_QUERY_USN_JOURNAL,
                    ptr::null(),
                    0,
                    (&mut data as *mut UsnJournalDataV0).cast(),
                    std::mem::size_of::<UsnJournalDataV0>() as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(JournalBounds {
                journal_id: data.journal_id,
                first_usn: data.first_usn,
                next_usn: data.next_usn,
            })
        }

        pub fn read_journal(
            &self,
            start_usn: i64,
            journal_id: u64,
            reason_mask: u32,
            capacity: usize,
        ) -> io::Result<(i64, Vec<UsnRecordV2>)> {
            let input = ReadUsnJournalDataV0 {
                start_usn,
                reason_mask,
                return_only_on_close: 0,
                timeout: 0,
                bytes_to_wait_for: 0,
                journal_id,
            };
            let mut output = vec![0_u8; capacity.max(64 * 1024)];
            let mut returned = 0_u32;
            let ok = unsafe {
                DeviceIoControl(
                    self.0,
                    FSCTL_READ_USN_JOURNAL,
                    (&input as *const ReadUsnJournalDataV0).cast(),
                    std::mem::size_of::<ReadUsnJournalDataV0>() as u32,
                    output.as_mut_ptr().cast(),
                    output.len() as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let returned = returned as usize;
            if returned < 8 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short USN journal buffer",
                ));
            }
            let next_usn = i64::from_le_bytes(output[..8].try_into().unwrap());
            let records = parse_usn_records_v2(&output[8..returned])?;
            Ok((next_usn, records))
        }

        pub fn enum_mft(
            &self,
            start_frn: u64,
            low_usn: i64,
            high_usn: i64,
            capacity: usize,
        ) -> io::Result<(u64, Vec<UsnRecordV2>)> {
            let input = MftEnumDataV0 {
                start_file_reference_number: start_frn,
                low_usn,
                high_usn,
            };
            let mut output = vec![0_u8; capacity.max(64 * 1024)];
            let mut returned = 0_u32;
            let ok = unsafe {
                DeviceIoControl(
                    self.0,
                    FSCTL_ENUM_USN_DATA,
                    (&input as *const MftEnumDataV0).cast(),
                    std::mem::size_of::<MftEnumDataV0>() as u32,
                    output.as_mut_ptr().cast(),
                    output.len() as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let returned = returned as usize;
            if returned < 8 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short MFT enum buffer",
                ));
            }
            let next_frn = u64::from_le_bytes(output[..8].try_into().unwrap());
            let records = parse_usn_records_v2(&output[8..returned])?;
            Ok((next_frn, records))
        }
    }
}
