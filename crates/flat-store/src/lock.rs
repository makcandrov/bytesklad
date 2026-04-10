#[cfg(windows)]
use std::{fs::File, io};
use std::{fs::OpenOptions, path::Path};

use crate::Error;

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> Result<(), io::Error> {
    use std::os::unix::io::AsRawFd;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn try_lock_exclusive(file: &File) -> Result<(), io::Error> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            !0,
            !0,
            &mut overlapped,
        )
    };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) struct LockFile(#[allow(unused)] File);

impl LockFile {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, Error> {
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.as_ref().join("lock"))?;
        try_lock_exclusive(&lock_file).map_err(|_| Error::Locked)?;
        Ok(Self(lock_file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_prevents_second_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = LockFile::new(dir.path()).unwrap();
        assert!(matches!(LockFile::new(dir.path()), Err(Error::Locked)));
    }

    #[test]
    fn test_lock_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _lock = LockFile::new(dir.path()).unwrap();
        }
        // Should succeed after the first lock is dropped.
        let _lock = LockFile::new(dir.path()).unwrap();
    }
}
