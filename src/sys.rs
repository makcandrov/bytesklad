use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
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
    let (tmp, mut file) = create_temp(path)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_data()?;
        drop(file);

        fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn create_temp(path: &Path) -> io::Result<(std::path::PathBuf, fs::File)> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let mut name = temp_prefix(path);
        name.push(format!("{}.{sequence}", std::process::id()));
        let tmp = path.with_file_name(name);
        // Exclusive creation prevents an existing file or link from being
        // opened and truncated. Stale names from a crashed process are skipped.
        match OpenOptions::new().create_new(true).write(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot create temporary metadata file",
    ))
}

/// Delete temp files a crashed `atomic_write` left for `path`; callers must exclude other writers.
pub(crate) fn remove_stale_temps(path: &Path) -> io::Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    let prefix = temp_prefix(path);
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(prefix.as_encoded_bytes())
        {
            match fs::remove_file(entry.path()) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
    }
    Ok(())
}

fn temp_prefix(path: &Path) -> OsString {
    let mut prefix = path.file_name().unwrap_or_default().to_os_string();
    prefix.push(".tmp.");
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_metadata_without_touching_existing_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint");
        let stale = path.with_extension("tmp");
        fs::write(&stale, b"preserve this file").unwrap();
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(fs::read(&stale).unwrap(), b"preserve this file");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn failed_replacement_removes_its_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint");
        fs::create_dir(&path).unwrap();
        assert!(atomic_write(&path, b"metadata").is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn only_the_targets_temporary_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "checkpoint",
            "checkpoint.tmp",
            "checkpoint.tmp.17.0",
            "checkpoint.tmp.42.3",
            "registry.tmp.17.0",
        ] {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        remove_stale_temps(&dir.path().join("checkpoint")).unwrap();
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["checkpoint", "checkpoint.tmp", "registry.tmp.17.0"]);
    }
}
