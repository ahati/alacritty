//! Kitty graphics protocol support.
//!
//! This module implements the terminal side of the [Kitty graphics
//! protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/), which carries
//! inline images inside APC strings (`ESC _ G … ESC \`).
//!
//! The implementation is split by responsibility:
//!
//! * [`command`] parses the protocol's `key=value` control data into a [`command::Command`].
//! * [`image`] owns decoded image data and the storage quota.
//! * [`placement`] owns the grid-anchored placements of images.
//! * This module orchestrates them, turning a stream of APC bytes into state changes and PTY
//!   responses.
//!
//! Payloads are streamed rather than buffered: [`GraphicsStore::apc_put`] is
//! called once per payload byte, and only the control data and the current
//! base64 group are held in memory beyond the decoded image itself.

pub mod command;
mod file;
pub mod image;
pub mod placement;

use std::fmt::Write as _;
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::index::{Line, Point};

use self::command::{Action, Command, Compression, CursorMovement, Format, Medium};
use self::image::{Decoded, ImageData, ImageStore};
pub use self::placement::CellFootprint;

use self::placement::{Placement, PlacementStore, SourceRect};

/// Byte introducing an APC graphics command.
const APC_PREFIX: u8 = b'G';

/// Separator between control data and the base64 payload.
const PAYLOAD_SEPARATOR: u8 = b';';

/// Space some clients put between the `G` and the control data. It cannot be
/// confused with a real key, so it is tolerated for interoperability.
const CONTROL_SPACER: u8 = b' ';

/// Number of base64 characters that encode one whole group of bytes.
const BASE64_GROUP: usize = 4;

/// Padding character used to complete a truncated final base64 group.
const BASE64_PAD: u8 = b'=';

/// Default maximum image width and height, in pixels.
pub const DEFAULT_MAX_IMAGE_DIMENSION: u32 = 10_000;

/// Default maximum bytes of decoded image data to keep resident.
///
/// The reference implementation's storage limit, so a directory of photos
/// behaves the same way here as there.
pub const DEFAULT_MEMORY_LIMIT: usize = 320 * 1024 * 1024;

/// Cell size assumed until the display reports the real one.
///
/// The reference implementation's core is constructed with these metrics, so
/// a terminal that never reports its own resolves cell-relative image sizes
/// the way the reference does, rather than counting one cell per pixel.
const DEFAULT_CELL_SIZE: (u16, u16) = (10, 20);

/// Configuration for graphics support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphicsConfig {
    /// Whether graphics escape sequences are handled at all.
    pub enabled: bool,
    /// Maximum width or height of an image, in pixels.
    pub max_image_dimension: u32,
    /// Maximum bytes of decoded image data to keep.
    pub memory_limit: usize,
    /// Whether a file transmission may name a path that leaves this machine.
    ///
    /// On Windows a share names the host to read from, so a client can make the
    /// terminal contact a machine of its choosing, and authenticate to it.
    /// Turning this off refuses such a path from the bytes the client sent,
    /// before the path is resolved, and refuses a path that resolves to a share
    /// as well.
    ///
    /// A directory junction or a folder redirect can still reach a share while
    /// the path is being resolved, which happens before any check that needs a
    /// file to exist; a mapped drive is reported as a drive letter, which cannot
    /// be told from a local drive. Neither can be refused, and neither is the
    /// connection prevented for.
    ///
    /// Network filesystems are ordinary mounts on other platforms, where the
    /// setting has no effect.
    pub allow_network_paths: bool,
}

impl Default for GraphicsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_image_dimension: DEFAULT_MAX_IMAGE_DIMENSION,
            memory_limit: DEFAULT_MEMORY_LIMIT,
            allow_network_paths: true,
        }
    }
}

impl GraphicsConfig {
    /// A configuration that ignores every graphics escape sequence.
    pub const DISABLED: Self = Self {
        enabled: false,
        max_image_dimension: DEFAULT_MAX_IMAGE_DIMENSION,
        memory_limit: DEFAULT_MEMORY_LIMIT,
        allow_network_paths: true,
    };
}

/// Reason a graphics command could not be carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsError {
    /// The referenced image does not exist.
    NotFound,
    /// A command's arguments were invalid.
    Invalid,
    /// The payload ended before the image was complete.
    NotEnoughData,
    /// The image data could not be decoded.
    BadData,
    /// The payload could not be decompressed.
    BadCompression,
    /// The image exceeds the configured dimension limit.
    TooLarge,
    /// There is no room left for the image.
    NoSpace,
    /// Data arrived out of order.
    BadSequence,
    /// A file medium named a path longer than the protocol allows.
    FilenameTooLong,
    /// The file a file medium named could not be read.
    FileUnreadable,
    /// A raw image declared no width or height.
    ZeroDimensions,
    /// A dimension exceeded the protocol's fixed limit.
    ImageTooLarge,
    /// Compressed data inflated to fewer bytes than the image needs.
    InflationSize,
    /// The command is recognised but not implemented.
    Unsupported,
    /// A referenced parent image does not exist.
    NoParent,
}

impl GraphicsError {
    /// Error code reported to the client.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => "ENOENT",
            Self::Invalid => "EINVAL",
            Self::NotEnoughData => "ENODATA",
            Self::BadData => "EBADPNG",
            Self::BadCompression => "EINVAL",
            Self::TooLarge => "EFBIG",
            Self::NoSpace => "ENOSPC",
            Self::BadSequence => "EILSEQ",
            Self::FilenameTooLong => "EINVAL",
            Self::FileUnreadable => "EBADF",
            Self::ZeroDimensions => "EINVAL",
            Self::ImageTooLarge => "EINVAL",
            Self::InflationSize => "EINVAL",
            Self::Unsupported => "EUNSUPPORTED",
            Self::NoParent => "ENOPARENT",
        }
    }

    /// Human readable explanation reported to the client.
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotFound => "image not found",
            Self::Invalid => "invalid arguments",
            Self::NotEnoughData => "not enough image data",
            Self::BadData => "could not decode image data",
            Self::BadCompression => "could not decompress image data",
            Self::TooLarge => "image too large",
            Self::NoSpace => "image storage limit reached",
            Self::BadSequence => "unexpected chunk sequence",
            Self::FilenameTooLong => "Filename too long",
            Self::FileUnreadable => "Failed to read image file",
            Self::ZeroDimensions => "Zero width/height not allowed",
            Self::ImageTooLarge => "Image too large, width or height greater than 10000",
            Self::InflationSize => "Image data size post inflation does not match expected size",
            Self::Unsupported => "unsupported command",
            Self::NoParent => "parent image not found",
        }
    }
}

/// Outcome of finishing an APC string.
#[derive(Debug, Default)]
pub struct GraphicsUpdate {
    /// Bytes that must be written back to the PTY, if any.
    pub response: Option<String>,
    /// Whether displayed content changed and needs damage.
    pub changed: bool,
    /// Cells the cursor must advance by, if an image was placed.
    pub cursor_advance: Option<CellFootprint>,
}

/// Which part of an APC string is currently being received.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ApcPhase {
    /// Waiting for the `G` that introduces a graphics command.
    #[default]
    Prefix,
    /// Reading `key=value` control data.
    Control,
    /// Reading the base64 payload.
    Payload,
}

/// A transmission being assembled across several APC strings.
#[derive(Debug)]
struct Transfer {
    /// The command from the first chunk, which carries the real control data.
    command: Command,
    /// Base64 characters not yet decoded, always fewer than one whole group
    /// once [`Transfer::decode_complete_groups`] has run.
    chars: Vec<u8>,
    /// Decoded payload bytes.
    data: Vec<u8>,
}

