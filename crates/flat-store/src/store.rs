use std::{fs, io, path::Path};

use hashbrown::HashMap;

use crate::{Checkpoint, DataFile, LockFile};

pub struct FlatStore {
    sized_files: HashMap<usize, DataFile>,
    unsized_file: DataFile,
    checkpoint: Checkpoint,
    _lock_file: LockFile,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("store is already locked by another process")]
    Locked,
}

impl FlatStore {
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
            let file = DataFile::open(path.join(format!("size_{bucket}")))?;
            sized_files.insert(bucket, file);
        }

        let unsized_file = DataFile::open(path.join("unsized"))?;

        let checkpoint = Checkpoint::new(path.join("checkpoint"));
        checkpoint.recover(&sized_files, &unsized_file, &buckets)?;

        Ok(Self {
            sized_files,
            unsized_file,
            checkpoint,
            _lock_file: lock_file,
        })
    }

    fn file(&self, bucket: usize) -> &DataFile {
        self.sized_files.get(&bucket).unwrap_or(&self.unsized_file)
    }

    pub fn insert(&self, data: &[u8]) -> Result<u64, io::Error> {
        self.file(data.len()).append(data)
    }

    pub fn read(&self, buf: &mut [u8], offset: u64) -> Result<(), io::Error> {
        self.file(buf.len()).read(buf, offset)
    }

    pub fn read_to_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>, io::Error> {
        let mut buf = vec![0; len];
        self.read(&mut buf, offset)?;
        Ok(buf)
    }

    pub fn read_to_array<const N: usize>(&self, offset: u64) -> Result<[u8; N], io::Error> {
        let mut buf = [0; N];
        self.read(&mut buf, offset)?;
        Ok(buf)
    }

    /// Flush all data files to disk and write a checkpoint.
    pub fn sync(&self) -> Result<(), io::Error> {
        for file in self.sized_files.values() {
            file.sync()?;
        }
        self.unsized_file.sync()?;
        self.checkpoint
            .write(&self.sized_files, &self.unsized_file)?;
        Ok(())
    }
}
