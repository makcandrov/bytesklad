use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::{
    Error, Result,
    store::segment::{SegmentReader, SegmentWriter},
    sys, varint,
};

/// Bytes read on the first, speculative read of a variable-length record.
/// Sized so that the length prefix and the payload of a typical small record
/// arrive in a single syscall; the kernel reads a whole page either way.
const PROBE_LEN: usize = 512;

const SEGMENT_EXT: &str = "seg";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Every record is exactly this many bytes, stored with no framing.
    Sized(usize),
    /// Records are framed as `[varint length][payload]`.
    Unsized,
}

#[derive(Debug)]
struct Active {
    id: u32,
    writer: SegmentWriter,
}

/// One bucket: a directory of append-only segment files holding records that
/// are all routed the same way.
///
/// A bucket's logical address space is `segment_index * segment_size + offset
/// within segment`, so a single 56-bit offset locates a record in it. Records
/// never straddle a segment boundary; the unused tail left behind when one is
/// sealed early is at most one record long.
#[derive(Debug)]
pub(crate) struct Bucket {
    kind: Kind,
    segment_size: u64,
    dir: PathBuf,
    /// Segment readers are opened on demand and cached, so a reader process
    /// picks up segments the writer created after the reader started.
    readers: RwLock<FxHashMap<u32, Arc<SegmentReader>>>,
    /// `Some` only for the writer; the mutex makes concurrent inserts on one
    /// bucket safe within the writing process.
    active: Option<Mutex<Active>>,
}

impl Bucket {
    pub fn open_read_only(dir: PathBuf, kind: Kind, segment_size: u64) -> Self {
        Self {
            kind,
            segment_size,
            dir,
            readers: RwLock::new(FxHashMap::default()),
            active: None,
        }
    }

    pub fn open_writable(
        dir: PathBuf,
        kind: Kind,
        segment_size: u64,
        frontier: Option<(u32, u64)>,
    ) -> Result<Self> {
        fs::create_dir_all(&dir)?;

        // Absent from the checkpoint means nothing in this bucket was ever
        // made durable, and therefore nothing committed to the index can
        // reference it: recovering to the very start is safe.
        let (segment, len) = frontier.unwrap_or((0, 0));
        recover(&dir, segment, len)?;

        let writer = SegmentWriter::open(&segment_path(&dir, segment))?;
        sys::sync_dir(&dir)?;

        Ok(Self {
            kind,
            segment_size,
            dir,
            readers: RwLock::new(FxHashMap::default()),
            active: Some(Mutex::new(Active {
                id: segment,
                writer,
            })),
        })
    }

    pub fn read(&self, offset: u64) -> Result<Vec<u8>> {
        let id = (offset / self.segment_size) as u32;
        let intra = offset % self.segment_size;
        let segment = self.segment(id)?;

        match self.kind {
            Kind::Sized(len) => {
                let mut buf = vec![0u8; len];
                segment.read_exact_at(&mut buf, intra)?;
                Ok(buf)
            }
            Kind::Unsized => {
                let mut probe = [0u8; PROBE_LEN];
                let got = segment.read_at(&mut probe, intra)?;

                let (len, header_len) = varint::decode(&probe[..got])
                    .ok_or_else(|| Error::corrupt("segment", "unreadable length prefix"))?;
                let len = usize::try_from(len)
                    .map_err(|_| Error::corrupt("segment", "length prefix out of range"))?;

                let mut buf = vec![0u8; len];
                let inline = (got - header_len).min(len);
                buf[..inline].copy_from_slice(&probe[header_len..header_len + inline]);
                if inline < len {
                    // Payload ran past the probe; fetch the remainder exactly.
                    segment
                        .read_exact_at(&mut buf[inline..], intra + (header_len + inline) as u64)?;
                }
                Ok(buf)
            }
        }
    }

    /// Append `data` and return its logical offset within this bucket.
    pub fn append(&self, data: &[u8]) -> Result<u64> {
        let active = self
            .active
            .as_ref()
            .expect("append on a read-only bucket is unreachable");
        let mut guard = active.lock();

        let mut header = [0u8; varint::MAX_ENCODED_LEN];
        let header_len = match self.kind {
            Kind::Sized(_) => 0,
            Kind::Unsized => varint::encode(data.len() as u64, &mut header),
        };
        let record_len = (header_len + data.len()) as u64;

        if guard.writer.len() > 0 && guard.writer.len() + record_len > self.segment_size {
            // Seal before rolling: once the checkpoint advances past this
            // segment nothing will ever fsync it again, so it must be durable
            // now. A record wider than a whole segment gets one to itself.
            guard.writer.sync()?;
            let id = guard.id + 1;
            guard.writer = SegmentWriter::open(&segment_path(&self.dir, id))?;
            guard.id = id;
            sys::sync_dir(&self.dir)?;
        }

        let intra = guard.writer.len();
        if header_len > 0 {
            guard.writer.append(&header[..header_len])?;
        }
        guard.writer.append(data)?;

        Ok(u64::from(guard.id) * self.segment_size + intra)
    }

    /// Flush this bucket and report its durable frontier. Cheap when nothing
    /// was appended since the last call: no fsync is issued.
    pub fn sync(&self) -> Result<(u32, u64)> {
        let active = self
            .active
            .as_ref()
            .expect("sync on a read-only bucket is unreachable");
        let mut guard = active.lock();
        let len = guard.writer.sync()?;
        Ok((guard.id, len))
    }

    fn segment(&self, id: u32) -> Result<Arc<SegmentReader>> {
        if let Some(reader) = self.readers.read().get(&id) {
            return Ok(reader.clone());
        }
        let mut guard = self.readers.write();
        if let Some(reader) = guard.get(&id) {
            return Ok(reader.clone());
        }
        let reader = Arc::new(SegmentReader::open(&segment_path(&self.dir, id))?);
        guard.insert(id, reader.clone());
        Ok(reader)
    }
}

fn segment_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("{id:010}.{SEGMENT_EXT}"))
}

fn segment_id(path: &Path) -> Option<u32> {
    if path.extension()?.to_str()? != SEGMENT_EXT {
        return None;
    }
    path.file_stem()?.to_str()?.parse().ok()
}

/// Discard everything written past the durable frontier: whole segments beyond
/// it, and the tail of the segment holding it.
fn recover(dir: &Path, segment: u32, len: u64) -> Result<()> {
    let mut stale = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if segment_id(&path).is_some_and(|id| id > segment) {
            stale.push(path);
        }
    }
    for path in stale {
        fs::remove_file(path)?;
    }

    let active = segment_path(dir, segment);
    if active.try_exists()? {
        let file = OpenOptions::new().write(true).open(&active)?;
        if file.metadata()?.len() > len {
            file.set_len(len)?;
            file.sync_all()?;
        }
    }

    sys::sync_dir(dir)?;
    Ok(())
}