impl Transfer {
    fn new(command: Command) -> Self {
        Self { command, chars: Vec::new(), data: Vec::new() }
    }

    /// Decode every whole base64 group currently buffered.
    fn decode_complete_groups(&mut self) -> Result<(), GraphicsError> {
        let complete = self.chars.len() - self.chars.len() % BASE64_GROUP;
        if complete == 0 {
            return Ok(());
        }

        let decoded = BASE64.decode(&self.chars[..complete]).map_err(|_| GraphicsError::BadData)?;
        self.data.extend_from_slice(&decoded);
        self.chars.drain(..complete);

        Ok(())
    }

    /// Decode the trailing group and return the assembled payload.
    ///
    /// Kitty tolerates a final group without its padding, so the padding is
    /// restored before decoding.
    fn finish(mut self) -> Result<Vec<u8>, GraphicsError> {
        self.decode_complete_groups()?;

        if !self.chars.is_empty() {
            while self.chars.len() % BASE64_GROUP != 0 {
                self.chars.push(BASE64_PAD);
            }
            let decoded = BASE64.decode(&self.chars).map_err(|_| GraphicsError::BadData)?;
            self.data.extend_from_slice(&decoded);
            self.chars.clear();
        }

        Ok(self.data)
    }
}

/// Protocol state for one screen.
#[derive(Debug)]
pub struct GraphicsStore {
    config: GraphicsConfig,
    images: ImageStore,
    placements: PlacementStore,
    /// Size of a single cell in pixels.
    ///
    /// The core has no font metrics of its own, so the display reports this.
    /// The protocol resolves image sizes given in cells from it.
    cell_size: (u16, u16),
    /// Transmission currently being assembled, if any.
    transfer: Option<Transfer>,
    /// Decodes queued by the parser, to be run with the terminal unlocked.
    jobs: Vec<DecodeJob>,
    /// Control data of the APC currently being received.
    control: Vec<u8>,
    /// Base64 characters of the APC currently being received.
    chars: Vec<u8>,
    /// Which part of the APC is being received.
    phase: ApcPhase,
    /// Whether the introducing `G` has been seen.
    started: bool,
    /// Whether this APC must be discarded.
    ignoring: bool,
}

impl Default for GraphicsStore {
    fn default() -> Self {
        Self::new(GraphicsConfig::default())
    }
}

impl GraphicsStore {
    /// Create an empty store.
    pub fn new(config: GraphicsConfig) -> Self {
        Self {
            images: ImageStore::new(config.memory_limit),
            config,
            placements: PlacementStore::new(),
            cell_size: DEFAULT_CELL_SIZE,
            transfer: None,
            jobs: Vec::new(),
            control: Vec::new(),
            chars: Vec::new(),
            phase: ApcPhase::default(),
            started: false,
            ignoring: false,
        }
    }

    /// Report the size of a single cell in pixels.
    ///
    /// The core has no font metrics, so the display supplies these. They only
    /// affect how image sizes given in cells are resolved.
    pub fn set_cell_size(&mut self, width: u16, height: u16) {
        let cell = (width.max(1), height.max(1));
        self.cell_size = cell;

        // Placements keep their anchors, but an offset measured inside the old
        // cell may now fall outside the new one.
        self.placements.clamp_offsets(cell);
    }

    /// Current configuration.
    pub fn config(&self) -> &GraphicsConfig {
        &self.config
    }

    /// Apply a new configuration, discarding everything if disabled.
    pub fn set_config(&mut self, config: GraphicsConfig) {
        self.config = config;
        self.images = ImageStore::new(config.memory_limit);
        if !config.enabled {
            self.clear();
        }
    }

    /// Whether any graphics are displayed.
    pub fn has_placements(&self) -> bool {
        !self.placements.is_empty()
    }

    /// Placements ordered back to front.
    pub fn placements(&self) -> Vec<&Placement> {
        self.placements.sorted()
    }

    /// Look up image data by internal identifier.
    pub fn image(&self, internal_id: u32) -> Option<&Arc<ImageData>> {
        self.images.get(internal_id)
    }

    /// Iterate over every stored image and its internal identifier.
    pub fn images(&self) -> impl Iterator<Item = (u32, &Arc<ImageData>)> {
        self.images.iter()
    }

    /// Bytes of decoded image data currently held.
    pub fn memory_used(&self) -> usize {
        self.images.used()
    }

    /// Begin an APC string.
    pub fn apc_start(&mut self) {
        self.control.clear();
        self.chars.clear();
        self.phase = ApcPhase::Prefix;
        self.started = false;
        self.ignoring = !self.config.enabled;
    }

    /// Consume one byte of the APC string started by [`GraphicsStore::apc_start`].
    pub fn apc_put(&mut self, byte: u8) {
        if self.ignoring {
            return;
        }

        match self.phase {
            ApcPhase::Prefix => {
                if byte == APC_PREFIX {
                    self.phase = ApcPhase::Control;
                } else {
                    // Not a graphics APC; ignore the rest of the string.
                    self.ignoring = true;
                }
            },
            ApcPhase::Control => {
                // Tolerate a spacer between the `G` and the control data.
                if byte == CONTROL_SPACER && self.control.is_empty() {
                    return;
                }

                if byte == PAYLOAD_SEPARATOR {
                    self.phase = ApcPhase::Payload;
                } else if self.control.len() < command::MAX_CONTROL_DATA_LEN {
                    self.control.push(byte);
                } else {
                    self.ignoring = true;
                }
            },
            ApcPhase::Payload => self.chars.push(byte),
        }

        self.started = true;
    }

    /// Finish the APC string and apply the command it carried.
    pub fn apc_end(&mut self, cursor: Point) -> GraphicsUpdate {
        let complete = mem::take(&mut self.started) && !mem::replace(&mut self.ignoring, false);
        self.phase = ApcPhase::Prefix;

        if !complete {
            self.control.clear();
            self.chars.clear();
            return GraphicsUpdate::default();
        }

        let command = match Command::parse(&self.control) {
            Ok(command) => command,
            Err(error) => {
                // Malformed control data is dropped without a reply, matching
                // the reference implementation.
                log::debug!("Ignoring malformed graphics command: {error}");
                self.control.clear();
                self.chars.clear();
                self.transfer = None;
                return GraphicsUpdate::default();
            },
        };
        self.control.clear();

        // Shared memory objects are not implemented. A client that supports
        // them tells the difference by the response, so this stays a refusal
        // rather than letting the payload reach the decoder. Only the actions
        // that load data ever consult the medium, so the others pass it by.
        if command.medium == Medium::SharedMemory && command.action.is_deferred() {
            self.transfer = None;
            return self.failure(&command, GraphicsError::Unsupported);
        }

        // A file medium's payload names a path rather than carrying image data,
        // and the reference ignores `m` for it: such a command is a complete
        // transfer of its own, never a chunk of one. Only the actions that
        // load data interrupt a chunked transfer — a put or delete leaves the
        // in-flight load alone, just as the reference does.
        if command.medium != Medium::Direct && command.action.is_deferred() {
            self.transfer = None;
            let mut transfer = Transfer::new(command);
            transfer.chars = mem::take(&mut self.chars);
            let command = transfer.command.clone();

            return match transfer.finish() {
                Ok(payload) => self.dispatch(&command, payload, cursor),
                Err(error) => self.failure(&command, error),
            };
        }

        let mut transfer = match self.transfer.take() {
            Some(mut transfer) => {
                // A continuation chunk carries only `m` and `q`. A chunk that
                // carries no `q` of its own leaves the transmission's alone, so
                // the quiet policy of the first chunk governs the reply.
                transfer.command.more = command.more;
                if command.quiet != 0 {
                    transfer.command.quiet = command.quiet;
                }
                transfer
            },
            None => Transfer::new(command),
        };

        transfer.chars.append(&mut self.chars);

        if transfer.command.more {
            return match transfer.decode_complete_groups() {
                Ok(()) => {
                    self.transfer = Some(transfer);
                    GraphicsUpdate::default()
                },
                Err(error) => {
                    let command = transfer.command;
                    self.failure(&command, error)
                },
            };
        }

        let command = transfer.command.clone();
        match transfer.finish() {
            Ok(data) => self.dispatch(&command, data, cursor),
            Err(error) => self.failure(&command, error),
        }
    }

