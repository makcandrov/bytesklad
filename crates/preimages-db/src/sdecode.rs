use sdecode_preimages_interface::{
    Image, Preimage, PreimageEntry, PreimageEntryRef, PreimagesProvider, PreimagesWriter,
};

use crate::{Error, PreimageDbRO, PreimageDbRW, PreimageDbRead, PreimageDbWrite};

impl PreimagesProvider for PreimageDbRW {
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

    fn is_empty(&self) -> Result<bool, Self::Error> {
        PreimageDbRead::is_empty(self)
    }
}

impl PreimagesProvider for PreimageDbRO {
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

    fn is_empty(&self) -> Result<bool, Self::Error> {
        PreimageDbRead::is_empty(self)
    }
}

impl PreimagesWriter for PreimageDbRW {
    type Error = Error;

    fn write_preimages<'a>(
        &self,
        preimages: impl IntoIterator<Item = impl Into<PreimageEntryRef<'a>>>,
    ) -> Result<(), Self::Error> {
        self.insert_batch(preimages.into_iter().map(|entry| {
            let entry = entry.into();
            entry.into()
        }))?;
        Ok(())
    }

    fn write_preimage_entry<'a>(
        &self,
        entry: impl Into<PreimageEntryRef<'a>>,
    ) -> Result<(), Self::Error> {
        let entry = entry.into();
        self.insert(entry.image_ref().as_ref(), entry.preimage())?;
        Ok(())
    }
}
