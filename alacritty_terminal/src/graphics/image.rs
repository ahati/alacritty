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
#[derive(Clone, Debug)]
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
    ///
    /// Empty while a deferred decode is still outstanding; see
    /// [`is_ready`](Self::is_ready).
    ///
    /// For an animation this is always the pixels of the current frame, see
    /// [`frames`](Self::frames).
    pub pixels: Arc<[u8]>,
    /// Bumped whenever the image data is replaced, so renderers can invalidate
    /// cached textures.
    pub generation: u64,
    /// Whether the client marked the image as transient (`N=1`), which makes it
    /// a preferred eviction candidate.
    pub transient: bool,
    /// Bytes charged against the quota for this image.
    ///
    /// Before the pixels exist this is the reservation made for them, since a
    /// decode that has not run yet still has to hold its space.
    charge: usize,
    /// Animation frames beyond the root, fully composed.
    ///
    /// The root frame is the image's own [`pixels`](Self::pixels); frame 1 on
    /// the wire is this image, and the entries here are frames 2 and onward.
    /// Empty for a static image.
    pub frames: Vec<Frame>,
    /// Playback state, present once a client controls the image's animation.
    pub animation: Option<Animation>,
}

impl ImageData {
    /// Whether the pixel data has been decoded yet.
    ///
    /// Decoding happens away from the terminal lock, so an image can be placed
    /// before its pixels exist. Such an image must not be uploaded or drawn.
    pub fn is_ready(&self) -> bool {
        !self.pixels.is_empty()
    }

    /// Bytes this image is charged against the quota.
    pub fn charge(&self) -> usize {
        self.charge
    }

    /// The stored animation frames, not counting the root frame.
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// The playback state, if the client has animated this image.
    pub fn animation(&self) -> Option<&Animation> {
        self.animation.as_ref()
    }

    /// Whether playing this image can ever show more than one frame.
    ///
    /// A single-frame image has nothing to advance to, and one whose frames
    /// are all gapless is never visible, so neither is armed for playback.
    pub fn is_animatable(&self) -> bool {
        self.frames.len() >= 2 && self.frames.iter().map(|frame| frame.gap_ms).sum::<u32>() > 0
    }

    /// Replace the frame data wholesale, as a frame edit does.
    fn with_frames(self, frames: Vec<Frame>) -> Self {
        let frame_bytes =
            |frames: &[Frame]| frames.iter().map(|frame| frame.pixels.len()).sum::<usize>();
        // The quota counts every stored frame once; the root frame's bytes are
        // shared with `pixels`, which is an `Arc` into the same buffer.
        let charge = self.charge - frame_bytes(&self.frames) + frame_bytes(&frames);
        Self { charge, frames, ..self }
    }

    /// Show the frame at `index`, making its pixels the displayed ones.
    fn with_current(self, index: u16) -> Self {
        let pixels = self.frames[index as usize].pixels.clone();
        let mut animation = self.animation.unwrap_or(Animation::stopped());
        animation.current = index;
        Self { pixels, animation: Some(animation), ..self }
    }