    /// Shift placements covered by a scrolling region.
    pub fn scroll(&mut self, region: &Range<Line>, delta: i32) {
        self.placements.scroll(region, delta);
    }

    /// Drop state that has scrolled out of the grid.
    pub fn prune(&mut self, top: Line, bottom: Line) {
        self.placements.retain_within(top, bottom);
        self.drop_orphaned_images();
    }

    /// Release images that lost their last placement and can never be
    /// addressed again because the client gave them no identity.
    ///
    /// The reference implementation frees such an image as soon as its last
    /// reference scrolls out of the buffer, rather than holding its pixels
    /// until the quota reclaims them.
    fn drop_orphaned_images(&mut self) {
        let orphaned: Vec<u32> = self
            .images
            .iter()
            .filter(|(internal_id, image)| {
                image.client_id == 0
                    && image.number == 0
                    && !self.placements.is_referenced(*internal_id)
            })
            .map(|(internal_id, _)| internal_id)
            .collect();
        for internal_id in orphaned {
            self.images.remove(internal_id);
        }
    }

    /// Remove every image and placement.
    pub fn clear(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.transfer = None;
        self.jobs.clear();
    }

    /// Remove what is on the screen, keeping placements that live entirely in
    /// the scrollback and the pixels of images the client can still address.
    ///
    /// Screen clears and resets take only the visible part, as the reference
    /// implementation does; scrolling back afterwards still shows the images
    /// the history holds.
    pub fn clear_visible(&mut self) {
        self.placements.retain_scrollback();
        self.drop_orphaned_images();
    }

    /// Take the decodes queued since the last call.
    ///
    /// Decompression and pixel decoding are proportional to the image rather
    /// than to the command, so the caller is expected to run these with the
    /// terminal unlocked and then hand each outcome to
    /// [`install`](Self::install). Until it does, an affected image is stored
    /// without pixels and is not drawn.
    pub fn take_jobs(&mut self) -> Vec<DecodeJob> {
        mem::take(&mut self.jobs)
    }

    /// Apply the outcome of a decode obtained from [`take_jobs`](Self::take_jobs).
    ///
    /// Returns the update the caller should act on, which carries the response
    /// the client has been waiting for since it sent the payload.
    pub fn install(&mut self, result: DecodeResult) -> GraphicsUpdate {
        let DecodeResult { internal_id, command, result, pending } = result;

        // A file-medium transmission was not reserved when it was parsed, since
        // its file had not been read yet. Its image, and for `a=T` its
        // placement, are established now that the pixels exist.
        if let Some(pending) = pending {
            return match result {
                Ok(decoded) => self.store_file_transmission(&command, decoded, pending),
                Err(error) => self.failure(&command, error),
            };
        }

        match result {
            Ok(decoded) => {
                // A query stores nothing, so it can never change the screen. For a
                // transmission, the image may have been evicted or deleted while
                // the decode ran; its pixels are then dropped and the
                // transmission counts as having changed nothing, exactly as an
                // eviction does.
                let changed = match internal_id {
                    Some(internal_id) => self.images.complete(internal_id, decoded),
                    None => false,
                };

                self.success(&command, changed)
            },
            Err(error) => {
                // Nothing will be displayed, so the space reserved for it is
                // released along with any placement that referred to it.
                if let Some(internal_id) = internal_id {
                    self.placements.remove_image(internal_id);
                    self.images.remove(internal_id);
                }

                self.failure(&command, error)
            },
        }
    }

    /// Carry out a parsed command.
    fn dispatch(&mut self, command: &Command, data: Vec<u8>, cursor: Point) -> GraphicsUpdate {
        // The identifier and the number are two different addressing schemes,
        // and the reference rejects a command that mixes them.
        if command.image_id != 0 && command.image_number != 0 {
            return self.failure(command, GraphicsError::Invalid);
        }

        // A file medium's payload is a path, and only the actions that load
        // data consult it; the others ignore the medium entirely without so
        // much as checking that the path is well formed, as the reference does.
        let source = match command.medium {
            Medium::Direct => Source::Inline(data),
            Medium::SharedMemory if command.action.is_deferred() => {
                return self.failure(command, GraphicsError::Unsupported)
            },
            Medium::File | Medium::TempFile if command.action.is_deferred() => {
                match file::FileSource::new(command, &data) {
                    Ok(source) => Source::File(source),
                    Err(error) => return self.failure(command, error),
                }
            },
            Medium::File | Medium::TempFile | Medium::SharedMemory => Source::Inline(Vec::new()),
        };

        let outcome = match command.action {
            Action::Transmit => self.transmit(command, source, cursor).map(|_| (true, None)),
            Action::TransmitAndDisplay => match self.transmit(command, source, cursor) {
                // A file medium cannot be reserved until its file has been read,
                // so its placement is added once the decode has landed.
                Ok(Some(internal_id)) => {
                    self.place(command, internal_id, cursor).map(|advance| (true, advance))
                },
                Ok(None) => Ok((true, None)),
                Err(error) => Err(error),
            },
            Action::Put => match self.resolve(command) {
                Some(internal_id) => {
                    self.place(command, internal_id, cursor).map(|advance| (true, advance))
                },
                None => Err(GraphicsError::NotFound),
            },
            Action::Query => {
                // A query without an identifier is ignored outright: the
                // reference logs it and answers nothing, and in particular
                // never reads the file a file medium named.
                if command.image_id == 0 {
                    return GraphicsUpdate::default();
                }

                self.validate(command, source).map(|()| (false, None))
            },
            Action::Delete => self.delete(command, cursor).map(|()| (true, None)),
            Action::Frame | Action::Animate | Action::Compose => Err(GraphicsError::Unsupported),
        };

        match outcome {
            Ok((changed, cursor_advance)) => {
                // A deferred command has not decoded its payload yet, so whether it
                // was accepted is still unknown and the reply is left to `install`.
                if command.action.is_deferred() {
                    return GraphicsUpdate { response: None, changed, cursor_advance };
                }

                let mut update = self.success(command, changed);
                update.cursor_advance = cursor_advance;
                update
            },
            Err(error) => self.failure(command, error),
        }
    }

