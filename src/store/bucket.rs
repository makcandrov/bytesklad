use std::{
    cmp::Ordering,
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use parking_lot::Mutex;

use crate::{
    Error, Result,
    store::{
        file::{Reader, Writer},
        pointer::{MAX_BUCKET_LEN, MAX_OFFSET},
    },
    varint,
};

/// Bytes read on the first, speculative read of a variable-length record.
/// Sized so that the length prefix and the payload of a typical small record
/// arrive in a single syscall; the kernel reads a whole page either way.
const PROBE_LEN: usize = 512;

const UNCHECKED_ALLOC_MAX: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Every record is exactly this many bytes, stored with no framing.
    Sized(usize),
    /// Records are framed as `[varint length][payload]`.
    Unsized,
}

/// One bucket: a single append-only file holding every record routed to it.
///
/// A record's address within the bucket is simply its byte offset in that
/// file, which is what the low 56 bits of a `Pointer` carry.
#[derive(Debug)]
pub(crate) struct Bucket {
    kind: Kind,
    path: PathBuf,
    /// Opened on first read rather than up front, because a reader can open a
    /// database in the window where the registry already names a bucket whose
    /// file the writer has not created yet.
    reader: OnceLock<Reader>,
    /// `Some` only for the writer; the mutex makes concurrent inserts on one
    /// bucket safe within the writing process.
    active: Option<Mutex<Writer>>,
}

impl Bucket {
    pub fn open_read_only(path: PathBuf, kind: Kind) -> Self {
        Self {
            kind,
            path,
            reader: OnceLock::new(),
            active: None,
        }
    }

    pub fn open_writable(path: PathBuf, kind: Kind, frontier: Option<u64>) -> Result<Self> {
        // Only an explicit durable frontier authorizes discarding bytes.
        // Missing recovery metadata must not erase an existing bucket.
        recover(&path, frontier)?;

        let writer = Writer::open(&path)?;
        Ok(Self {
            kind,
            path,
            reader: OnceLock::new(),
            active: Some(Mutex::new(writer)),
        })
    }

    pub fn read(&self, offset: u64) -> Result<Vec<u8>> {
        if offset > MAX_OFFSET {
            return Err(Error::corrupt("bucket", "record offset out of range"));
        }
        let reader = self.reader()?;

        match self.kind {
            Kind::Sized(len) => {
                let len = checked_record_len(reader, offset, 0, len as u64)?;
                let mut buf = allocate_record(len)?;
                read_record_at(reader, &mut buf, offset)?;
                Ok(buf)
            }
            Kind::Unsized => {
                let mut probe = [0u8; PROBE_LEN];
                let got = reader.read_at(&mut probe, offset)?;

                let (len, header_len) = varint::decode(&probe[..got])
                    .ok_or_else(|| Error::corrupt("bucket", "unreadable length prefix"))?;
                let len = checked_record_len(reader, offset, header_len, len)?;

                let mut buf = allocate_record(len)?;
                let inline = (got - header_len).min(len);
                buf[..inline].copy_from_slice(&probe[header_len..header_len + inline]);
                if inline < len {
                    // Payload ran past the probe; fetch the remainder exactly.
                    read_record_at(
                        reader,
                        &mut buf[inline..],
                        offset + (header_len + inline) as u64,
                    )?;
                }
                Ok(buf)
            }
        }
    }

    /// Append `data` and return its offset within this bucket.
    pub fn append(&self, data: &[u8]) -> Result<u64> {
        let active = self
            .active
            .as_ref()
            .expect("append on a read-only bucket is unreachable");
        let mut writer = active.lock();

        let offset = writer.len();
        let mut header = [0u8; varint::MAX_ENCODED_LEN];
        let header_len = match self.kind {
            Kind::Unsized => varint::encode(data.len() as u64, &mut header),
            Kind::Sized(_) => 0,
        };
        if offset > MAX_OFFSET
            || offset
                .checked_add(header_len as u64)
                .and_then(|start| start.checked_add(data.len() as u64))
                .is_none_or(|end| end > MAX_BUCKET_LEN)
        {
            return Err(Error::StoreFull {
                max: MAX_BUCKET_LEN,
            });
        }
        if header_len > 0 {
            writer.append(&header[..header_len])?;
        }
        writer.append(data)?;

        Ok(offset)
    }

    /// Flush this bucket and report its durable frontier. Cheap when nothing
    /// was appended since the last call: no fsync is issued.
    pub fn sync(&self) -> Result<u64> {
        let active = self
            .active
            .as_ref()
            .expect("sync on a read-only bucket is unreachable");
        let mut writer = active.lock();
        Ok(writer.sync()?)
    }

