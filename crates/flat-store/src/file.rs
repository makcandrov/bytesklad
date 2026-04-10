use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
};

use parking_lot::Mutex;

pub(crate) struct DataFile {
    writer: Mutex<Writer>,
    reader: File,
}

struct Writer {
    file: File,
    offset: u64,
}

impl DataFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let writer = OpenOptions::new().create(true).append(true).open(&path)?;
        let reader = OpenOptions::new().read(true).open(path)?;
        let write_offset = writer.metadata()?.len();
        Ok(Self {
            writer: Mutex::new(Writer {
                file: writer,
                offset: write_offset,
            }),
            reader,
        })
    }

    pub fn append(&self, data: &[u8]) -> Result<u64, io::Error> {
        let mut w = self.writer.lock();
        let offset = w.offset;
        w.file.write_all(data)?;
        w.offset += data.len() as u64;
        Ok(offset)
    }

    pub fn read(&self, buf: &mut [u8], offset: u64) -> Result<(), io::Error> {
        read_at(buf, &self.reader, offset)
    }

    #[cfg(test)]
    pub fn read_to_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>, io::Error> {
        let mut buf = vec![0; len];
        self.read(&mut buf, offset)?;
        Ok(buf)
    }

    #[cfg(test)]
    pub fn read_to_array<const N: usize>(&self, offset: u64) -> Result<[u8; N], io::Error> {
        let mut buf = [0; N];
        self.read(&mut buf, offset)?;
        Ok(buf)
    }

    pub fn offset(&self) -> u64 {
        self.writer.lock().offset
    }

    pub fn truncate(&self, offset: u64) -> Result<(), io::Error> {
        let mut w = self.writer.lock();
        w.file.set_len(offset)?;
        w.offset = offset;
        Ok(())
    }

    pub fn sync(&self) -> Result<(), io::Error> {
        self.writer.lock().file.sync_data()?;
        Ok(())
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_append_and_read() {
        let tmp = NamedTempFile::new().unwrap();
        let data_file = DataFile::open(tmp.path()).unwrap();

        let a = vec![0, 1, 2, 3, 4, 5, 6, 7, 8];
        let b = [0, 1, 2, 3];
        let c = [0, 1, 2, 3, 4, 5, 6];

        let ao = data_file.append(&a).unwrap();
        let bo = data_file.append(&b).unwrap();
        let co = data_file.append(&c).unwrap();

        assert_eq!(data_file.read_to_vec(ao, a.len()).unwrap(), a);
        assert_eq!(data_file.read_to_array::<4>(bo).unwrap(), b);
        assert_eq!(data_file.read_to_array::<7>(co).unwrap(), c);
    }

    #[test]
    fn test_offsets_are_sequential() {
        let tmp = NamedTempFile::new().unwrap();
        let data_file = DataFile::open(tmp.path()).unwrap();

        let o1 = data_file.append(&[0; 10]).unwrap();
        let o2 = data_file.append(&[0; 5]).unwrap();
        let o3 = data_file.append(&[0; 3]).unwrap();

        assert_eq!(o1, 0);
        assert_eq!(o2, 10);
        assert_eq!(o3, 15);
    }

    #[test]
    fn test_concurrent_reads_during_writes() {
        let tmp = NamedTempFile::new().unwrap();
        let data_file = DataFile::open(tmp.path()).unwrap();

        // Pre-populate with known data.
        let data: Vec<u8> = (0..=255).collect();
        let offset = data_file.append(&data).unwrap();

        let data_file = std::sync::Arc::new(data_file);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        // Spawn a writer that continuously appends while readers are active.
        let writer = {
            let data_file = data_file.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0u8..=255 {
                    data_file.append(&[i; 128]).unwrap();
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
            let data_file = DataFile::open(tmp.path()).unwrap();
            data_file.append(&[1, 2, 3]).unwrap()
        };

        let data_file = DataFile::open(tmp.path()).unwrap();
        let o2 = data_file.append(&[4, 5]).unwrap();

        assert_eq!(o2, 3);
        assert_eq!(data_file.read_to_array::<3>(o1).unwrap(), [1, 2, 3]);
        assert_eq!(data_file.read_to_array::<2>(o2).unwrap(), [4, 5]);
    }
}