    /// Reserve space for a transmission and queue its payload for decoding.
    ///
    /// Returns the internal identifier of the reserved image, or `None` when
    /// the payload is a file whose read has been queued: such a transmission
    /// cannot be reserved until the file has supplied the image's dimensions.
    fn transmit(
        &mut self,
        command: &Command,
        source: Source,
        cursor: Point,
    ) -> Result<Option<u32>, GraphicsError> {
        check_dimensions(command)?;

        // A file medium is read away from the event thread, so neither the
        // dimensions a PNG states in its own header nor the pixels themselves
        // are available yet. The image and, for `a=T`, its placement are
        // therefore established by `install` once the decode has landed.
        let data = match source {
            Source::File(file) => {
                self.jobs.push(DecodeJob {
                    internal_id: None,
                    command: command.clone(),
                    source: Source::File(file),
                    limit: decompression_limit(command, &self.config),
                    max_dimension: self.config.max_image_dimension,
                    allow_network_paths: self.config.allow_network_paths,
                    pending: Some(Pending {
                        anchor: cursor,
                        display: command.action == Action::TransmitAndDisplay,
                    }),
                });

                return Ok(None);
            },
            Source::Inline(data) => data,
        };

        let limit = decompression_limit(command, &self.config);
        let max_dimension = self.config.max_image_dimension;

        // A placement has to be anchored and quota reserved before the payload
        // is decoded, so the dimensions are established here. A PNG carries
        // them in a header, which a compressed payload has to be expanded to
        // reach; raw formats declare theirs in the control data and stay
        // compressed until the worker runs.
        let (data, mut command, (width, height)) = match (command.format, command.compression) {
            (Format::Png, Some(_)) => {
                let data = decompress(command, data, limit)?;
                let mut expanded = command.clone();
                expanded.compression = None;
                let shape = image::probe_png(&data, max_dimension)?;
                (data, expanded, shape)
            },
            (Format::Png, None) => {
                let shape = image::probe_png(&data, max_dimension)?;
                (data, command.clone(), shape)
            },
            _ => {
                let shape = image::probe_raw(
                    command.format,
                    command.source_width,
                    command.source_height,
                    max_dimension,
                )?;
                (data, command.clone(), shape)
            },
        };

        // The payload has not been decoded, so the layout it will produce is
        // not known yet and the largest one is reserved for it. A raw
        // transmission already fixes its layout, so it reserves exactly what
        // it will occupy; a PNG may still turn out to have an alpha channel.
        let channels = if command.format == Format::Rgb {
            image::PixelFormat::Rgb.channels()
        } else {
            image::PixelFormat::Rgba.channels()
        };
        let reserved = image::max_pixel_bytes(width, height, channels);
        self.make_room(reserved)?;

        // Re-transmitting a client identifier replaces the image and drops its
        // existing placements, as the protocol requires. A number is not an
        // identifier: images identified by number coexist, and lookups pick
        // the newest.
        if command.image_id != 0 {
            if let Some(existing) = self.images.resolve(command.image_id, 0) {
                self.placements.remove_image(existing);
            }
        }

        // A number-only transmission is assigned a free client identifier,
        // as the reference implementation does, so the reply can carry it and
        // the client can address the image by id from then on.
        if command.image_id == 0 && command.image_number != 0 {
            command.image_id = self.images.free_client_id();
        }

        let (internal_id, _) = self.images.insert_pending(
            command.image_id,
            command.image_number,
            width,
            height,
            reserved,
            command.transient,
        );

        self.jobs.push(DecodeJob {
            internal_id: Some(internal_id),
            command,
            source: Source::Inline(data),
            limit,
            max_dimension,
            allow_network_paths: self.config.allow_network_paths,
            pending: None,
        });

        Ok(Some(internal_id))
    }

    /// Queue a validation of image data that is not stored (`a=q`).
    fn validate(&mut self, command: &Command, source: Source) -> Result<(), GraphicsError> {
        check_dimensions(command)?;

        self.jobs.push(DecodeJob {
            internal_id: None,
            command: command.clone(),
            source,
            limit: decompression_limit(command, &self.config),
            max_dimension: self.config.max_image_dimension,
            allow_network_paths: self.config.allow_network_paths,
            pending: None,
        });

        Ok(())
    }

    /// Store and, for `a=T`, anchor an image that was read from a file.
    ///
    /// The image could not be reserved while the command was parsed, because a
    /// PNG states its dimensions in its own header and those only exist once
    /// the file has been read, so the accounting is charged here instead.
    fn store_file_transmission(
        &mut self,
        command: &Command,
        decoded: Decoded,
        pending: Pending,
    ) -> GraphicsUpdate {
        let mut command = command.clone();

        if let Err(error) = self.make_room(decoded.pixels.len()) {
            return self.failure(&command, error);
        }

        // Re-transmitting a client identifier replaces the image and drops its
        // placements, exactly as an inline transmission does.
        if command.image_id != 0 {
            if let Some(existing) = self.images.resolve(command.image_id, 0) {
                self.placements.remove_image(existing);
            }
        }

        // A number-only transmission is assigned a free client identifier so
        // that the reply can carry it.
        if command.image_id == 0 && command.image_number != 0 {
            command.image_id = self.images.free_client_id();
        }

        let (internal_id, _) =
            self.images.insert(command.image_id, command.image_number, decoded, command.transient);

        let advance = match if pending.display {
            self.place(&command, internal_id, pending.anchor)
        } else {
            Ok(None)
        } {
            Ok(advance) => advance,
            Err(error) => {
                // The placement is the transmission's visible outcome, so its
                // failure fails the transmission just as it does inline.
                self.placements.remove_image(internal_id);
                self.images.remove(internal_id);
                return self.failure(&command, error);
            },
        };

        let mut update = self.success(&command, true);
        update.cursor_advance = advance;

        update
    }

    /// Anchor a placement of a stored image at the cursor.
    fn place(
        &mut self,
        command: &Command,
        internal_id: u32,
        cursor: Point,
    ) -> Result<Option<CellFootprint>, GraphicsError> {
        let image = self.images.get(internal_id).ok_or(GraphicsError::NotFound)?;

        let (width, height) = (image.width, image.height);
        let source = SourceRect {
            x: command.src_x,
            y: command.src_y,
            width: if command.src_width == 0 { width } else { command.src_width },
            height: if command.src_height == 0 { height } else { command.src_height },
        };

        let footprint = resolve_footprint(command, source, self.cell_size);
        let placement = Placement {
            internal_id,
            placement_id: command.placement_id,
            anchor: cursor,
            footprint,
            offset_x: clamp_to_u16(command.cell_offset_x),
            offset_y: clamp_to_u16(command.cell_offset_y),
            // A size of zero means "as many cells as needed", which the renderer
            // resolves. The cell footprint is only needed to move the cursor.
            columns: clamp_to_u16(command.columns),
            rows: clamp_to_u16(command.rows),
            src: source,
            z_index: command.z_index,
            is_virtual: command.virtual_placement,
        };

        let advance = if command.cursor_movement == CursorMovement::Move && !placement.is_virtual {
            Some(placement.footprint)
        } else {
            None
        };

        self.placements.put(placement);

        Ok(advance)
    }

