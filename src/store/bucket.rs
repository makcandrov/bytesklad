use std::{
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use parking_lot::Mutex;

use crate::{
    Error, Result,
    store::file::{Reader, Writer},
    varint,
};

/// Bytes read on the first, speculative read of a variable-length record.
/// Sized so that the length prefix and the payload of a typical small record
/// arrive in a single syscall; the kernel reads a whole page either way.
const PROBE_LEN: usize = 512;

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
        // Absent from the checkpoint means nothing in this bucket was ever
        // made durable, and therefore nothing committed to the index can
        // reference it: recovering to the very start is safe.
        recover(&path, frontier.unwrap_or(0))?;

        let writer = Writer::open(&path)?;
        Ok(Self {
            kind,
            path,
            reader: OnceLock::new(),
            active: Some(Mutex::new(writer)),
        })
    }

    pub fn read(&self, offset: u64) -> Result<Vec<u8>> {
        let reader = self.reader()?;

        match self.kind {
            Kind::Sized(len) => {
                let mut buf = vec![0u8; len];
                reader.read_exact_at(&mut buf, offset)?;
                Ok(buf)
            }
            Kind::Unsized => {
                let mut probe = [0u8; PROBE_LEN];
                let got = reader.read_at(&mut probe, offset)?;

                let (len, header_len) = varint::decode(&probe[..got])
                    .ok_or_else(|| Error::corrupt("bucket", "unreadable length prefix"))?;
                let len = usize::try_from(len)
                    .map_err(|_| Error::corrupt("bucket", "length prefix out of range"))?;

                let mut buf = vec![0u8; len];
                let inline = (got - header_len).min(len);
                buf[..inline].copy_from_slice(&probe[header_len..header_len + inline]);
                if inline < len {
                    // Payload ran past the probe; fetch the remainder exactly.
                    reader
                        .read_exact_at(&mut buf[inline..], offset + (header_len + inline) as u64)?;
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
        if let Kind::Unsized = self.kind {
            let mut header = [0u8; varint::MAX_ENCODED_LEN];
            let header_len = varint::encode(data.len() as u64, &mut header);
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

/// Discard everything written past the durable frontier.
fn recover(path: &Path, len: u64) -> Result<()> {
    let file = match OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    if file.metadata()?.len() > len {
        file.set_len(len)?;
        file.sync_all()?;
    }
    Ok(())
}
