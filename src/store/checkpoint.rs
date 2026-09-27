use std::{
    fs,
    io::{self, Read},
    path::Path,
};

use rustc_hash::FxHashMap;

use crate::{
    Error, Result,
    store::pointer::{MAX_BUCKET_LEN, MAX_BUCKETS},
    sys,
};

const MAGIC: [u8; 8] = *b"SKLDCKPT";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 12;
const MAX_ENTRIES: usize = MAX_BUCKETS + 1;
const MAX_FILE_LEN: usize = HEADER_LEN + MAX_ENTRIES * ENTRY_LEN;
pub(super) const FILE: &str = "checkpoint";

/// The durable write frontier of every bucket: how many of its bytes are known
/// to have reached stable storage.
///
/// Bytes past a bucket's frontier were never referenced by a committed index
/// entry — the store is always fsynced and checkpointed *before* the index
/// transaction that names those bytes commits — so a writer may truncate them
/// on reopen.
pub(crate) type Frontiers = FxHashMap<u8, u64>;

pub(crate) fn load(dir: &Path) -> Result<Frontiers> {
    // Only an empty store can safely open without a checkpoint. The bucket
    // recovery path checks this before truncating any data.
    let file = match fs::File::open(dir.join(FILE)) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Frontiers::default()),
        Err(e) => return Err(e.into()),
    };
    let mut raw = Vec::new();
    file.take((MAX_FILE_LEN + 1) as u64).read_to_end(&mut raw)?;
    if raw.len() > MAX_FILE_LEN {
        return Err(Error::corrupt(FILE, "checkpoint is too large"));
    }

    if raw.len() < HEADER_LEN || raw[..8] != MAGIC {
        return Err(Error::corrupt(FILE, "bad magic"));
    }
    if u32::from_le_bytes(raw[8..12].try_into().unwrap()) != VERSION {
        return Err(Error::corrupt(FILE, "unsupported version"));
    }

    let count = u32::from_le_bytes(raw[12..16].try_into().unwrap()) as usize;
    if count == 0 || count > MAX_ENTRIES || raw.len() != HEADER_LEN + count * ENTRY_LEN {
        return Err(Error::corrupt(FILE, "truncated frontier table"));
    }

    let mut frontiers = Frontiers::default();
    for chunk in raw[HEADER_LEN..].as_chunks::<ENTRY_LEN>().0 {
        let tag = u32::from_le_bytes(chunk[..4].try_into().unwrap());
        let len = u64::from_le_bytes(chunk[4..].try_into().unwrap());
        if !(1..=255).contains(&tag) || len > MAX_BUCKET_LEN {
            return Err(Error::corrupt(FILE, "frontier out of range"));
        }
        if frontiers.insert(tag as u8, len).is_some() {
            return Err(Error::corrupt(FILE, "duplicate bucket tag"));
        }
    }
    Ok(frontiers)
}

pub(crate) fn store(dir: &Path, frontiers: &[(u8, u64)]) -> Result<()> {
    let mut buf = Vec::with_capacity(HEADER_LEN + frontiers.len() * ENTRY_LEN);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&(frontiers.len() as u32).to_le_bytes());
    for &(tag, len) in frontiers {
        buf.extend_from_slice(&u32::from(tag).to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
    }
    sys::atomic_write(&dir.join(FILE), &buf)?;
    Ok(())
}