    /// Delete images and placements (`a=d`).
    fn delete(&mut self, command: &Command, cursor: Point) -> Result<(), GraphicsError> {
        let selector = command.delete.unwrap_or(b'a');
        let free_data = selector.is_ascii_uppercase();
        let internal = self.images.resolve(command.image_id, command.image_number);

        // The cell, column and row selectors take cursor positions, which the
        // protocol counts from one, so the coordinates are shifted here. A
        // value of zero is treated as the first position, as `saturating_sub`
        // makes it indistinguishable from one.
        let column = crate::index::Column(command.src_x.saturating_sub(1) as usize);
        let row = Line(command.src_y.saturating_sub(1) as i32);
        let point = Point::new(row, column);

        // Images whose placements a selector below removes, so that a freeing
        // selector can release the ones that lost their last placement.
        let referenced: Vec<u32> =
            self.placements.iter().map(|placement| placement.internal_id).collect();

        match selector.to_ascii_lowercase() {
            // Everything on the screen; scrollback placements survive.
            b'a' => self.placements.retain_scrollback(),
            // Everything belonging to one image, optionally one placement.
            b'i' | b'n' => match internal {
                Some(internal_id) => {
                    if command.placement_id != 0 {
                        self.placements.remove_placement(internal_id, command.placement_id);
                    } else {
                        self.placements.remove_image(internal_id);
                    }
                },
                None => return Err(GraphicsError::NotFound),
            },
            // Placements intersecting a cell.
            b'c' => self.placements.remove_at(cursor),
            b'p' => self.placements.remove_at(point),
            b'q' => self.remove_at_with_z(point, command.z_index),
            // Placements intersecting a whole column or row.
            b'x' => self.placements.remove_column(column),
            b'y' => self.placements.remove_row(row),
            b'z' => self.placements.remove_with_z(command.z_index),
            // Animation frames are not implemented.
            b'f' => return Err(GraphicsError::Unsupported),
            // A range of image identifiers.
            b'r' => {
                let range = command.src_x..=command.src_y;
                let ids: Vec<u32> = range
                    .clone()
                    .filter_map(|client_id| self.images.resolve(client_id, 0))
                    .collect();
                for internal_id in ids {
                    self.placements.remove_image(internal_id);
                    if free_data {
                        self.images.remove(internal_id);
                    }
                }
            },
            _ => return Err(GraphicsError::Invalid),
        }

        if free_data {
            if let Some(internal_id) = internal {
                if !self.placements.is_referenced(internal_id) {
                    self.images.remove(internal_id);
                }
            }

            // A freeing selector releases the pixels of every image that lost
            // its last placement to it, not only one resolved from an
            // identifier, as the reference implementation's filter does.
            for internal_id in referenced {
                if !self.placements.is_referenced(internal_id)
                    && self.images.get(internal_id).is_some()
                {
                    self.images.remove(internal_id);
                }
            }
        } else {
            // Even a non-freeing selector drops the pixels of an anonymous
            // image it orphaned: nothing can address it again.
            for internal_id in referenced {
                if !self.placements.is_referenced(internal_id)
                    && self.images.get(internal_id).is_some()
                    && self.images.get(internal_id).is_some_and(|image| image.client_id == 0)
                {
                    self.images.remove(internal_id);
                }
            }
        }

        Ok(())
    }

    /// Remove placements covering `point` with a matching stacking order.
    fn remove_at_with_z(&mut self, point: Point, z_index: i32) {
        let matching: Vec<(u32, u32)> = self
            .placements
            .iter()
            .filter(|placement| placement.z_index == z_index && placement.contains(point))
            .map(|placement| (placement.internal_id, placement.placement_id))
            .collect();

        for (internal_id, placement_id) in matching {
            self.placements.remove_placement(internal_id, placement_id);
        }
    }

    /// Resolve the image a command addresses.
    fn resolve(&self, command: &Command) -> Option<u32> {
        self.images.resolve(command.image_id, command.image_number)
    }

    /// Make room for `needed` bytes of pixel data.
    fn make_room(&mut self, needed: usize) -> Result<(), GraphicsError> {
        if !self.images.is_full(needed) {
            return Ok(());
        }

        // Prefer images nothing refers to.
        let referenced: Vec<u32> =
            self.placements.iter().map(|placement| placement.internal_id).collect();
        let is_referenced = |internal_id: u32| referenced.contains(&internal_id);

        if self.images.evict_unreferenced(needed, &is_referenced) {
            return Ok(());
        }

        // Everything left is on screen, so images have to go, along with their
        // placements. Dropping the newest instead would silently discard the
        // image the client just asked to display.
        //
        // Images the client marked as transient (`N=1`) go first, regardless of
        // age, as the protocol and the reference implementation require.
        for internal_id in self.images.by_age() {
            if !self.images.is_full(needed) {
                break;
            }
            self.placements.remove_image(internal_id);
            self.images.remove(internal_id);
        }

        if self.images.is_full(needed) {
            Err(GraphicsError::NoSpace)
        } else {
            Ok(())
        }
    }

    /// Build a successful response.
    fn success(&self, command: &Command, changed: bool) -> GraphicsUpdate {
        GraphicsUpdate {
            response: if command.suppresses_ok() { None } else { self.response(command, "OK") },
            changed,
            cursor_advance: None,
        }
    }

    /// Build a failure response.
    fn failure(&self, command: &Command, error: GraphicsError) -> GraphicsUpdate {
        let response = if command.suppresses_all() {
            None
        } else {
            let mut message = String::with_capacity(32);
            let _ = write!(message, "{}:{}", error.code(), error.message());
            self.response(command, &message)
        };

        GraphicsUpdate { response, changed: false, cursor_advance: None }
    }

    /// Format a response, if the client asked for one.
    fn response(&self, command: &Command, body: &str) -> Option<String> {
        if !command.wants_response() {
            return None;
        }

        let mut response = String::with_capacity(body.len() + 24);
        response.push_str("\x1b_G");
        let mut separator = "";
        if command.image_id != 0 {
            let _ = write!(response, "i={}", command.image_id);
            separator = ",";
        }
        if command.image_number != 0 {
            let _ = write!(response, "{separator}I={}", command.image_number);
            separator = ",";
        }
        if command.placement_id != 0 {
            let _ = write!(response, "{separator}p={}", command.placement_id);
        }
        response.push(';');
        response.push_str(body);
        response.push_str("\x1b\\");

        Some(response)
    }
}

/// Clamp a client supplied value into the range a placement can hold.
fn clamp_to_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Resolve the cell footprint of a placement, in columns and rows.
///
/// Mirrors the reference implementation: a cell count of zero means "as many
/// cells as needed". With neither axis given the footprint covers the image's
/// pixel size, and with one axis given the other follows from the aspect ratio.
fn resolve_footprint(
    command: &Command,
    source: SourceRect,
    cell_size: (u16, u16),
) -> CellFootprint {
    /// Smallest footprint that can hold anything.
    const MIN_CELLS: f64 = 1.;

    let cell_width = f64::from(cell_size.0.max(1));
    let cell_height = f64::from(cell_size.1.max(1));
    let offset_x = f64::from(command.cell_offset_x);
    let offset_y = f64::from(command.cell_offset_y);
    let source_width = f64::from(source.width.max(1));
    let source_height = f64::from(source.height.max(1));

    let auto_columns = command.columns == 0;
    let auto_rows = command.rows == 0;

    let mut columns = f64::from(command.columns);
    let mut rows = f64::from(command.rows);

    if auto_columns {
        columns = if auto_rows {
            ((source_width + offset_x) / cell_width).ceil()
        } else {
            let height = cell_height * rows + offset_y;
            (height * source_width / source_height / cell_width).ceil()
        };
    }

    if auto_rows {
        rows = if auto_columns {
            ((source_height + offset_y) / cell_height).ceil()
        } else {
            let width = cell_width * columns + offset_x;
            (width * source_height / source_width / cell_height).ceil()
        };
    }

    CellFootprint {
        columns: clamp_to_u16(columns.max(MIN_CELLS) as u32),
        rows: clamp_to_u16(rows.max(MIN_CELLS) as u32),
    }
}

/// Where a decode takes its payload from.
#[derive(Debug)]
enum Source {
    /// Payload bytes the command carried inline.
    Inline(Vec<u8>),
    /// A window of a file the command named, yet to be read.
    File(file::FileSource),
}

/// A transmission whose image is only established once its payload is known.
///
/// The payload of a file medium cannot be measured while the command is
/// parsed, since neither the pixels nor, for a PNG, the dimensions that live
/// in the file's own header are available until it has been read.
#[derive(Clone, Copy, Debug)]
struct Pending {
    /// Where to anchor the placement, for `a=T`.
    anchor: Point,
    /// Whether the image is displayed as well as stored.
    display: bool,
}

/// A decode handed off by the parser so that its cost is not paid while the
/// terminal is locked.
///
/// Reading a file, decompressing a payload and decoding pixels dominate a
/// transmission and are proportional to the image, so performing them under
/// the lock stalls the display for as long as the image takes to decode.
#[derive(Debug)]
pub struct DecodeJob {
    /// Image the pixels belong to, or `None` for a validation query.
    internal_id: Option<u32>,
    /// Command the payload belongs to, holding any compression still to undo.
    command: Command,
    /// Where the payload comes from.
    source: Source,
    /// Bound on the bytes the payload may expand to.
    limit: usize,
    /// Maximum image dimension the terminal will allocate.
    max_dimension: u32,
    /// Whether a file medium may name a path that leaves this machine.
    allow_network_paths: bool,
    /// Transmission to complete once the payload has been decoded.
    pending: Option<Pending>,
}

