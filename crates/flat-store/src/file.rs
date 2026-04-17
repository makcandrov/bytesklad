use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
};

use parking_lot::Mutex;

use crate::{FlatStoreRead, FlatStoreWrite};

#[derive(Debug)]
pub(crate) struct DataFileRW {
    writer: Mutex<Writer>,
    reader: Reader,
}

#[derive(Debug)]
pub(crate) struct DataFileRO {
    reader: Reader,
}

#[derive(Debug)]
struct Writer {
    file: File,
    offset: u64,
}

#[derive(Debug)]
struct Reader {
    file: File,
}

impl Reader {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let file = OpenOptions::new().read(true).open(&path)?;
        Ok(Self { file })
    }

    pub fn read(&self, buf: &mut [u8], offset: u64) -> Result<(), io::Error> {
        #[cfg(unix)]
        fn read_at(buf: &mut [u8], file: &File, offset: u64) -> Result<(), io::Error> {
            use std::os::unix::fs::FileExt;
            file.read_exact_at(buf, offset)
        }

        #[cfg(windows)]
        fn read_at(buf: &mut [u8], file: &File, offset: u64) -> Result<(), io::Error> {
            use std::os::windows::fs::FileExt;
            let len = buf.len();
            let mut pos = 0;
            while pos < len {
                let n = file.seek_read(&mut buf[pos..], offset + pos as u64)?;
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                pos += n;
            }
            Ok(())
        }

        read_at(buf, &self.file, offset)
    }
}

impl Writer {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let offset = file.metadata()?.len();
        Ok(Self { file, offset })
    }

    pub fn append(&mut self, data: &[u8]) -> Result<u64, io::Error> {
        let offset = self.offset;
        self.file.write_all(data)?;
        self.offset += data.len() as u64;
        Ok(offset)
    }

    pub fn truncate(&mut self, offset: u64) -> Result<(), io::Error> {
        self.file.set_len(offset)?;
        self.offset = offset;
        Ok(())
    }

    /// Fsync the file and return the current end-offset, captured atomically
    /// while the writer lock is held.
    pub fn sync(&mut self) -> Result<u64, io::Error> {
        self.file.sync_data()?;
        Ok(self.offset)
    }
}

impl DataFileRW {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        Ok(Self {
            writer: Mutex::new(Writer::new(&path)?),
            reader: Reader::new(&path)?,
        })
    }

    pub fn offset(&self) -> u64 {
        self.writer.lock().offset
    }

    pub fn truncate(&self, offset: u64) -> Result<(), io::Error> {
        self.writer.lock().truncate(offset)
    }

    /// Fsync and return the synced end-offset. Use this — not `sync()` followed
    /// by `offset()` — when callers need the offset to reflect what is actually
    /// durable on disk: a concurrent `insert` can otherwise advance `offset`
    /// past the synced point, which would let the checkpoint claim durability
    /// for bytes that haven't been fsynced yet.
    pub fn sync_to_offset(&self) -> Result<u64, io::Error> {
        self.writer.lock().sync()
    }
}

impl DataFileRO {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        Ok(Self {
            reader: Reader::new(path)?,
        })
    }
}

impl FlatStoreRead for DataFileRW {
    fn read(&self, buf: &mut [u8], offset: u64) -> Result<(), io::Error> {
        self.reader.read(buf, offset)
    }
}

impl FlatStoreWrite for DataFileRW {
    fn insert(&self, data: &[u8]) -> Result<u64, io::Error> {
        self.writer.lock().append(data)
    }

    fn sync(&self) -> Result<(), io::Error> {
        self.writer.lock().sync()?;
        Ok(())
    }
}

impl FlatStoreRead for DataFileRO {
    fn read(&self, buf: &mut [u8], offset: u64) -> Result<(), io::Error> {
        self.reader.read(buf, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_append_and_read() {
        let tmp = NamedTempFile::new().unwrap();
        let data_file = DataFileRW::open(tmp.path()).unwrap();

        let a = vec![0, 1, 2, 3, 4, 5, 6, 7, 8];
        let b = [0, 1, 2, 3];
        let c = [0, 1, 2, 3, 4, 5, 6];

        let ao = data_file.insert(&a).unwrap();
        let bo = data_file.insert(&b).unwrap();
        let co = data_file.insert(&c).unwrap();

        assert_eq!(data_file.read_to_vec(ao, a.len()).unwrap(), a);
        assert_eq!(data_file.read_to_array::<4>(bo).unwrap(), b);
        assert_eq!(data_file.read_to_array::<7>(co).unwrap(), c);
    }

    #[test]
    fn test_offsets_are_sequential() {
        let tmp = NamedTempFile::new().unwrap();
        let data_file = DataFileRW::open(tmp.path()).unwrap();

        let o1 = data_file.insert(&[0; 10]).unwrap();
        let o2 = data_file.insert(&[0; 5]).unwrap();
        let o3 = data_file.insert(&[0; 3]).unwrap();

        assert_eq!(o1, 0);
        assert_eq!(o2, 10);
        assert_eq!(o3, 15);
    }

    #[test]
    fn test_concurrent_reads_during_writes() {
        let tmp = NamedTempFile::new().unwrap();
        let data_file = DataFileRW::open(tmp.path()).unwrap();

        // Pre-populate with known data.
        let data: Vec<u8> = (0..=255).collect();
        let offset = data_file.insert(&data).unwrap();

        let data_file = std::sync::Arc::new(data_file);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        // Spawn a writer that continuously appends while readers are active.
        let writer = {
            let data_file = data_file.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0u8..=255 {
                    data_file.insert(&[i; 128]).unwrap();
                }
            })
        };

        // Spawn readers that repeatedly read the pre-populated data.
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let data_file = data_file.clone();
                let barrier = barrier.clone();
                let expected = data.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..1000 {
                        let result = data_file.read_to_vec(offset, expected.len()).unwrap();
                        assert_eq!(result, expected);
                    }
                })
            })
            .collect();

        for handle in readers {
            handle.join().unwrap();
        }
        writer.join().unwrap();
    }

    #[test]
    fn test_resumes_from_existing_file() {
        let tmp = NamedTempFile::new().unwrap();

        let o1 = {
            let data_file = DataFileRW::open(tmp.path()).unwrap();
            data_file.insert(&[1, 2, 3]).unwrap()
        };

        let data_file = DataFileRW::open(tmp.path()).unwrap();
        let o2 = data_file.insert(&[4, 5]).unwrap();

        assert_eq!(o2, 3);
        assert_eq!(data_file.read_to_array::<3>(o1).unwrap(), [1, 2, 3]);
        assert_eq!(data_file.read_to_array::<2>(o2).unwrap(), [4, 5]);
    }
}
