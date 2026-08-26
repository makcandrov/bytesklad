use std::{fs, io, path::Path};

use crate::{Error, Result, store::pointer::MAX_BUCKETS, sys};

const MAGIC: [u8; 8] = *b"SKLDREGY";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 32;
const ENTRY_LEN: usize = 8;
const FILE: &str = "registry";

/// The database's immutable identity plus the list of size buckets.
///
/// `key_len` and `segment_size` are fixed at creation because stored pointers
/// are interpreted against them. The bucket list, by contrast, is append-only:
/// a record carries its own tag, so declaring a new bucket never invalidates
/// records already written, and tags are therefore never reused or reordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registry {
    pub key_len: usize,
    pub segment_size: u64,
    /// Record size of the bucket with tag `i + 1`.
    pub buckets: Vec<usize>,
}

impl Registry {
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let raw = match fs::read(dir.join(FILE)) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        if raw.len() < HEADER_LEN || raw[..8] != MAGIC {
            return Err(Error::corrupt(FILE, "bad magic"));
        }
        if u32::from_le_bytes(raw[8..12].try_into().unwrap()) != VERSION {
            return Err(Error::corrupt(FILE, "unsupported version"));
        }

        let key_len = u32::from_le_bytes(raw[12..16].try_into().unwrap()) as usize;
        let segment_size = u64::from_le_bytes(raw[16..24].try_into().unwrap());
        let count = u32::from_le_bytes(raw[24..28].try_into().unwrap()) as usize;

        if count > MAX_BUCKETS || raw.len() != HEADER_LEN + count * ENTRY_LEN {
            return Err(Error::corrupt(FILE, "truncated bucket table"));
        }

        let buckets = raw[HEADER_LEN..]
            .as_chunks::<ENTRY_LEN>()
            .0
            .iter()
            .map(|&chunk| u64::from_le_bytes(chunk) as usize)
            .collect();

        Ok(Some(Self {
            key_len,
            segment_size,
            buckets,
        }))
    }

    pub fn store(&self, dir: &Path) -> Result<()> {
        let mut buf = Vec::with_capacity(HEADER_LEN + self.buckets.len() * ENTRY_LEN);
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(self.key_len as u32).to_le_bytes());
        buf.extend_from_slice(&self.segment_size.to_le_bytes());
        buf.extend_from_slice(&(self.buckets.len() as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        for &size in &self.buckets {
            buf.extend_from_slice(&(size as u64).to_le_bytes());
        }
        sys::atomic_write(&dir.join(FILE), &buf)?;
        Ok(())
    }

    /// Merge `requested` into the existing bucket list, preserving the tag of
    /// every bucket already present. Returns `true` if anything was added.
    pub fn add_buckets(&mut self, requested: &[usize]) -> Result<bool> {
        let mut added = false;
        for &size in requested {
            if size == 0 {
                return Err(Error::ZeroBucket);
            }
            if self.buckets.contains(&size) {
                continue;
            }
            if self.buckets.len() == MAX_BUCKETS {
                return Err(Error::TooManyBuckets { max: MAX_BUCKETS });
            }
            self.buckets.push(size);
            added = true;
        }
        Ok(added)
    }
}
