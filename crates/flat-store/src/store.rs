use std::{fs, io, path::Path};

use hashbrown::HashMap;
use parking_lot::Mutex;

use crate::{Checkpoint, DataFileRO, DataFileRW, FlatStoreRead, FlatStoreWrite, LockFile};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("store is already locked by another process")]
    Locked,

    #[error("store does not exist or has not been initialized")]
    Empty,
}

/// Read-write flat store. Only one writer process may hold the store open at a
/// time (enforced via a lock file in the store directory). Within that
/// process, the store is `Send + Sync`: `insert` may be called from multiple
/// threads and serialization of `sync` is handled internally.
#[derive(Debug)]
pub struct FlatStoreRW {
    sized_files: HashMap<usize, DataFileRW>,
    unsized_file: DataFileRW,
    checkpoint: Checkpoint,
    /// Held for the whole of `sync()` so concurrent callers don't race on the
    /// `checkpoint.tmp` rename and so checkpoint offsets advance monotonically.
    sync_lock: Mutex<()>,
    _lock_file: LockFile,
}

/// Read-only flat store. Any number of read-only handles may coexist with a
/// single writer — no lock is taken and no recovery is run.
#[derive(Debug)]
pub struct FlatStoreRO {
    sized_files: HashMap<usize, DataFileRO>,
    unsized_file: DataFileRO,
}

impl FlatStoreRW {
    pub fn open(
        path: impl AsRef<Path>,
        buckets: impl IntoIterator<Item = usize>,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        fs::create_dir_all(path)?;

        let lock_file = LockFile::new(path)?;

        let mut sized_files = HashMap::new();
        let buckets: Vec<usize> = buckets.into_iter().collect();

        for &bucket in &buckets {
            let file = DataFileRW::open(path.join(format!("size_{bucket}")))?;
            sized_files.insert(bucket, file);
        }

        let unsized_file = DataFileRW::open(path.join("unsized"))?;

        let checkpoint = Checkpoint::new(path.join("checkpoint"));
        checkpoint.recover(&sized_files, &unsized_file, &buckets)?;

        Ok(Self {
            sized_files,
            unsized_file,
            checkpoint,
            sync_lock: Mutex::new(()),
            _lock_file: lock_file,
        })
    }

    fn file(&self, bucket: usize) -> &DataFileRW {
        self.sized_files.get(&bucket).unwrap_or(&self.unsized_file)
    }
}

impl FlatStoreRO {
    /// Open the store read-only. Buckets are discovered by enumerating
    /// `size_{N}` files in `path`, so the reader always matches the writer's
    /// actual on-disk layout rather than a user-supplied (and possibly stale)
    /// bucket list.
    ///
    /// Returns [`Error::Empty`] if the store directory doesn't exist or hasn't
    /// been initialized by a writer yet (i.e. the `unsized` data file is
    /// absent). The writer creates that file unconditionally on first open.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();

        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::Empty),
            Err(e) => return Err(e.into()),
        };

        let mut sized_files = HashMap::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix("size_") else {
                continue;
            };
            let Ok(bucket) = rest.parse::<usize>() else {
                continue;
            };
            let file = DataFileRO::open(entry.path())?;
            sized_files.insert(bucket, file);
        }

        let unsized_file = match DataFileRO::open(path.join("unsized")) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::Empty),
            Err(e) => return Err(e.into()),
        };

        Ok(Self {
            sized_files,
            unsized_file,
        })
    }

    fn file(&self, bucket: usize) -> &DataFileRO {
        self.sized_files.get(&bucket).unwrap_or(&self.unsized_file)
    }
}

impl FlatStoreRead for FlatStoreRW {
    fn read(&self, buf: &mut [u8], offset: u64) -> Result<(), io::Error> {
        self.file(buf.len()).read(buf, offset)
    }
}

impl FlatStoreWrite for FlatStoreRW {
    fn insert(&self, data: &[u8]) -> Result<u64, io::Error> {
        self.file(data.len()).insert(data)
    }

    fn sync(&self) -> Result<(), io::Error> {
        let _sync_guard = self.sync_lock.lock();
        for file in self.sized_files.values() {
            file.sync()?;
        }
        self.unsized_file.sync()?;
        self.checkpoint
            .write(&self.sized_files, &self.unsized_file)?;
        Ok(())
    }
}

impl FlatStoreRead for FlatStoreRO {
    fn read(&self, buf: &mut [u8], offset: u64) -> Result<(), io::Error> {
        self.file(buf.len()).read(buf, offset)
    }
}
