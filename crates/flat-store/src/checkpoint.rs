use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
};

use hashbrown::HashMap;

use crate::DataFile;

/// Each entry is 16 bytes: bucket size (u64 LE) + offset (u64 LE).
/// u64::MAX as bucket means the unsized file.
const ENTRY_SIZE: usize = 16;

pub(crate) struct Checkpoint {
    path: PathBuf,
}

impl Checkpoint {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn recover(
        &self,
        sized_files: &HashMap<usize, DataFile>,
        unsized_file: &DataFile,
        buckets: &[usize],
    ) -> Result<(), io::Error> {
        let data = match fs::read(&self.path) {
            Ok(data) if data.len() >= ENTRY_SIZE && data.len() % ENTRY_SIZE == 0 => data,
            Ok(_) => return Ok(()), // Corrupt or empty, treat as no checkpoint.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };

        let entries: HashMap<u64, u64> = data
            .chunks_exact(ENTRY_SIZE)
            .map(|chunk| {
                let bucket = u64::from_le_bytes(chunk[..8].try_into().unwrap());
                let offset = u64::from_le_bytes(chunk[8..].try_into().unwrap());
                (bucket, offset)
            })
            .collect();

        for &bucket in buckets {
            if let Some(&offset) = entries.get(&(bucket as u64)) {
                let file = &sized_files[&bucket];
                if file.offset() > offset {
                    file.truncate(offset)?;
                }
            }
        }

        if let Some(&offset) = entries.get(&u64::MAX)
            && unsized_file.offset() > offset
        {
            unsized_file.truncate(offset)?;
        }

        Ok(())
    }

    pub fn write(
        &self,
        sized_files: &HashMap<usize, DataFile>,
        unsized_file: &DataFile,
    ) -> Result<(), io::Error> {
        let mut buf = Vec::with_capacity((sized_files.len() + 1) * ENTRY_SIZE);
        for (&bucket, file) in sized_files {
            buf.extend_from_slice(&(bucket as u64).to_le_bytes());
            buf.extend_from_slice(&file.offset().to_le_bytes());
        }
        // Unsized file uses u64::MAX as sentinel.
        buf.extend_from_slice(&u64::MAX.to_le_bytes());
        buf.extend_from_slice(&unsized_file.offset().to_le_bytes());

        let tmp_path = self.path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)?;
        file.write_all(&buf)?;
        file.sync_data()?;
        fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }
}