impl DecodeJob {
    /// Decode the payload.
    ///
    /// Touches no terminal state, so it is safe to run without the lock.
    pub fn decode(self) -> DecodeResult {
        let max_to_read = max_read_size(&self.command);

        let result = match self.source {
            Source::Inline(data) => {
                decode_payload(&self.command, data, self.limit, self.max_dimension)
            },
            Source::File(file) => file::read(&file, max_to_read, self.allow_network_paths)
                .and_then(|data| {
                    decode_payload(&self.command, data, self.limit, self.max_dimension)
                }),
        };

        DecodeResult {
            internal_id: self.internal_id,
            command: self.command,
            result,
            pending: self.pending,
        }
    }
}

/// The outcome of a [`DecodeJob`].
pub struct DecodeResult {
    /// Image the pixels belong to, or `None` for a validation query.
    internal_id: Option<u32>,
    /// Command the result belongs to, used to build the response.
    command: Command,
    /// Pixels, or the reason the payload was rejected.
    result: Result<Decoded, GraphicsError>,
    /// Transmission that still has to store and anchor its image.
    pending: Option<Pending>,
}

/// Bound on the bytes a file medium may read.
///
/// A payload that still has to be decompressed or parsed as a PNG is not
/// bounded by its declared pixels, so the protocol's data cap applies instead.
/// A raw payload states exactly how many bytes it needs.
fn max_read_size(command: &Command) -> usize {
    if command.compression.is_some() || command.format == Format::Png {
        return file::MAX_DATA_SIZE;
    }

    let bytes_per_pixel = command.format.bytes_per_pixel().unwrap_or(image::RGBA_CHANNELS);
    (command.source_width as usize)
        .saturating_mul(command.source_height as usize)
        .saturating_mul(bytes_per_pixel)
}

/// Decompress a payload and decode it into pixels.
fn decode_payload(
    command: &Command,
    data: Vec<u8>,
    limit: usize,
    max_dimension: u32,
) -> Result<Decoded, GraphicsError> {
    let data = decompress(command, data, limit)?;

    // Compressed data must inflate to exactly the bytes the control data
    // declares: a short result is its own error, not missing data, since the
    // whole payload did arrive.
    if command.compression.is_some() && command.format != Format::Png {
        let expected = (command.source_width as usize)
            .saturating_mul(command.source_height as usize)
            .saturating_mul(command.format.bytes_per_pixel().unwrap_or(image::RGBA_CHANNELS));
        if expected > 0 && data.len() < expected {
            return Err(GraphicsError::InflationSize);
        }
    }

    image::decode(command.format, &data, command.source_width, command.source_height, max_dimension)
}

/// Reject dimensions the protocol rules out before any payload is touched.
///
/// The reference makes both checks before it reads a file medium's file, so
/// the checks belong here rather than after a decode.
fn check_dimensions(command: &Command) -> Result<(), GraphicsError> {
    if command.format != Format::Png && (command.source_width == 0 || command.source_height == 0) {
        return Err(GraphicsError::ZeroDimensions);
    }

    const MAX_IMAGE_DIMENSION: u32 = 10000;
    if command.source_width > MAX_IMAGE_DIMENSION || command.source_height > MAX_IMAGE_DIMENSION {
        return Err(GraphicsError::ImageTooLarge);
    }

    Ok(())
}

/// Undo the payload compression requested by the command, if any.
///
/// Kitty compresses with zlib *before* base64 encoding, so this runs on the
/// already decoded payload. `limit` bounds the output so that a small payload
/// cannot expand into an arbitrarily large allocation.
fn decompress(command: &Command, data: Vec<u8>, limit: usize) -> Result<Vec<u8>, GraphicsError> {
    match command.compression {
        None => Ok(data),
        Some(Compression::Zlib) => {
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&data, limit)
                .map_err(|_| GraphicsError::BadCompression)
        },
    }
}

