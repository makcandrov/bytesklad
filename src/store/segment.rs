use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
};

/// Read handle on one segment file. Shared by any number of threads; every
/// read is a positioned read, so no shared file cursor is involved.
#[derive(Debug)]
pub(crate) struct SegmentReader {
    file: File,
}

impl SegmentReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: OpenOptions::new().read(true).open(path)?,
        })
    }

    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(buf, offset)
        }
        #[cfg(windows)]
        {
            let mut read = 0;
            while read < buf.len() {
                match self.read_at(&mut buf[read..], offset + read as u64)? {
                    0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                    n => read += n,
                }
            }
            Ok(())
        }
    }

    /// Best-effort positioned read: may return fewer bytes than requested,
    /// including when the read runs into the end of the segment.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(buf, offset)
        }
    }
}

/// Append handle on the one segment currently being written.
///
/// `dirty` tracks whether any bytes have been appended since the last fsync,
/// so [`sync`](Self::sync) can skip the syscall entirely for untouched
/// segments — the write path fsyncs what it wrote, not every file it holds.
#[derive(Debug)]
pub(crate) struct SegmentWriter {
    file: File,
    len: u64,
    dirty: bool,
}

impl SegmentWriter {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            len,
            dirty: false,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn append(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all(data)?;
        self.len += data.len() as u64;
        self.dirty = true;
        Ok(())
    }

    /// Flush appended bytes to stable storage, and return the durable length.
    ///
    /// `fdatasync` is enough: it flushes the file size along with the data,
    /// which is all a subsequent positioned read needs.
    pub fn sync(&mut self) -> io::Result<u64> {
        if self.dirty {
            self.file.sync_data()?;
            self.dirty = false;
        }
        Ok(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_is_skipped_when_nothing_was_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.seg");
        let mut writer = SegmentWriter::open(&path).unwrap();

        // A freshly opened segment owes the disk nothing.
        assert!(!writer.dirty);
        assert_eq!(writer.sync().unwrap(), 0);

        writer.append(b"hello").unwrap();
        assert!(writer.dirty);

        assert_eq!(writer.sync().unwrap(), 5);
        assert!(!writer.dirty);

        // A second sync with no intervening append issues no fsync at all,
        // which is what keeps a write to one bucket from flushing the others.
        assert_eq!(writer.sync().unwrap(), 5);
        assert!(!writer.dirty);
    }

    #[test]
    fn writer_resumes_from_an_existing_segment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.seg");

        SegmentWriter::open(&path).unwrap().append(b"abc").unwrap();

        let writer = SegmentWriter::open(&path).unwrap();
        assert_eq!(writer.len(), 3);

        let reader = SegmentReader::open(&path).unwrap();
        let mut buf = [0u8; 3];
        reader.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"abc");
    }
}
