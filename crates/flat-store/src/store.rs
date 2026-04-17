use std::{fs, io, path::Path};

use hashbrown::HashMap;

use crate::{Checkpoint, DataFileRO, DataFileRW, FlatStoreRead, FlatStoreWrite, LockFile};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("store is already locked by another process")]
    Locked,
}

/// Read-write flat store. Only one writer may hold the store open at a time;
/// the lock is enforced via a lock file in the store directory.
#[derive(Debug)]
pub struct FlatStoreRW {
    sized_files: HashMap<usize, DataFileRW>,
    unsized_file: DataFileRW,
    checkpoint: Checkpoint,
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
            _lock_file: lock_file,
        })
    }

    fn file(&self, bucket: usize) -> &DataFileRW {
        self.sized_files.get(&bucket).unwrap_or(&self.unsized_file)
    }
}

impl FlatStoreRO {
    pub fn open(
        path: impl AsRef<Path>,
        buckets: impl IntoIterator<Item = usize>,
    ) -> Result<Self, Error> {
        let path = path.as_ref();

        let mut sized_files = HashMap::new();
        for bucket in buckets {
            let file_path = path.join(format!("size_{bucket}"));
            match DataFileRO::open(&file_path) {
                Ok(file) => {
                    sized_files.insert(bucket, file);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }

        let unsized_file = DataFileRO::open(path.join("unsized"))?;

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