    /// Replace the playback state.
    fn with_animation(self, animation: Animation) -> Self {
        Self { animation: Some(animation), ..self }
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

use std::time::Instant;

/// An image on its way into an [`ImageStore`].
struct Stored {
    client_id: u32,
    number: u32,
    width: u32,
    height: u32,
    format: PixelFormat,
    pixels: Arc<[u8]>,
    charge: usize,
    transient: bool,
}

/// Upper bound on the pixel data an image of these dimensions can occupy.
///
/// A deferred decode reserves this much before it knows which layout the
/// payload will decode to, and corrects the accounting when it finishes.
pub fn max_pixel_bytes(width: u32, height: u32, channels: usize) -> usize {
    (width as usize).saturating_mul(height as usize).saturating_mul(channels)
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

/// One animation frame: fully composed pixels and the gap shown before the
/// frame that follows it.
#[derive(Clone, Debug)]
pub struct Frame {
    /// The composed W×H pixels of the frame, in the image's format.
    pub pixels: Arc<[u8]>,
    /// Milliseconds this frame is shown before the next one. Zero marks a
    /// gapless frame, which is never displayed.
    pub gap_ms: u32,
}

/// How far an animation has played.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    /// Not playing; resumes where it stopped when restarted.
    Stopped,
    /// Playing, but holding on the last frame instead of wrapping, waiting
    /// for further frames to arrive.
    Loading,
    /// Playing with wrap-around.
    Running,
}

/// The playback state of an animating image.
#[derive(Clone, Copy, Debug)]
pub struct Animation {
    pub state: PlayState,
    /// Index of the displayed frame in the image's [`frames`](super::ImageData::frames).
    pub current: u16,
    /// Full cycles left before the animation stops; [`u32::MAX`] is infinite.
    pub loops_left: u32,
    /// When the current frame's gap has elapsed and the next one is due.
    pub deadline: Option<Instant>,
}

impl Animation {
    /// Loop count that never stops playing.
    pub const INFINITE: u32 = u32::MAX;

    /// A fresh, idle playback state.
    pub fn stopped() -> Self {
        Self { state: PlayState::Stopped, current: 0, loops_left: Self::INFINITE, deadline: None }
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

    /// Apply a mutation to one image, replacing it in place the way
    /// [`complete`](Self::complete) does.
    ///
    /// The closure receives a clone of the image and returns it with the frame
    /// or playback changes applied; the quota accounting follows the charge
    /// difference. Returns whether the image was still stored.
    fn update(&mut self, internal_id: u32, change: impl FnOnce(&ImageData) -> ImageData) -> bool {
        let Some(existing) = self.images.get(&internal_id) else { return false };
        let mut updated = change(existing);
        self.used = self.used + updated.charge - existing.charge;
        self.generation += 1;
        // The new generation is what makes renderers drop their cached
        // texture for the image.
        updated.generation = self.generation;
        self.images.insert(internal_id, Arc::new(updated));
        true
    }

    /// Append a composed animation frame, returning its 1-based number.
    pub fn append_frame(
        &mut self,
        internal_id: u32,
        pixels: Arc<[u8]>,
        gap_ms: u32,
    ) -> Option<u32> {
        if !self.update(internal_id, |image| {
            let mut image = image.clone();
            image.frames.push(Frame { pixels, gap_ms });
            let frames = std::mem::take(&mut image.frames);
            image.with_frames(frames)
        }) {
            return None;
        }
        Some(self.get(internal_id)?.frames().len() as u32)
    }

    /// Overwrite an existing frame's pixels, leaving its gap alone.
    ///
    /// Returns whether the displayed pixels changed, which happens only when
    /// the edited frame is the one on screen.
    pub fn replace_frame(&mut self, internal_id: u32, index: u16, pixels: Arc<[u8]>) -> bool {
        // The displayed frame is the animation's current one, or the root when
        // the client has never controlled playback.
        let displayed = self
            .get(internal_id)
            .map(|image| image.animation.map(|animation| animation.current).unwrap_or(0))
            == Some(index);
        self.update(internal_id, |image| {
            let mut image = image.clone();
            image.frames[index as usize].pixels = pixels;
            if displayed {
                image.pixels = image.frames[index as usize].pixels.clone();
            }
            let frames = std::mem::take(&mut image.frames);
            image.with_frames(frames)
        }) && displayed
    }

    /// Change one frame's gap, in milliseconds.
    pub fn retime_frame(&mut self, internal_id: u32, index: u16, gap_ms: u32) -> bool {
        self.update(internal_id, |image| {
            let mut image = image.clone();
            image.frames[index as usize].gap_ms = gap_ms;
            let frames = std::mem::take(&mut image.frames);
            image.with_frames(frames)
        })
    }

    /// Make the frame at `index` the displayed one.
    ///
    /// Returns whether the displayed pixels changed, which is false when the
    /// frame was already current or the image is gone.
    pub fn show_frame(&mut self, internal_id: u32, index: u16) -> bool {
        let displayed = self
            .get(internal_id)
            .and_then(|image| image.animation)
            .map(|animation| animation.current)
            == Some(index);
        if displayed {
            return false;
        }
        self.update(internal_id, |image| image.clone().with_current(index))
    }

    /// Store the playback state for an image.
    pub fn set_animation(&mut self, internal_id: u32, animation: Animation) -> bool {
        self.update(internal_id, |image| image.clone().with_animation(animation))
    }

    /// Remove the frame at `index`, renumbering the frames after it.
    ///
    /// The displayed frame follows the removal the way the reference
    /// implementation's does: the successor takes the removed frame's place,
    /// unless the last frame was removed, in which case the predecessor is
    /// displayed. Returns whether the displayed pixels changed. Removing the
    /// only frame of an image is a no-op, as the reference makes it.
    pub fn remove_frame(&mut self, internal_id: u32, index: u16) -> bool {
        let Some(image) = self.get(internal_id) else { return false };
        if image.frames.len() <= 1 || index as usize >= image.frames.len() {
            return false;
        }
        let frame_count = image.frames.len();
        let current = image.animation.map(|animation| animation.current).unwrap_or(0);

        // Only the removal of the displayed frame changes the displayed
        // pixels: its successor (or predecessor, for the last frame) is shown
        // instead.
        let changed_display = current == index;

        self.update(internal_id, |image| {
            let mut image = image.clone();
            image.frames.remove(index as usize);
            if let Some(animation) = &mut image.animation {
                if index < animation.current {
                    animation.current -= 1;
                } else if animation.current as usize == frame_count - 1 {
                    // The last frame was removed: show its predecessor.
                    animation.current -= 1;
                }
                animation.current = animation.current.min(image.frames.len() as u16 - 1);
            }
            // Without playback the image displays its root frame, which after
            // the removal is whatever frame now sits at the root's place.
            image.pixels = image.frames[0].pixels.clone();
            let frames = std::mem::take(&mut image.frames);
            image.with_frames(frames)
        });

        changed_display
    }

    /// Advance every image whose frame deadline has passed.
    ///
    /// Returns whether any displayed pixels changed. A gapless frame is
    /// skipped without being shown, a loading animation holds on its last
    /// frame instead of wrapping, and a running one stops once its loop count
    /// is exhausted.
    pub fn advance_animations(&mut self, now: Instant) -> bool {
        let due: Vec<u32> = self
            .images
            .iter()
            .filter_map(|(internal_id, image)| {
                let animation = image.animation()?;
                if animation.state == PlayState::Stopped || !image.is_animatable() {
                    return None;
                }
                animation.deadline.filter(|deadline| *deadline <= now).map(|_| *internal_id)
            })
            .collect();

        let mut changed = false;
        for internal_id in due {
            changed |= self.advance_one(internal_id, now);
        }
        changed
    }

    /// Advance one animation past its deadline.
    fn advance_one(&mut self, internal_id: u32, now: Instant) -> bool {
        let Some(image) = self.get(internal_id) else { return false };
        let Some(animation) = image.animation else { return false };
        let frame_count = image.frames.len();
        if frame_count < 2 {
            return false;
        }

        let mut animation = animation;
        let mut current = animation.current as usize;
        loop {
            current += 1;
            if current >= frame_count {
                // The last frame's gap has elapsed: loading animations hold
                // here until a new frame arrives, running ones wrap or stop.
                // Neither changes the displayed pixels.
                if animation.state == PlayState::Loading {
                    animation.deadline = None;
                    self.set_animation(internal_id, animation);
                    return false;
                }
                if animation.loops_left == 0 {
                    animation.state = PlayState::Stopped;
                    animation.deadline = None;
                    self.set_animation(internal_id, animation);
                    return false;
                }
                if animation.loops_left != Animation::INFINITE {
                    animation.loops_left -= 1;
                }
                current = 0;
            }
            if image.frames[current].gap_ms > 0 {
                break;
            }
            // A gapless frame is never displayed: walk over it.
        }

        animation.current = current as u16;
        animation.deadline =
            Some(now + std::time::Duration::from_millis(image.frames[current].gap_ms as u64));
        self.update(internal_id, |image| {
            let mut image = image.clone();
            image.pixels = image.frames[current].pixels.clone();
            image.animation = Some(animation);
            image
        })
    }

    /// The earliest moment some animation is due to advance.
    pub fn next_animation_deadline(&self) -> Option<Instant> {
        self.images
            .values()
            .filter_map(|image| {
                let animation = image.animation()?;
                if animation.state == PlayState::Stopped || !image.is_animatable() {
                    return None;
                }
                animation.deadline
            })
            .min()
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
        let charge = decoded.pixels.len();
        let pixels: Arc<[u8]> = decoded.pixels.into();

        self.store(Stored {
            client_id,
            number,
            width: decoded.width,
            height: decoded.height,
            format: decoded.format,
            pixels,
            charge,
            transient,
        })
    }

    /// Reserve space for an image whose pixels have not been decoded yet.
    ///
    /// The placement is anchored before the decode runs, so the image is
    /// registered up front with the dimensions taken from the transmission and
    /// `reserved` as an upper bound on the size it will eventually occupy.
    pub fn insert_pending(
        &mut self,
        client_id: u32,
        number: u32,
        width: u32,
        height: u32,
        reserved: usize,
        transient: bool,
    ) -> (u32, Arc<ImageData>) {
        self.store(Stored {
            client_id,
            number,
            width,
            height,
            format: PixelFormat::Rgba,
            pixels: Arc::from(Vec::new()),
            charge: reserved,
            transient,
        })
    }

    /// Attach decoded pixels to an image created by [`insert_pending`].
    ///
    /// Returns whether the image was still stored. It may have been evicted or
    /// deleted while the decode ran, in which case the pixels are dropped.
    ///
    /// [`insert_pending`]: Self::insert_pending
    pub fn complete(&mut self, internal_id: u32, decoded: Decoded) -> bool {
        let Some(existing) = self.images.get(&internal_id) else {
            return false;
        };

        let mut stored = Stored {
            client_id: existing.client_id,
            number: existing.number,
            width: decoded.width,
            height: decoded.height,
            format: decoded.format,
            pixels: decoded.pixels.into(),
            charge: 0,
            transient: existing.transient,
        };
        stored.charge = stored.pixels.len();

        self.used -= existing.charge();
        self.used += stored.charge;
        self.generation += 1;

        let image = Arc::new(ImageData {
            client_id: stored.client_id,
            number: stored.number,
            width: stored.width,
            height: stored.height,
            format: stored.format,
            pixels: stored.pixels.clone(),
            generation: self.generation,
            transient: stored.transient,
            charge: stored.charge,
            // The decoded pixels are the root frame, from which the animation
            // frames the client adds later are composed.
            frames: vec![Frame { pixels: stored.pixels, gap_ms: 0 }],
            animation: None,
        });
        self.images.insert(internal_id, Arc::clone(&image));

        true
    }

    /// Insert an image, reusing the identifier a client identifier maps to.
    fn store(&mut self, stored: Stored) -> (u32, Arc<ImageData>) {
        let internal_id = match self.by_client_id.get(&stored.client_id) {
            Some(internal_id) if stored.client_id != 0 => {
                // Drop the replaced image's bytes.
                if let Some(old) = self.images.remove(internal_id) {
                    self.used -= old.charge();
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
            client_id: stored.client_id,
            number: stored.number,
            width: stored.width,
            height: stored.height,
            format: stored.format,
            pixels: stored.pixels.clone(),
            generation: self.generation,
            transient: stored.transient,
            charge: stored.charge,
            frames: if stored.pixels.is_empty() {
                // A pending image has no root frame until its decode lands.
                Vec::new()
            } else {
                vec![Frame { pixels: stored.pixels, gap_ms: 0 }]
            },
            animation: None,
        });

        self.used += image.charge();
        if stored.client_id != 0 {
            self.by_client_id.insert(stored.client_id, internal_id);
        }
        if stored.number != 0 {
            self.by_number.insert(stored.number, internal_id);
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

        self.used -= image.charge();
        if image.client_id != 0 && self.by_client_id.get(&image.client_id) == Some(&internal_id) {
            self.by_client_id.remove(&image.client_id);
        }
        if image.number != 0 && self.by_number.get(&image.number) == Some(&internal_id) {
            // Numbers are not unique keys: a later image with the same number
            // coexists with an earlier one, and lookups resolve to the newest.
            // Removing the newest hands the number back to its predecessor.
            let number = image.number;
            self.by_number.remove(&number);
            if let Some((newest, _)) = self
                .images
                .iter()
                .filter(|(_, image)| image.number == number)
                .max_by_key(|(internal_id, _)| *internal_id)
            {
                self.by_number.insert(number, *newest);
            }
        }

        true
    }

    /// The smallest client identifier not taken, as the reference
    /// implementation assigns one to a transmission identified only by
    /// number.
    pub fn free_client_id(&self) -> u32 {
        let mut candidate = 1;
        while self.by_client_id.contains_key(&candidate) {
            candidate += 1;
        }
        candidate
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

/// Dimensions a raw transmission declares in its control data.
pub fn probe_raw(
    format: Format,
    width: u32,
    height: u32,
    max_dimension: u32,
) -> Result<(u32, u32), GraphicsError> {
    if format.bytes_per_pixel().is_none() {
        return Err(GraphicsError::Invalid);
    }

    check_dimensions(width, height, max_dimension)?;

    Ok((width, height))
}

/// Dimensions a PNG stream declares in its header.
///
/// Only the header is parsed, so that a placement can be anchored and quota
/// reserved before the pixels are decoded.
pub fn probe_png(data: &[u8], max_dimension: u32) -> Result<(u32, u32), GraphicsError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let reader = decoder.read_info().map_err(|_| GraphicsError::BadData)?;

    let info = reader.info();
    check_dimensions(info.width, info.height, max_dimension)?;

    Ok((info.width, info.height))
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
