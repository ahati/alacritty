//! Decoded image data and its storage.
//!
//! Pixels are kept in the layout the client transmitted them in, so an
//! uncompressed transmission is stored without any per-pixel work: RGB8 stays
//! three channels and RGBA8 four. Alpha, where present, is straight
//! (unpremultiplied). Images are shared with the renderer through an [`Arc`] so
//! that pixel data is never copied per frame.

use std::collections::HashMap;
use std::sync::Arc;

use super::GraphicsError;
use super::command::Format;

/// Number of channels in a greyscale pixel.
const GRAYSCALE_CHANNELS: usize = 1;

/// Number of channels in a greyscale pixel with alpha.
const GRAYSCALE_ALPHA_CHANNELS: usize = 2;

/// Number of channels in a three-channel pixel.
pub const RGB_CHANNELS: usize = 3;

/// Number of channels in a four-channel pixel.
pub const RGBA_CHANNELS: usize = 4;

/// Channel layout of stored pixel data.
///
/// Alpha forces four channels and its absence three, which is also how the
/// protocol's `f=32` and `f=24` transmissions differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Red, green and blue, without alpha.
    Rgb,
    /// Red, green, blue and straight alpha.
    Rgba,
}

impl PixelFormat {
    /// Number of bytes one pixel occupies in this layout.
    pub const fn channels(self) -> usize {
        match self {
            Self::Rgb => RGB_CHANNELS,
            Self::Rgba => RGBA_CHANNELS,
        }
    }
}

/// A decoded image, shared with the renderer.
#[derive(Debug)]
pub struct ImageData {
    /// Identifier the client used in `i`, or `0` for an anonymous image.
    pub client_id: u32,
    /// Client-assigned number from `I`, or `0` when unset.
    pub number: u32,
    pub width: u32,
    pub height: u32,
    /// Channel layout of `pixels`.
    pub format: PixelFormat,
    /// Tightly packed 8-bit pixels in [`format`](Self::format).
    pub pixels: Arc<[u8]>,
    /// Bumped whenever the image data is replaced, so renderers can invalidate
    /// cached textures.
    pub generation: u64,
    /// Whether the client marked the image as transient (`N=1`), which makes it
    /// a preferred eviction candidate.
    pub transient: bool,
}

impl ImageData {
    /// Bytes of pixel data held by this image.
    pub fn byte_len(&self) -> usize {
        self.pixels.len()
    }
}

/// Pixels decoded from a transmission, before an identifier is assigned.
#[derive(Debug)]
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub pixels: Vec<u8>,
}

/// Images and the memory quota that bounds them.
#[derive(Debug)]
pub struct ImageStore {
    /// Images keyed by an internal identifier that is never reused.
    images: HashMap<u32, Arc<ImageData>>,
    /// Client `i` identifier to internal identifier.
    by_client_id: HashMap<u32, u32>,
    /// Client `I` number to the internal identifier of the newest image.
    by_number: HashMap<u32, u32>,
    /// Total bytes of pixel data currently held.
    used: usize,
    /// Maximum bytes of pixel data to hold.
    limit: usize,
    /// Next internal identifier to hand out.
    next_id: u32,
    /// Monotonic counter used to invalidate renderer caches.
    generation: u64,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ImageStore {
    /// Create a store holding at most `limit` bytes of pixel data.
    pub fn new(limit: usize) -> Self {
        Self {
            images: HashMap::new(),
            by_client_id: HashMap::new(),
            by_number: HashMap::new(),
            used: 0,
            limit,
            next_id: 1,
            generation: 0,
        }
    }

    /// Bytes of pixel data currently held.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Maximum bytes of pixel data that may be held.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Number of stored images.
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Whether no images are stored.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Look up an image by its client `i` identifier.
    pub fn by_client_id(&self, client_id: u32) -> Option<&Arc<ImageData>> {
        let internal = self.by_client_id.get(&client_id)?;
        self.images.get(internal)
    }

    /// Look up the newest image with the client `I` number `number`.
    pub fn by_number(&self, number: u32) -> Option<&Arc<ImageData>> {
        let internal = self.by_number.get(&number)?;
        self.images.get(internal)
    }

    /// Resolve the identifier used by a command to an internal identifier.
    ///
    /// The client identifier takes precedence, matching the protocol rule that
    /// `i` and `I` must not be combined.
    pub fn resolve(&self, client_id: u32, number: u32) -> Option<u32> {
        if client_id != 0 {
            self.by_client_id.get(&client_id).copied()
        } else if number != 0 {
            self.by_number.get(&number).copied()
        } else {
            None
        }
    }

    /// Look up an image by its internal identifier.
    pub fn get(&self, internal_id: u32) -> Option<&Arc<ImageData>> {
        self.images.get(&internal_id)
    }

    /// Iterate over every stored image and its internal identifier.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &Arc<ImageData>)> {
        self.images.iter().map(|(internal_id, image)| (*internal_id, image))
    }

