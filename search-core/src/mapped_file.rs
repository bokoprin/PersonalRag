use std::fs::File;
use std::io;
use std::path::Path;

pub struct MappedFile {
    inner: PlatformMap,
}

impl MappedFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        PlatformMap::open(path).map(|inner| Self { inner })
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.inner.as_slice()
    }
}

#[cfg(unix)]
struct PlatformMap {
    ptr: *mut core::ffi::c_void,
    len: usize,
}

#[cfg(unix)]
unsafe impl Send for PlatformMap {}
#[cfg(unix)]
unsafe impl Sync for PlatformMap {}

#[cfg(unix)]
impl PlatformMap {
    fn open(path: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;

        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| io::Error::other("mapped file too large"))?;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "empty mapped file",
            ));
        }
        const PROT_READ: i32 = 0x1;
        const MAP_PRIVATE: i32 = 0x02;
        unsafe extern "C" {
            fn mmap(
                addr: *mut core::ffi::c_void,
                length: usize,
                prot: i32,
                flags: i32,
                fd: i32,
                offset: i64,
            ) -> *mut core::ffi::c_void;
        }
        let ptr = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

#[cfg(unix)]
impl Drop for PlatformMap {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn munmap(addr: *mut core::ffi::c_void, length: usize) -> i32;
        }
        let _ = unsafe { munmap(self.ptr, self.len) };
    }
}

#[cfg(windows)]
struct PlatformMap {
    mapping: *mut core::ffi::c_void,
    view: *mut core::ffi::c_void,
    len: usize,
    _file: File,
}

#[cfg(windows)]
unsafe impl Send for PlatformMap {}
#[cfg(windows)]
unsafe impl Sync for PlatformMap {}

#[cfg(windows)]
impl PlatformMap {
    fn open(path: &Path) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;

        type Handle = *mut core::ffi::c_void;
        const PAGE_READONLY: u32 = 0x02;
        const FILE_MAP_READ: u32 = 0x0004;
        unsafe extern "system" {
            fn CreateFileMappingW(
                file: Handle,
                attrs: *mut core::ffi::c_void,
                protect: u32,
                max_size_high: u32,
                max_size_low: u32,
                name: *const u16,
            ) -> Handle;
            fn MapViewOfFile(
                mapping: Handle,
                desired_access: u32,
                file_offset_high: u32,
                file_offset_low: u32,
                bytes_to_map: usize,
            ) -> *mut core::ffi::c_void;
            fn CloseHandle(handle: Handle) -> i32;
        }
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| io::Error::other("mapped file too large"))?;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "empty mapped file",
            ));
        }
        let mapping = unsafe {
            CreateFileMappingW(
                file.as_raw_handle().cast(),
                core::ptr::null_mut(),
                PAGE_READONLY,
                0,
                0,
                core::ptr::null(),
            )
        };
        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
        if view.is_null() {
            let error = io::Error::last_os_error();
            let _ = unsafe { CloseHandle(mapping) };
            return Err(error);
        }
        Ok(Self {
            mapping,
            view,
            len,
            _file: file,
        })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.view.cast::<u8>(), self.len) }
    }
}

#[cfg(windows)]
impl Drop for PlatformMap {
    fn drop(&mut self) {
        type Handle = *mut core::ffi::c_void;
        unsafe extern "system" {
            fn UnmapViewOfFile(base: *const core::ffi::c_void) -> i32;
            fn CloseHandle(handle: Handle) -> i32;
        }
        let _ = unsafe { UnmapViewOfFile(self.view) };
        let _ = unsafe { CloseHandle(self.mapping as Handle) };
    }
}

#[cfg(not(any(unix, windows)))]
struct PlatformMap {
    bytes: Vec<u8>,
}

#[cfg(not(any(unix, windows)))]
impl PlatformMap {
    fn open(path: &Path) -> io::Result<Self> {
        std::fs::read(path).map(|bytes| Self { bytes })
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}