/// Upper bound on the bytes a payload may decompress to.
///
/// Raw formats declare their exact size, so anything larger is invalid. PNG
/// streams carry their own dimensions, so the configured memory limit is used
/// instead.
fn decompression_limit(command: &Command, config: &GraphicsConfig) -> usize {
    let bytes_per_pixel = command.format.bytes_per_pixel().unwrap_or(image::RGBA_CHANNELS);
    let declared = (command.source_width as usize)
        .saturating_mul(command.source_height as usize)
        .saturating_mul(bytes_per_pixel);

    if declared > 0 {
        declared
    } else {
        config.memory_limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Column;

    /// Encode bytes as base64 the way a client would.
    fn encode(data: &[u8]) -> String {
        BASE64.encode(data)
    }

    /// Path of a file under the temporary directory, unique per call.
    fn temp_path(name: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("alacritty-graphics-{}-{unique}-{name}", std::process::id()))
    }

    /// The base64 payload that names a path for a file medium.
    fn path_payload(path: &std::path::Path) -> String {
        encode(path.as_os_str().as_encoded_bytes())
    }

    /// Feed one complete APC string to the store.
    /// Feed one APC string to the store, then run any decode it queues.
    ///
    /// Decoding is deferred to whoever drains `take_jobs`, so the tests settle
    /// it here to leave the store in the state the event loop would reach. The
    /// cursor advance comes from parsing and the response from the decode, which
    /// is how they are applied in production.
    fn apc(store: &mut GraphicsStore, payload: &str) -> GraphicsUpdate {
        store.apc_start();
        for byte in payload.bytes() {
            store.apc_put(byte);
        }

        let mut update = store.apc_end(Point::new(Line(0), Column(0)));

        for result in store.take_jobs() {
            let settled = store.install(result.decode());
            update.response = settled.response;
            update.changed |= settled.changed;
        }

        update
    }

    /// A 1x1 red RGBA image payload.
    fn red_pixel() -> String {
        encode(&[255, 0, 0, 255])
    }

    #[test]
    fn tolerates_a_spacer_after_the_prefix() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let payload = format!("G f=32,s=1,v=1,a=T,i=6;{}", red_pixel());

        let update = apc(&mut store, &payload);

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=6;OK\x1b\\"));
        assert_eq!(store.placements().len(), 1);
    }

    #[test]
    fn placing_an_image_reports_the_cursor_advance() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        store.set_cell_size(10, 20);

        // A 25x45 image covers 3x3 cells at a 10x20 cell size.
        let pixels = vec![0u8; 25 * 45 * 3];
        let payload = BASE64.encode(&pixels);
        let sequence = format!("Gf=24,s=25,v=45,a=T,i=20;{payload}");

        let update = apc(&mut store, &sequence);

        assert_eq!(update.cursor_advance, Some(CellFootprint { columns: 3, rows: 3 }));
    }

    #[test]
    fn an_explicit_cell_size_overrides_the_resolved_one() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        store.set_cell_size(10, 20);

        let pixels = vec![0u8; 25 * 45 * 3];
        let payload = BASE64.encode(&pixels);
        let sequence = format!("Gf=24,s=25,v=45,a=T,i=21,c=7,r=2;{payload}");

        let update = apc(&mut store, &sequence);

        assert_eq!(update.cursor_advance, Some(CellFootprint { columns: 7, rows: 2 }));
    }

    #[test]
    fn the_cursor_can_be_pinned_in_place() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        store.set_cell_size(10, 20);

        let pixels = vec![0u8; 25 * 45 * 3];
        let payload = BASE64.encode(&pixels);
        let sequence = format!("Gf=24,s=25,v=45,a=T,i=22,C=1;{payload}");

        let update = apc(&mut store, &sequence);

        assert_eq!(update.cursor_advance, None, "C=1 must not move the cursor");
        assert_eq!(store.placements().len(), 1);
    }

    #[test]
    fn decompresses_a_zlib_payload() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());

        // This is the shape `kitten icat` uses: zlib-compressed RGB, sent with
        // a silenced response.
        let pixels = [255u8, 0, 0, 255];
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&pixels, 6);
        let payload = BASE64.encode(&compressed);
        let sequence = format!("Gf=32,s=1,v=1,a=T,i=11,o=z;{payload}");

        let update = apc(&mut store, &sequence);

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=11;OK\x1b\\"));
        assert_eq!(store.placements().len(), 1);
        assert_eq!(store.memory_used(), 4);
    }

    #[test]
    fn rejects_corrupt_compressed_payloads() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let payload = BASE64.encode(b"not zlib at all");
        let sequence = format!("Gf=32,s=1,v=1,a=T,i=12,o=z;{payload}");

        let update = apc(&mut store, &sequence);

        assert_eq!(
            update.response.as_deref(),
            Some("\x1b_Gi=12;EINVAL:could not decompress image data\x1b\\")
        );
        assert!(store.placements().is_empty());
    }

    #[test]
    fn transmits_and_displays_an_image() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let payload = format!("Gf=32,s=1,v=1,a=T,i=7;{}", red_pixel());

        let update = apc(&mut store, &payload);

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=7;OK\x1b\\"));
        assert!(update.changed);
        assert_eq!(store.placements().len(), 1);
        assert_eq!(store.memory_used(), 4);
    }

    #[test]
    fn reassembles_a_chunked_transmission() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let data = encode(&[1, 2, 3, 255, 4, 5, 6, 255]);

        // Split the payload across three APC strings.
        let first = format!("Gf=32,s=2,v=1,a=T,i=9,m=1;{}", &data[..4]);
        let second = format!("Gm=1;{}", &data[4..8]);
        let third = format!("Gm=0;{}", &data[8..]);

        assert!(apc(&mut store, &first).response.is_none());
        assert!(apc(&mut store, &second).response.is_none());

        let update = apc(&mut store, &third);
        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=9;OK\x1b\\"));
        assert_eq!(store.memory_used(), 8);
    }

    #[test]
    fn survives_arbitrary_byte_boundaries() {
        let payload = format!("Gf=32,s=1,v=1,a=T,i=3;{}", red_pixel());

        for chunk_size in 1..=payload.len() {
            let mut store = GraphicsStore::new(GraphicsConfig::default());
            store.apc_start();
            for chunk in payload.as_bytes().chunks(chunk_size) {
                for byte in chunk {
                    store.apc_put(*byte);
                }
            }
            let mut update = store.apc_end(Point::new(Line(0), Column(0)));

            // The reply follows the decode, as it does in the event loop.
            for result in store.take_jobs() {
                update.response = store.install(result.decode()).response;
            }

            assert_eq!(
                update.response.as_deref(),
                Some("\x1b_Gi=3;OK\x1b\\"),
                "chunk size {chunk_size}"
            );
            assert_eq!(store.placements().len(), 1, "chunk size {chunk_size}");
        }
    }

    #[test]
    fn query_does_not_store() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let payload = format!("Gf=32,s=1,v=1,a=q,i=1;{}", red_pixel());

        let update = apc(&mut store, &payload);

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=1;OK\x1b\\"));
        assert!(!update.changed);
        assert!(store.placements().is_empty());
        assert_eq!(store.memory_used(), 0);
    }

    #[test]
    fn quiet_levels_silence_responses() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let ok = format!("Gf=32,s=1,v=1,a=q,i=1,q=1;{}", red_pixel());
        assert!(apc(&mut store, &ok).response.is_none());

        let failure = format!("Gf=32,s=1,v=1,a=q,i=1,q=2;{}", encode(&[0, 0]));
        assert!(apc(&mut store, &failure).response.is_none());
    }

    #[test]
    fn reports_failures() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());

        // Truncated payload.
        let update = apc(&mut store, "Gf=32,s=4,v=4,a=T,i=2;AAAA");
        assert_eq!(
            update.response.as_deref(),
            Some("\x1b_Gi=2;ENODATA:not enough image data\x1b\\")
        );

        // Placing an unknown image.
        let update = apc(&mut store, "Ga=p,i=99");
        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=99;ENOENT:image not found\x1b\\"));

        // Shared memory transmission is not implemented and stays refused, so
        // that a client probing for it is told the terminal does not have it.
        let update = apc(&mut store, "Ga=t,f=32,t=s,s=1,v=1,i=4;/dev/shm/foo");
        assert_eq!(
            update.response.as_deref(),
            Some("\x1b_Gi=4;EUNSUPPORTED:unsupported command\x1b\\")
        );

        // A file that cannot be read reports one information-free error.
        let missing = std::env::temp_dir().join("alacritty-graphics-missing-file");
        let payload = path_payload(&missing);
        let update = apc(&mut store, &format!("Ga=t,f=24,s=1,v=1,i=4,t=f;{payload}"));
        assert_eq!(
            update.response.as_deref(),
            Some("\x1b_Gi=4;EBADF:Failed to read image file\x1b\\")
        );
    }

    #[test]
    fn an_icat_file_probe_succeeds() {
        // `kitten icat` probes for the file medium by validating a one pixel
        // raw image held in a marked temporary file, and treats an `OK` reply
        // as the terminal supporting it.
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let path = temp_path("tty-graphics-protocol-probe");
        std::fs::write(&path, [1u8, 2, 3]).expect("failed to write the probe file");
        let query = format!(
            "Ga=q,f=24,t=t,s=1,v=1,S={},i=77;{}",
            path.as_os_str().as_encoded_bytes().len(),
            path_payload(&path),
        );

        let update = apc(&mut store, &query);

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=77;OK\x1b\\"));
        assert_eq!(store.memory_used(), 0);
        assert!(!path.exists(), "the probe's temporary file must be removed");
    }

    #[test]
    fn reads_an_image_from_a_marked_temporary_file() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let path = temp_path("tty-graphics-protocol-read");
        std::fs::write(&path, [10u8, 20, 30]).expect("failed to write the temporary file");
        let payload = path_payload(&path);

        let update = apc(&mut store, &format!("Ga=t,f=24,t=t,s=1,v=1,i=5;{payload}"));

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=5;OK\x1b\\"));
        assert_eq!(store.memory_used(), 3);
        assert!(!path.exists(), "a `t=t` file is removed once it has been read");
    }

    #[test]
    fn keeps_a_file_that_was_not_sent_as_temporary() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let path = temp_path("tty-graphics-protocol-keep");
        std::fs::write(&path, [10u8, 20, 30]).expect("failed to write the temporary file");
        let payload = path_payload(&path);

        let update = apc(&mut store, &format!("Ga=t,f=24,t=f,s=1,v=1,i=6;{payload}"));

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=6;OK\x1b\\"));
        assert!(path.exists(), "a `t=f` file belongs to the client");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removes_an_opened_temporary_file_even_when_reading_fails() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let path = temp_path("tty-graphics-protocol-short");
        std::fs::write(&path, [1u8, 2, 3]).expect("failed to write the temporary file");
        let payload = path_payload(&path);

        // Two pixels need six bytes, but the file only holds three.
        let update = apc(&mut store, &format!("Ga=t,f=24,t=t,s=1,v=2,i=7;{payload}"));

        assert_eq!(
            update.response.as_deref(),
            Some("\x1b_Gi=7;EBADF:Failed to read image file\x1b\\")
        );
        assert!(!path.exists(), "an opened `t=t` file is removed even on failure");
    }

    #[test]
    fn refuses_a_filename_longer_than_the_protocol_allows() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let payload = encode("a".repeat(2049).as_bytes());

        let update = apc(&mut store, &format!("Ga=t,f=24,s=1,v=1,i=8,t=f;{payload}"));

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=8;EINVAL:Filename too long\x1b\\"));
    }

    #[test]
    fn an_embedded_nul_ends_the_path() {
        // The payload is a C string, so anything after a NUL is ignored.
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let path = temp_path("tty-graphics-protocol-nul");
        std::fs::write(&path, [10u8, 20, 30]).expect("failed to write the temporary file");
        let mut bytes = path.as_os_str().as_encoded_bytes().to_vec();
        bytes.extend_from_slice(b"\0/ignored");

        let update = apc(&mut store, &format!("Ga=t,f=24,s=1,v=1,i=9,t=f;{}", encode(&bytes)));

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=9;OK\x1b\\"));
        assert_eq!(store.memory_used(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn anonymous_transmissions_are_silent() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let payload = format!("Gf=32,s=1,v=1,a=T;{}", red_pixel());
        let update = apc(&mut store, &payload);
        assert!(update.response.is_none());
        assert_eq!(store.placements().len(), 1);
    }

    #[test]
    fn malformed_control_data_is_dropped_silently() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let payload = format!("Gf=32,s=1,v=1,a=T,i=5,Z=1;{}", red_pixel());
        let update = apc(&mut store, &payload);
        assert!(update.response.is_none());
        assert!(store.placements().is_empty());
    }

    #[test]
    fn non_graphics_apc_is_ignored() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let update = apc(&mut store, "Xnot graphics");
        assert!(update.response.is_none());
        assert!(!update.changed);
    }

    #[test]
    fn disabled_config_ignores_everything() {
        let mut store = GraphicsStore::new(GraphicsConfig::DISABLED);
        let payload = format!("Gf=32,s=1,v=1,a=T,i=1;{}", red_pixel());
        let update = apc(&mut store, &payload);
        assert!(update.response.is_none());
        assert_eq!(store.memory_used(), 0);
    }

    #[test]
    fn a_network_path_is_allowed_unless_the_configuration_refuses_it() {
        // A client that has its pixels on a share is served by default, and the
        // setting exists to take that away.
        assert!(GraphicsConfig::default().allow_network_paths);

        let refused = GraphicsConfig { allow_network_paths: false, ..GraphicsConfig::default() };
        assert!(!refused.allow_network_paths);
        // Only that one setting changes when it is turned off.
        assert!(refused.enabled);
        assert_eq!(refused.memory_limit, GraphicsConfig::default().memory_limit);
    }

    #[test]
    fn delete_removes_placements() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let payload = format!("Gf=32,s=1,v=1,a=T,i=8;{}", red_pixel());
        apc(&mut store, &payload);
        assert_eq!(store.placements().len(), 1);

        let update = apc(&mut store, "Ga=d,d=i,i=8");
        assert!(update.changed);
        assert!(store.placements().is_empty());
    }

    #[test]
    fn the_oldest_images_are_evicted_first() {
        // Room for exactly one 1x1 RGBA image.
        let config = GraphicsConfig { memory_limit: 4, ..GraphicsConfig::default() };
        let mut store = GraphicsStore::new(config);

        let first = format!("Gf=32,s=1,v=1,a=T,i=1;{}", red_pixel());
        assert_eq!(apc(&mut store, &first).response.as_deref(), Some("\x1b_Gi=1;OK\x1b\\"));

        // The second image cannot fit alongside the first, so the oldest one is
        // dropped rather than the new one being rejected: the client is asking
        // to display the new one now.
        let second = format!("Gf=32,s=1,v=1,a=T,i=2;{}", red_pixel());
        assert_eq!(apc(&mut store, &second).response.as_deref(), Some("\x1b_Gi=2;OK\x1b\\"));

        assert_eq!(store.memory_used(), 4);
        let placements = store.placements();
        assert_eq!(placements.len(), 1, "the evicted image's placement is dropped too");
        assert_eq!(store.image(placements[0].internal_id).unwrap().client_id, 2);
    }

    #[test]
    fn an_image_larger_than_the_limit_is_still_rejected() {
        let config = GraphicsConfig { memory_limit: 4, ..GraphicsConfig::default() };
        let mut store = GraphicsStore::new(config);

        // Two pixels of RGBA: nothing can be evicted to make room for it.
        let pixels = BASE64.encode([0u8; 8]);
        let sequence = format!("Gf=32,s=2,v=1,a=T,i=3;{pixels}");

        let update = apc(&mut store, &sequence);

        assert_eq!(
            update.response.as_deref(),
            Some("\x1b_Gi=3;ENOSPC:image storage limit reached\x1b\\")
        );
        assert!(store.placements().is_empty());
    }

    #[test]
    fn scroll_moves_placements_with_the_grid() {
        let mut store = GraphicsStore::new(GraphicsConfig::default());
        store.apc_start();
        for byte in format!("Gf=32,s=1,v=1,a=T,i=1;{}", red_pixel()).bytes() {
            store.apc_put(byte);
        }
        store.apc_end(Point::new(Line(5), Column(0)));

        store.scroll(&(Line(0)..Line(24)), 3);

        assert_eq!(store.placements()[0].anchor.line, Line(2));
    }

    #[test]
    fn oversized_images_are_rejected() {
        let config = GraphicsConfig { max_image_dimension: 2, ..GraphicsConfig::default() };
        let mut store = GraphicsStore::new(config);
        let payload = format!("Gf=32,s=8,v=1,a=T,i=1;{}", encode(&[0; 32]));
        let update = apc(&mut store, &payload);
        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=1;EFBIG:image too large\x1b\\"));
    }

    #[test]
    #[cfg(unix)]
    fn refuses_paths_outside_the_allowed_directories() {
        // `/proc` and `/sys` expose process and kernel state and `/dev` holds
        // device nodes and FIFOs, so none of them may be read; every refusal is
        // reported as the one generic file error.
        let mut store = GraphicsStore::new(GraphicsConfig::default());

        for (image_id, path) in [
            (20u32, std::path::Path::new("/proc/self/environ")),
            (21, std::path::Path::new("/proc/version")),
            (22, std::path::Path::new("/dev/null")),
        ] {
            let query = format!("Ga=t,f=24,s=1,v=1,i={image_id},t=f;{}", path_payload(path));
            let update = apc(&mut store, &query);

            assert_eq!(
                update.response.as_deref(),
                Some(format!("\x1b_Gi={image_id};EBADF:Failed to read image file\x1b\\").as_str()),
                "unexpected reply for {path:?}",
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn reads_a_file_from_the_shared_memory_directory() {
        // `/dev/shm` is the one exception to the `/dev` refusal, since that is
        // where clients put shared memory objects.
        let path = std::path::Path::new("/dev/shm")
            .join(format!("alacritty-graphics-{}-probe", std::process::id()));
        if std::fs::write(&path, [10u8, 20, 30]).is_err() {
            return;
        }

        let mut store = GraphicsStore::new(GraphicsConfig::default());
        let query = format!("Ga=t,f=24,s=1,v=1,i=23,t=f;{}", path_payload(&path));
        let update = apc(&mut store, &query);

        assert_eq!(update.response.as_deref(), Some("\x1b_Gi=23;OK\x1b\\"));
        assert_eq!(store.memory_used(), 3);

        let _ = std::fs::remove_file(&path);
    }
}