    /// Internal identifiers ordered oldest first.
    ///
    /// "Oldest" is the generation the image data was written at, so an image
    /// that was re-transmitted counts as recently used.
    pub fn by_age(&self) -> Vec<u32> {
        let mut images: Vec<(bool, u64, u32)> = self
            .images
            .iter()
            .map(|(internal_id, image)| {
                // Transient images sort first, so they are evicted first
                // regardless of age.
                (!image.transient, image.generation, *internal_id)
            })
            .collect();
        images.sort_unstable();

        images.into_iter().map(|(_, _, internal_id)| internal_id).collect()
    }

    /// Whether storing `bytes` more pixel data would exceed the quota.
    pub fn is_full(&self, bytes: usize) -> bool {
        self.used.saturating_add(bytes) > self.limit
    }

    /// Store decoded pixels, replacing any image the client identifier refers
    /// to.
    ///
    /// Returns the internal identifier and the stored image. Replacing an
    /// image keeps its internal identifier so that existing placements can be
    /// re-pointed rather than silently detached.
    pub fn insert(
        &mut self,
        client_id: u32,
        number: u32,
        decoded: Decoded,
        transient: bool,
    ) -> (u32, Arc<ImageData>) {
        let internal_id = match self.by_client_id.get(&client_id) {
            Some(internal_id) if client_id != 0 => {
                // Drop the replaced image's bytes.
                if let Some(old) = self.images.remove(internal_id) {
                    self.used -= old.byte_len();
                }
                *internal_id
            },
            _ => {
                let internal_id = self.next_id;
                self.next_id += 1;
                internal_id
            },
        };

        self.generation += 1;
        let image = Arc::new(ImageData {
            client_id,
            number,
            width: decoded.width,
            height: decoded.height,
            format: decoded.format,
            pixels: decoded.pixels.into(),
            generation: self.generation,
            transient,
        });

        self.used += image.byte_len();
        if client_id != 0 {
            self.by_client_id.insert(client_id, internal_id);
        }
        if number != 0 {
            self.by_number.insert(number, internal_id);
        }
        self.images.insert(internal_id, Arc::clone(&image));

        (internal_id, image)
    }

    /// Remove an image and its identifiers.
    ///
    /// Returns whether the image existed.
    pub fn remove(&mut self, internal_id: u32) -> bool {
        let Some(image) = self.images.remove(&internal_id) else {
            return false;
        };

        self.used -= image.byte_len();
        if image.client_id != 0 && self.by_client_id.get(&image.client_id) == Some(&internal_id) {
            self.by_client_id.remove(&image.client_id);
        }
        if image.number != 0 && self.by_number.get(&image.number) == Some(&internal_id) {
            self.by_number.remove(&image.number);
        }

        true
    }

    /// Evict images that no placement refers to until `needed` bytes fit.
    ///
    /// Returns whether enough room was made. Images are evicted oldest first.
    pub fn evict_unreferenced(&mut self, needed: usize, referenced: &dyn Fn(u32) -> bool) -> bool {
        let mut candidates: Vec<u32> = self
            .images
            .iter()
            .filter(|(internal_id, _)| !referenced(**internal_id))
            .map(|(internal_id, _)| *internal_id)
            .collect();
        candidates.sort_unstable();

        for internal_id in candidates {
            if !self.is_full(needed) {
                return true;
            }
            self.remove(internal_id);
        }

        !self.is_full(needed)
    }

    /// Remove every image.
    pub fn clear(&mut self) {
        self.images.clear();
        self.by_client_id.clear();
        self.by_number.clear();
        self.used = 0;
    }
}

/// Decode a transmission's payload into pixels.
///
/// The layout of the returned data follows the source rather than a single
/// internal representation, so callers must consult [`Decoded::format`].
pub fn decode(
    format: Format,
    data: &[u8],
    width: u32,
    height: u32,
    max_dimension: u32,
) -> Result<Decoded, GraphicsError> {
    match format {
        Format::Png => decode_png(data, max_dimension),
        Format::Rgb | Format::Rgba => decode_raw(format, data, width, height, max_dimension),
    }
}

