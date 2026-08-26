use std::{collections::HashMap, fs, io, path::Path};

use crate::{Error, Result, sys};

const MAGIC: [u8; 8] = *b"SKLDCKPT";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 16;
const FILE: &str = "checkpoint";

/// The durable write frontier of every bucket: the segment being appended to
/// and how many of its bytes are known to have reached stable storage.
///
/// Bytes past a bucket's frontier were never referenced by a committed index
/// entry — the store is always fsynced and checkpointed *before* the index
/// transaction that names those bytes commits — so a writer may truncate them
/// on reopen.
pub(crate) type Frontiers = HashMap<u8, (u32, u64)>;

pub(crate) fn load(dir: &Path) -> Result<Frontiers> {
    // A torn or missing checkpoint is not an error: `atomic_write` guarantees
    // the file is either the complete previous version or the complete next
    // one, and an absent file simply means nothing has been synced yet. In
    // both cases recovery falls back to an earlier, and therefore safe,
    // frontier.
    let raw = match fs::read(dir.join(FILE)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Frontiers::new()),
        Err(e) => return Err(e.into()),
    };

    if raw.len() < HEADER_LEN || raw[..8] != MAGIC {
        return Err(Error::corrupt(FILE, "bad magic"));
    }
    if u32::from_le_bytes(raw[8..12].try_into().unwrap()) != VERSION {
        return Err(Error::corrupt(FILE, "unsupported version"));
    }

    let count = u32::from_le_bytes(raw[12..16].try_into().unwrap()) as usize;
    if raw.len() != HEADER_LEN + count * ENTRY_LEN {
        return Err(Error::corrupt(FILE, "truncated frontier table"));
    }

    Ok(raw[HEADER_LEN..]
        .as_chunks::<ENTRY_LEN>()
        .0
        .iter()
        .map(|chunk| {
            let tag = u32::from_le_bytes(chunk[..4].try_into().unwrap()) as u8;
            let segment = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
            let len = u64::from_le_bytes(chunk[8..].try_into().unwrap());
            (tag, (segment, len))
        })
        .collect())
}

pub(crate) fn store(dir: &Path, frontiers: &[(u8, u32, u64)]) -> Result<()> {
    let mut buf = Vec::with_capacity(HEADER_LEN + frontiers.len() * ENTRY_LEN);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&(frontiers.len() as u32).to_le_bytes());
    for &(tag, segment, len) in frontiers {
        buf.extend_from_slice(&u32::from(tag).to_le_bytes());
        buf.extend_from_slice(&segment.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
    }
    sys::atomic_write(&dir.join(FILE), &buf)?;
    Ok(())
}
