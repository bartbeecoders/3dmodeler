//! Decoded images, before they are anything the GPU knows about.
//!
//! Reference images, the PBR library's cached maps and the ORM packer all
//! produce the same thing: a width, a height and RGBA bytes. Under three-d that
//! thing was `CpuTexture`, which the app used purely as a carrier — it named the
//! type in six places and read `data`, `width` and `height` and nothing else.
//!
//! So this is the carrier, kept, with the fields that were actually used.
//! Everything upstream of the GPU — decoding a PNG, packing three greyscale maps
//! into one — stays renderer-independent, and the uploader takes it from here.

/// Pixel data. One variant, because one variant is what the app produces:
/// everything is decoded to 8-bit RGBA before it gets this far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextureData {
    RgbaU8(Vec<[u8; 4]>),
}

impl Default for TextureData {
    fn default() -> Self {
        Self::RgbaU8(Vec::new())
    }
}

/// A decoded image in main memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuTexture {
    pub data: TextureData,
    pub width: u32,
    pub height: u32,
}

impl CpuTexture {
    /// The pixels, if the dimensions agree with the data.
    ///
    /// A texture whose `data` is shorter than `width * height` is what a
    /// truncated download produces, and indexing it by row is an out-of-bounds
    /// panic several functions away from the cause.
    pub fn pixels(&self) -> Option<&[[u8; 4]]> {
        let TextureData::RgbaU8(px) = &self.data;
        (px.len() as u64 == self.width as u64 * self.height as u64).then_some(px.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixels_are_refused_when_they_do_not_fill_the_stated_size() {
        let short = CpuTexture {
            data: TextureData::RgbaU8(vec![[0, 0, 0, 255]; 3]),
            width: 2,
            height: 2,
        };
        assert!(short.pixels().is_none());

        let exact = CpuTexture {
            data: TextureData::RgbaU8(vec![[0, 0, 0, 255]; 4]),
            width: 2,
            height: 2,
        };
        assert_eq!(exact.pixels().map(<[_]>::len), Some(4));
    }

    #[test]
    fn an_empty_texture_is_consistent_by_default() {
        assert_eq!(CpuTexture::default().pixels().map(<[_]>::len), Some(0));
    }
}
