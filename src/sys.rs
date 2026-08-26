use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};

/// Fsync a directory so entries created or renamed inside it survive a crash.
/// A no-op on Windows, where directory handles cannot be flushed this way.
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Replace `path` with `contents` atomically: write a sibling temporary file,
/// fsync it, rename over the target, then fsync the parent directory. A reader
/// therefore only ever observes the complete previous or the complete next
/// version, never a partial one.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    file.write_all(contents)?;
    file.sync_data()?;
    drop(file);

    fs::rename(&tmp, path)?;

    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}