/// Decode a PNG stream into pixels.
fn decode_png(data: &[u8], max_dimension: u32) -> Result<Decoded, GraphicsError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|_| GraphicsError::BadData)?;

    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    check_dimensions(width, height, max_dimension)?;

    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).map_err(|_| GraphicsError::BadData)?;

    let stride = info.line_size;
    let length = stride.checked_mul(info.height as usize).ok_or(GraphicsError::TooLarge)?;
    let data = buffer.get(..length).ok_or(GraphicsError::BadData)?;

    let (format, pixels) = normalize(data, stride, info.color_type, info.width, info.height)?;

    Ok(Decoded { width: info.width, height: info.height, format, pixels })
}

/// Decode uncompressed RGB or RGBA pixels.
///
/// The transmitted layout is also the stored one, so the payload is kept as it
/// arrived.
fn decode_raw(
    format: Format,
    data: &[u8],
    width: u32,
    height: u32,
    max_dimension: u32,
) -> Result<Decoded, GraphicsError> {
    check_dimensions(width, height, max_dimension)?;

    let format = match format {
        Format::Rgb => PixelFormat::Rgb,
        Format::Rgba => PixelFormat::Rgba,
        Format::Png => return Err(GraphicsError::Invalid),
    };

    let stride = (width as usize).checked_mul(format.channels()).ok_or(GraphicsError::TooLarge)?;
    let length = stride.checked_mul(height as usize).ok_or(GraphicsError::TooLarge)?;
    let data = data.get(..length).ok_or(GraphicsError::NotEnoughData)?;

    Ok(Decoded { width, height, format, pixels: data.to_vec() })
}

/// Reject dimensions the terminal refuses to allocate.
fn check_dimensions(width: u32, height: u32, max_dimension: u32) -> Result<(), GraphicsError> {
    if width == 0 || height == 0 {
        return Err(GraphicsError::Invalid);
    }
    if width > max_dimension || height > max_dimension {
        return Err(GraphicsError::TooLarge);
    }
    Ok(())
}