    fn reader(&self) -> Result<&Reader> {
        if let Some(reader) = self.reader.get() {
            return Ok(reader);
        }
        let reader = Reader::open(&self.path)?;
        // A racing thread may have won; either handle reads the same bytes.
        Ok(self.reader.get_or_init(|| reader))
    }
}

/// Validate the complete record before using an on-disk size for allocation.
fn checked_record_len(reader: &Reader, offset: u64, header_len: usize, len: u64) -> Result<usize> {
    let end = offset
        .checked_add(header_len as u64)
        .and_then(|start| start.checked_add(len))
        .filter(|&end| end <= MAX_BUCKET_LEN)
        .ok_or_else(|| Error::corrupt("bucket", "record length out of range"))?;
    let len =
        usize::try_from(len).map_err(|_| Error::corrupt("bucket", "record length out of range"))?;
    // Small buffers are harmless, and the read itself detects truncation.
    if len > UNCHECKED_ALLOC_MAX && end > reader.len()? {
        return Err(Error::corrupt("bucket", "record extends past end of file"));
    }
    Ok(len)
}

fn read_record_at(reader: &Reader, buf: &mut [u8], offset: u64) -> Result<()> {
    reader
        .read_exact_at(buf, offset)
        .map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => {
                Error::corrupt("bucket", "record extends past end of file")
            }
            _ => e.into(),
        })
}

fn allocate_record(len: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(io::Error::other)?;
    buf.resize(len, 0);
    Ok(buf)
}

/// Discard everything written past the durable frontier.
fn recover(path: &Path, frontier: Option<u64>) -> Result<()> {
    let len = frontier.unwrap_or(0);
    if len > MAX_BUCKET_LEN {
        return Err(Error::corrupt("checkpoint", "frontier out of range"));
    }
    let file = match OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound && len == 0 => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Error::corrupt("bucket", "checkpointed bucket is missing"));
        }
        Err(e) => return Err(e.into()),
    };

    let actual_len = file.metadata()?.len();
    if frontier.is_none() && actual_len > 0 {
        return Err(Error::corrupt(
            "checkpoint",
            "nonempty bucket has no frontier",
        ));
    }

    match actual_len.cmp(&len) {
        Ordering::Less => Err(Error::corrupt(
            "bucket",
            "bucket is shorter than checkpoint",
        )),
        Ordering::Greater => {
            file.set_len(len)?;
            file.sync_all()?;
            Ok(())
        }
        Ordering::Equal => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_record_lengths_return_errors_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsized.bucket");
        for len in [u64::MAX, MAX_OFFSET, 1024] {
            let mut header = [0u8; varint::MAX_ENCODED_LEN];
            let header_len = varint::encode(len, &mut header);
            std::fs::write(&path, &header[..header_len]).unwrap();
            let bucket = Bucket::open_read_only(path.clone(), Kind::Unsized);
            assert!(matches!(bucket.read(0), Err(Error::Corrupt { .. })));
        }
    }

    #[test]
    fn sized_records_must_fit_within_the_file_and_address_space() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sized.bucket");
        std::fs::write(&path, b"short").unwrap();
        for len in [usize::MAX, 32] {
            let bucket = Bucket::open_read_only(path.clone(), Kind::Sized(len));
            assert!(matches!(bucket.read(0), Err(Error::Corrupt { .. })));
        }
        let bucket = Bucket::open_read_only(path, Kind::Sized(7));
        assert!(matches!(
            bucket.read(MAX_OFFSET),
            Err(Error::Corrupt { .. })
        ));
        assert!(matches!(
            bucket.read(MAX_OFFSET + 1),
            Err(Error::Corrupt { .. })
        ));
    }

    #[test]
    fn recovery_rejects_missing_or_truncated_committed_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsized.bucket");
        assert!(matches!(
            Bucket::open_writable(path.clone(), Kind::Unsized, Some(8)),
            Err(Error::Corrupt { .. })
        ));
        assert!(!path.exists());

        std::fs::write(&path, b"short").unwrap();
        assert!(matches!(
            Bucket::open_writable(path.clone(), Kind::Unsized, Some(8)),
            Err(Error::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(path).unwrap(), b"short");
    }

    #[test]
    fn missing_frontier_does_not_erase_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsized.bucket");
        std::fs::write(&path, b"existing data").unwrap();
        assert!(matches!(
            Bucket::open_writable(path.clone(), Kind::Unsized, None),
            Err(Error::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(path).unwrap(), b"existing data");
    }
}
