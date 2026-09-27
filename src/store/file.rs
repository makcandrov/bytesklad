use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
};

/// Read handle on one bucket file. Shared by any number of threads; every read
/// is a positioned read, so no shared file cursor is involved.
#[derive(Debug)]
pub(crate) struct Reader {
    file: File,
}

impl Reader {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: OpenOptions::new().read(true).open(path)?,
        })
    }

    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
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
    /// including when the read runs into the end of the file.
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

/// Append handle on one bucket file.
///
/// `dirty` tracks whether any bytes have been appended since the last fsync,
/// so [`sync`](Self::sync) can skip the syscall entirely for untouched
/// buckets — the write path fsyncs what it wrote, not every file it holds.
#[derive(Debug)]
pub(crate) struct Writer {
    file: File,
    len: u64,
    dirty: bool,
}

impl Writer {
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
        append_counted(&mut self.file, data, &mut self.len, &mut self.dirty)
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

// A failed write may still have appended bytes. Account for each successful
// write before trying again, so the next record starts at the physical EOF
// even after an error such as a full disk.
fn append_counted(
    file: &mut impl Write,
    mut data: &[u8],
    len: &mut u64,
    dirty: &mut bool,
) -> io::Result<()> {
    len.checked_add(data.len() as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file length overflow"))?;
    while !data.is_empty() {
        match file.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => {
                *len += written as u64;
                *dirty = true;
                data = &data[written..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_is_skipped_when_nothing_was_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsized.bucket");
        let mut writer = Writer::open(&path).unwrap();

        // A freshly opened bucket owes the disk nothing.
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
    fn writer_resumes_from_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsized.bucket");

        Writer::open(&path).unwrap().append(b"abc").unwrap();

        let writer = Writer::open(&path).unwrap();
        assert_eq!(writer.len(), 3);

        let reader = Reader::open(&path).unwrap();
        let mut buf = [0u8; 3];
        reader.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"abc");
    }

    #[test]
    fn partial_write_error_preserves_the_next_record_offset() {
        struct LimitedWriter {
            bytes: Vec<u8>,
            remaining: usize,
        }
        impl Write for LimitedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    return Err(io::Error::other("injected storage failure"));
                }
                let written = bytes.len().min(self.remaining);
                self.bytes.extend_from_slice(&bytes[..written]);
                self.remaining -= written;
                Ok(written)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut file = LimitedWriter {
            bytes: Vec::new(),
            remaining: 3,
        };
        let mut len = 0;
        let mut dirty = false;
        assert!(append_counted(&mut file, b"failed record", &mut len, &mut dirty).is_err());
        assert_eq!(len, 3);
        assert!(dirty);

        let next_offset = len as usize;
        file.remaining = usize::MAX;
        append_counted(&mut file, b"next record", &mut len, &mut dirty).unwrap();
        assert_eq!(&file.bytes[next_offset..], b"next record");
        assert_eq!(len, file.bytes.len() as u64);
    }
}