/// Convert decoded PNG scanlines into a tightly packed [`PixelFormat`].
///
/// Only greyscale sources need per-pixel work. Palette, bit depth and 16-to-8
/// bit conversions have already been done by the decoder's `normalize_to_color8`
/// transform, and the remaining colour types either already match the target
/// layout or differ from it only by an alpha channel.
fn normalize(
    data: &[u8],
    stride: usize,
    color_type: png::ColorType,
    width: u32,
    height: u32,
) -> Result<(PixelFormat, Vec<u8>), GraphicsError> {
    let source_channels = match color_type {
        png::ColorType::Grayscale => GRAYSCALE_CHANNELS,
        png::ColorType::GrayscaleAlpha => GRAYSCALE_ALPHA_CHANNELS,
        png::ColorType::Rgb => RGB_CHANNELS,
        png::ColorType::Rgba => RGBA_CHANNELS,
        // Palette images are expanded by the `normalize_to_color8` transform.
        png::ColorType::Indexed => return Err(GraphicsError::BadData),
    };

    let format = match source_channels {
        GRAYSCALE_CHANNELS | RGB_CHANNELS => PixelFormat::Rgb,
        _ => PixelFormat::Rgba,
    };
    let target_channels = format.channels();

    let width = width as usize;
    let height = height as usize;
    let row_length = width * source_channels;
    if stride < row_length || stride == 0 || data.len() < stride * height {
        return Err(GraphicsError::BadData);
    }

    // Rows that are already tightly packed with the target channel count need no
    // conversion at all, which is the case for RGB and RGBA PNGs.
    if source_channels == target_channels && stride == row_length {
        return Ok((format, data.to_vec()));
    }

    debug_assert!(
        source_channels <= GRAYSCALE_ALPHA_CHANNELS,
        "only greyscale sources should need expanding",
    );

    let mut pixels = vec![0; width * height * target_channels];
    for (row, source) in data.chunks_exact(stride).take(height).enumerate() {
        let row_start = row * width * target_channels;
        for (column, pixel) in source[..row_length].chunks_exact(source_channels).enumerate() {
            let start = row_start + column * target_channels;
            // Greyscale is replicated across the colour channels, and greyscale
            // with alpha carries that alpha in its second component.
            pixels[start] = pixel[0];
            pixels[start + 1] = pixel[0];
            pixels[start + 2] = pixel[0];
            if target_channels == RGBA_CHANNELS {
                pixels[start + 3] = pixel[1];
            }
        }
    }

    Ok((format, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_DIMENSION: u32 = 10_000;

    /// Encode a PNG so the decoder can be exercised without fixtures.
    fn encode_png(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut output, width, height);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        output
    }

    #[test]
    fn decodes_an_rgb_png_without_converting_it() {
        let data = [10, 20, 30, 40, 50, 60];
        let png = encode_png(2, 1, png::ColorType::Rgb, &data);

        let decoded = decode(Format::Png, &png, 0, 0, MAX_DIMENSION).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.format, PixelFormat::Rgb);
        assert_eq!(decoded.pixels, vec![10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn decodes_an_rgba_png_without_converting_it() {
        let data = [1, 2, 3, 4, 5, 6, 7, 8];
        let png = encode_png(1, 2, png::ColorType::Rgba, &data);

        let decoded = decode(Format::Png, &png, 0, 0, MAX_DIMENSION).unwrap();
        assert_eq!(decoded.format, PixelFormat::Rgba);
        assert_eq!(decoded.pixels, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn decodes_grayscale_png() {
        let png = encode_png(2, 1, png::ColorType::Grayscale, &[7, 9]);

        let decoded = decode(Format::Png, &png, 0, 0, MAX_DIMENSION).unwrap();
        assert_eq!(decoded.format, PixelFormat::Rgb);
        assert_eq!(decoded.pixels, vec![7, 7, 7, 9, 9, 9]);
    }

    #[test]
    fn decodes_grayscale_alpha_png() {
        let png = encode_png(2, 1, png::ColorType::GrayscaleAlpha, &[7, 1, 9, 2]);

        let decoded = decode(Format::Png, &png, 0, 0, MAX_DIMENSION).unwrap();
        assert_eq!(decoded.format, PixelFormat::Rgba);
        assert_eq!(decoded.pixels, vec![7, 7, 7, 1, 9, 9, 9, 2]);
    }

    #[test]
    fn decodes_raw_rgb_and_rgba() {
        let rgb = decode(Format::Rgb, &[1, 2, 3], 1, 1, MAX_DIMENSION).unwrap();
        assert_eq!(rgb.format, PixelFormat::Rgb);
        assert_eq!(rgb.pixels, vec![1, 2, 3]);

        let rgba = decode(Format::Rgba, &[1, 2, 3, 4], 1, 1, MAX_DIMENSION).unwrap();
        assert_eq!(rgba.format, PixelFormat::Rgba);
        assert_eq!(rgba.pixels, vec![1, 2, 3, 4]);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(
            decode(Format::Png, b"not a png", 0, 0, MAX_DIMENSION),
            Err(GraphicsError::BadData)
        ));
        assert!(matches!(
            decode(Format::Rgb, &[1, 2], 1, 1, MAX_DIMENSION),
            Err(GraphicsError::NotEnoughData)
        ));
        assert!(matches!(
            decode(Format::Rgb, &[1, 2, 3, 4, 5, 6], 2, 1, 1),
            Err(GraphicsError::TooLarge)
        ));
    }

    #[test]
    fn insert_replaces_and_tracks_quota() {
        let decoded =
            || Decoded { width: 1, height: 1, format: PixelFormat::Rgba, pixels: vec![0; 4] };

        let mut store = ImageStore::new(1024);
        let (first, image) = store.insert(5, 0, decoded(), false);
        assert_eq!(image.generation, 1);
        assert_eq!(store.used(), 4);

        // Re-transmitting the same client id replaces in place.
        let (second, replaced) = store.insert(5, 0, decoded(), false);
        assert_eq!(first, second);
        assert!(replaced.generation > image.generation);
        assert_eq!(store.len(), 1);
        assert_eq!(store.used(), 4);

        assert!(store.remove(second));
        assert_eq!(store.used(), 0);
        assert!(store.is_empty());
    }

    #[test]
    fn resolves_by_client_id_and_number() {
        let mut store = ImageStore::new(1024);
        let decoded =
            Decoded { width: 1, height: 1, format: PixelFormat::Rgba, pixels: vec![0; 4] };

        let (by_id, _) = store.insert(7, 0, decoded, false);
        assert_eq!(store.resolve(7, 0), Some(by_id));
        assert_eq!(store.resolve(0, 0), None);

        let decoded =
            Decoded { width: 1, height: 1, format: PixelFormat::Rgba, pixels: vec![0; 4] };
        let (by_number, _) = store.insert(0, 3, decoded, false);
        assert_eq!(store.resolve(0, 3), Some(by_number));
        assert_eq!(store.resolve(0, 4), None);
    }
}
