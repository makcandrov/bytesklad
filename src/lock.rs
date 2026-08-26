use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

use crate::Error;

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn try_lock_exclusive(file: &File) -> io::Result<()> {
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

/// Advisory exclusive lock held by the single writer for as long as it has the
/// database open. Released by the OS if the process dies.
#[derive(Debug)]
pub(crate) struct LockFile(#[allow(unused)] File);

impl LockFile {
    pub fn acquire(root: &Path) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join("LOCK"))?;
        try_lock_exclusive(&file).map_err(|_| Error::Locked)?;
        Ok(Self(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = LockFile::acquire(dir.path()).unwrap();
        assert!(matches!(LockFile::acquire(dir.path()), Err(Error::Locked)));
    }

    #[test]
    fn lock_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        drop(LockFile::acquire(dir.path()).unwrap());
        let _lock = LockFile::acquire(dir.path()).unwrap();
    }
}
