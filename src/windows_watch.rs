#[cfg(windows)]
pub mod live {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1_isize as Handle;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    const WAIT_FAILED: u32 = u32::MAX;
    const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x0000_0001;
    const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x0000_0002;
    const FILE_NOTIFY_CHANGE_ATTRIBUTES: u32 = 0x0000_0004;
    const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x0000_0008;
    const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x0000_0010;
    const FILE_NOTIFY_CHANGE_CREATION: u32 = 0x0000_0040;

    unsafe extern "system" {
        fn FindFirstChangeNotificationW(
            path_name: *const u16,
            watch_subtree: i32,
            notify_filter: u32,
        ) -> Handle;
        fn FindNextChangeNotification(change_handle: Handle) -> i32;
        fn FindCloseChangeNotification(change_handle: Handle) -> i32;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    }

    pub struct ChangeNotification(Handle);

    impl ChangeNotification {
        pub fn open(root: &Path) -> io::Result<Self> {
            let mut wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
            wide.push(0);
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_ATTRIBUTES
                | FILE_NOTIFY_CHANGE_SIZE
                | FILE_NOTIFY_CHANGE_LAST_WRITE
                | FILE_NOTIFY_CHANGE_CREATION;
            let handle = unsafe { FindFirstChangeNotificationW(wide.as_ptr(), 1, filter) };
            if handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        pub fn poll_changed(&self) -> io::Result<bool> {
            match unsafe { WaitForSingleObject(self.0, 0) } {
                WAIT_TIMEOUT => Ok(false),
                WAIT_OBJECT_0 => {
                    if unsafe { FindNextChangeNotification(self.0) } == 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(true)
                    }
                }
                WAIT_FAILED => Err(io::Error::last_os_error()),
                value => Err(io::Error::other(format!(
                    "unexpected WaitForSingleObject result: {value}"
                ))),
            }
        }
    }

    impl Drop for ChangeNotification {
        fn drop(&mut self) {
            unsafe {
                FindCloseChangeNotification(self.0);
            }
        }
    }
}
