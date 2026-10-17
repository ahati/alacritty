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
pub mod image;
pub mod placement;

use std::fmt::Write as _;
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::index::{Line, Point};

use self::command::{Action, Command, Compression, CursorMovement, Medium};
use self::image::{ImageData, ImageStore};
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
pub const DEFAULT_MEMORY_LIMIT: usize = 128 * 1024 * 1024;

/// Cell size assumed until the display reports the real one.
///
/// It only affects how image sizes given in cells are resolved, so a wrong
/// guess merely moves the cursor by an odd number of cells.
const DEFAULT_CELL_SIZE: (u16, u16) = (1, 1);

/// Configuration for graphics support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphicsConfig {
    /// Whether graphics escape sequences are handled at all.
    pub enabled: bool,
    /// Maximum width or height of an image, in pixels.
    pub max_image_dimension: u32,
    /// Maximum bytes of decoded image data to keep.
    pub memory_limit: usize,
}

impl Default for GraphicsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_image_dimension: DEFAULT_MAX_IMAGE_DIMENSION,
            memory_limit: DEFAULT_MEMORY_LIMIT,
        }
    }
}

impl GraphicsConfig {
    /// A configuration that ignores every graphics escape sequence.
    pub const DISABLED: Self = Self {
        enabled: false,
        max_image_dimension: DEFAULT_MAX_IMAGE_DIMENSION,
        memory_limit: DEFAULT_MEMORY_LIMIT,
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
        self.cell_size = (width.max(1), height.max(1));
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

        let mut transfer = match self.transfer.take() {
            Some(mut transfer) => {
                // A continuation chunk carries only `m` and `q`.
                transfer.command.more = command.more;
                transfer.command.quiet = command.quiet;
                transfer
            },
            None => Transfer::new(command),
        };

        // Reject transmission features we do not implement before touching the
        // payload: for file and shared memory mediums the payload is a path,
        // not base64, so it must never reach the decoder.
        if let Err(error) = unsupported_transmission(&transfer.command) {
            let command = transfer.command;
            return self.failure(&command, error);
        }

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
    }

    /// Remove every image and placement.
    pub fn clear(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.transfer = None;
    }

    /// Carry out a parsed command.
    fn dispatch(&mut self, command: &Command, data: Vec<u8>, cursor: Point) -> GraphicsUpdate {
        let outcome = match command.action {
            Action::Transmit => self.transmit(command, data).map(|_| (true, None)),
            Action::TransmitAndDisplay => self
                .transmit(command, data)
                .and_then(|internal_id| self.place(command, internal_id, cursor))
                .map(|advance| (true, advance)),
            Action::Put => match self.resolve(command) {
                Some(internal_id) => {
                    self.place(command, internal_id, cursor).map(|advance| (true, advance))
                },
                None => Err(GraphicsError::NotFound),
            },
            Action::Query => self.validate(command, data).map(|()| (false, None)),
            Action::Delete => self.delete(command, cursor).map(|()| (true, None)),
            Action::Frame | Action::Animate | Action::Compose => Err(GraphicsError::Unsupported),
        };

        match outcome {
            Ok((changed, cursor_advance)) => {
                let mut update = self.success(command, changed);
                update.cursor_advance = cursor_advance;
                update
            },
            Err(error) => self.failure(command, error),
        }
    }

    /// Decode and store image data.
    fn transmit(&mut self, command: &Command, data: Vec<u8>) -> Result<u32, GraphicsError> {
        let data = decompress(command, data, decompression_limit(command, &self.config))?;
        let decoded = image::decode(
            command.format,
            &data,
            command.source_width,
            command.source_height,
            self.config.max_image_dimension,
        )?;

        self.make_room(decoded.pixels.len())?;

        // Re-transmitting an identifier replaces the image and drops its
        // existing placements, as the protocol requires.
        if let Some(existing) = self.images.resolve(command.image_id, command.image_number) {
            self.placements.remove_image(existing);
        }

        let (internal_id, _) =
            self.images.insert(command.image_id, command.image_number, decoded, command.transient);

        Ok(internal_id)
    }

    /// Validate image data without storing it (`a=q`).
    fn validate(&self, command: &Command, data: Vec<u8>) -> Result<(), GraphicsError> {
        let data = decompress(command, data, decompression_limit(command, &self.config))?;
        image::decode(
            command.format,
            &data,
            command.source_width,
            command.source_height,
            self.config.max_image_dimension,
        )
        .map(|_| ())
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

        match selector.to_ascii_lowercase() {
            // Everything visible.
            b'a' => self.placements.clear(),
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
            b'p' => self.placements.remove_at(Point::new(
                Line(command.src_y as i32),
                crate::index::Column(command.src_x as usize),
            )),
            b'q' => {
                let point = Point::new(
                    Line(command.src_y as i32),
                    crate::index::Column(command.src_x as usize),
                );
                self.remove_at_with_z(point, command.z_index);
            },
            // Placements intersecting a whole column or row.
            b'x' => self.placements.remove_column(crate::index::Column(command.src_x as usize)),
            b'y' => self.placements.remove_row(Line(command.src_y as i32)),
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

        if self.images.is_full(needed) { Err(GraphicsError::NoSpace) } else { Ok(()) }
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

/// Reject transmission features this implementation does not support.
///
/// File, temporary file and shared memory mediums would let a client make the
/// terminal read arbitrary paths, so only direct transmission is accepted.
fn unsupported_transmission(command: &Command) -> Result<(), GraphicsError> {
    if command.medium != Medium::Direct {
        return Err(GraphicsError::Unsupported);
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

    if declared > 0 { declared } else { config.memory_limit }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Column;

    /// Encode bytes as base64 the way a client would.
    fn encode(data: &[u8]) -> String {
        BASE64.encode(data)
    }

    /// Feed one complete APC string to the store.
    fn apc(store: &mut GraphicsStore, payload: &str) -> GraphicsUpdate {
        store.apc_start();
        for byte in payload.bytes() {
            store.apc_put(byte);
        }
        store.apc_end(Point::new(Line(0), Column(0)))
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
            let update = store.apc_end(Point::new(Line(0), Column(0)));

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

        // Mediums that would read from the filesystem are refused.
        let update = apc(&mut store, "Ga=t,f=32,t=f,s=1,v=1,i=4;/etc/passwd");
        assert_eq!(
            update.response.as_deref(),
            Some("\x1b_Gi=4;EUNSUPPORTED:unsupported command\x1b\\")
        );
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
}
