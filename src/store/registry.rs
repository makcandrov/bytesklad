use std::{
    fs,
    io::{self, Read},
    path::Path,
};

use indexmap::IndexSet;
use rustc_hash::FxBuildHasher;

use crate::{
    Error, Result,
    store::{
        bucket::Kind,
        pointer::{MAX_BUCKET_LEN, MAX_BUCKETS, MAX_INLINE_LEN, UNSIZED_TAG},
    },
    sys,
};

const MAGIC: [u8; 8] = *b"SKLDREGY";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 24;
const ENTRY_LEN: usize = 8;
const MAX_FILE_LEN: usize = HEADER_LEN + MAX_BUCKETS * ENTRY_LEN;
pub(super) const FILE: &str = "registry";

/// The database's immutable identity: its key length and bucket layout.
#[derive(Debug)]
pub(crate) struct Registry {
    key_len: usize,
    layout: Layout,
}

/// Size buckets in tag order: index `i` is tag `i + 1`. Only this module inserts.
#[derive(Debug)]
pub(crate) struct Layout(IndexSet<usize, FxBuildHasher>);

impl Registry {
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let file = match fs::File::open(dir.join(FILE)) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut raw = Vec::new();
        file.take((MAX_FILE_LEN + 1) as u64).read_to_end(&mut raw)?;
        if raw.len() > MAX_FILE_LEN {
            return Err(Error::corrupt(FILE, "registry is too large"));
        }

        if raw.len() < HEADER_LEN || raw[..8] != MAGIC {
            return Err(Error::corrupt(FILE, "bad magic"));
        }
        if u32::from_le_bytes(raw[8..12].try_into().unwrap()) != VERSION {
            return Err(Error::corrupt(FILE, "unsupported version"));
        }

        let key_len = u32::from_le_bytes(raw[12..16].try_into().unwrap()) as usize;
        let count = u32::from_le_bytes(raw[16..20].try_into().unwrap()) as usize;

        if count > MAX_BUCKETS || raw.len() != HEADER_LEN + count * ENTRY_LEN {
            return Err(Error::corrupt(FILE, "truncated bucket table"));
        }

        let mut layout = Layout::with_capacity(count);
        for &chunk in raw[HEADER_LEN..].as_chunks::<ENTRY_LEN>().0 {
            let size = u64::from_le_bytes(chunk);
            match layout.insert(size as usize) {
                Err(_) => return Err(Error::corrupt(FILE, "bucket size out of range")),
                Ok(false) => return Err(Error::corrupt(FILE, "duplicate bucket size")),
                Ok(true) => {}
            }
        }

        Ok(Some(Self { key_len, layout }))
    }

    pub fn create(dir: &Path, key_len: usize, sizes: &[usize]) -> Result<Self> {
        let mut layout = Layout::with_capacity(sizes.len());
        for &size in sizes {
            layout.insert(size)?;
        }
        let registry = Self { key_len, layout };
        registry.store(dir)?;
        Ok(registry)
    }

    fn store(&self, dir: &Path) -> Result<()> {
        let sizes = &self.layout.0;
        let mut buf = Vec::with_capacity(HEADER_LEN + sizes.len() * ENTRY_LEN);
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(self.key_len as u32).to_le_bytes());
        buf.extend_from_slice(&(sizes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        for &size in sizes {
            buf.extend_from_slice(&(size as u64).to_le_bytes());
        }
        sys::atomic_write(&dir.join(FILE), &buf)?;
        Ok(())
    }

    pub fn check_key_len(&self, key_len: usize) -> Result<()> {
        if self.key_len != key_len {
            return Err(Error::KeyLenMismatch {
                stored: self.key_len,
                requested: key_len,
            });
        }
        Ok(())
    }

    pub fn into_layout(self) -> Layout {
        self.layout
    }
}

impl Layout {
    fn with_capacity(capacity: usize) -> Self {
        Self(IndexSet::with_capacity_and_hasher(capacity, FxBuildHasher))
    }

    fn insert(&mut self, size: usize) -> Result<bool> {
        if size <= MAX_INLINE_LEN {
            return Err(Error::BucketTooSmall {
                max_inline: MAX_INLINE_LEN,
            });
        }
        if size as u64 > MAX_BUCKET_LEN {
            return Err(Error::BucketTooLarge {
                max: MAX_BUCKET_LEN,
            });
        }
        if self.0.contains(&size) {
            return Ok(false);
        }
        if self.0.len() == MAX_BUCKETS {
            return Err(Error::TooManyBuckets { max: MAX_BUCKETS });
        }
        self.0.insert(size);
        Ok(true)
    }

    /// Set equality: order and repeats in `requested` are not significant.
    pub fn check(&self, requested: &[usize]) -> Result<()> {
        let requested_set: IndexSet<_, FxBuildHasher> = requested.iter().copied().collect();
        if requested_set != self.0 {
            return Err(Error::BucketsMismatch {
                stored: self.sizes().collect(),
                requested: requested.to_vec(),
            });
        }
        Ok(())
    }

    pub fn sizes(&self) -> impl ExactSizeIterator<Item = usize> {
        self.0.iter().copied()
    }

    pub fn tag_for(&self, size: usize) -> Option<u8> {
        self.0.get_index_of(&size).map(|i| (i + 1) as u8)
    }

    /// The unsized bucket first, then the size buckets in tag order, matching `slot`.
    pub fn kinds(&self) -> impl Iterator<Item = (u8, Kind)> {
        std::iter::once((UNSIZED_TAG, Kind::Unsized)).chain(
            self.0
                .iter()
                .enumerate()
                .map(|(i, &size)| ((i + 1) as u8, Kind::Sized(size))),
        )
    }
}
