use sdecode_preimages_interface::{
    Image, Preimage, PreimageEntry, PreimagesProvider, PreimagesWriter,
};

use crate::{Error, PreimageDb};

impl PreimagesProvider for PreimageDb {
    type Error = Error;

    fn nearest_lower_preimage(&self, image: &Image) -> crate::Result<Option<PreimageEntry>> {
        let result = self.nearest_lower(image.as_ref())?;
        Ok(result
            .map(|(hash, data)| PreimageEntry::new_unchecked(hash.into(), Preimage::from(data))))
    }

    fn nearest_upper_preimage(&self, image: &Image) -> crate::Result<Option<PreimageEntry>> {
        let result = self.nearest_upper(image.as_ref())?;
        Ok(result
            .map(|(hash, data)| PreimageEntry::new_unchecked(hash.into(), Preimage::from(data))))
    }

    fn exact_preimage(&self, image: &Image) -> Result<Option<Preimage>, Self::Error> {
        let data = self.get(image.as_ref())?;
        Ok(data.map(Preimage::from))
    }
}

impl PreimagesWriter for PreimageDb {
    type Error = Error;

    fn write_preimages<'a>(
        &self,
        preimages: impl IntoIterator<Item = &'a PreimageEntry>,
    ) -> crate::Result<()> {
        let batch: Vec<([u8; 32], Vec<u8>)> = preimages
            .into_iter()
            .map(|e| (e.image().0, e.preimage().to_vec()))
            .collect();
        self.insert_batch(&batch)?;
        Ok(())
    }

    fn write_preimage_entry(&self, preimage: &PreimageEntry) -> Result<(), Self::Error> {
        self.insert(preimage.image_ref().as_ref(), preimage.preimage())?;
        Ok(())
    }
}
