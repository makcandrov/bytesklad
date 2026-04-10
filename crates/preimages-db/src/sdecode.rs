use sdecode_preimages_interface::{Image, Preimage, PreimageEntry, PreimagesProvider};

use crate::{Error, PreimageDb};

impl PreimageDb {
    /// Insert a [`PreimageEntry`]. Returns `true` if newly inserted.
    pub fn insert_entry(&self, entry: &PreimageEntry) -> crate::Result<bool> {
        self.insert(entry.image_ref().as_ref(), entry.preimage())
    }

    /// Insert a batch of [`PreimageEntry`] values. Returns the number of new entries.
    pub fn insert_entries(&self, entries: &[PreimageEntry]) -> crate::Result<usize> {
        let batch: Vec<([u8; 32], Vec<u8>)> = entries
            .iter()
            .map(|e| (e.image().0, e.preimage().to_vec()))
            .collect();
        self.insert_batch(&batch)
    }
}

impl PreimagesProvider for PreimageDb {
    type Error = Error;

    fn nearest_lower_preimage(&self, image: Image) -> crate::Result<Option<PreimageEntry>> {
        let result = self.nearest_lower(image.as_ref())?;
        Ok(result
            .map(|(hash, data)| PreimageEntry::new_unchecked(hash.into(), Preimage::from(data))))
    }

    fn nearest_upper_preimage(&self, image: Image) -> crate::Result<Option<PreimageEntry>> {
        let result = self.nearest_upper(image.as_ref())?;
        Ok(result
            .map(|(hash, data)| PreimageEntry::new_unchecked(hash.into(), Preimage::from(data))))
    }

    fn exact_preimage(&self, image: Image) -> Result<Option<Preimage>, Self::Error> {
        let data = self.get(image.as_ref())?;
        Ok(data.map(Preimage::from))
    }
}
