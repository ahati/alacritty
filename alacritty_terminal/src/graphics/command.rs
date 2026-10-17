//! Parsing of Kitty graphics protocol control data.
//!
//! A graphics command is carried in an APC string as:
//!
//! ```text
//! ESC _ G <control-data> ; <base64-payload> ESC \
//! ```
//!
//! The control data is a comma-separated list of `key=value` pairs. Values are
//! either a single flag character, an unsigned integer, or (for a handful of
//! keys) a signed integer.

use std::fmt;

/// Maximum length of the control data section, in bytes.
///
/// Kitty chunks payloads at 4096 bytes, and the control data is a small handful
/// of `key=value` pairs, so this is already extremely generous. Bounding it
/// keeps a malicious client from making the parser buffer without limit.
pub const MAX_CONTROL_DATA_LEN: usize = 4096;

/// Maximum number of decimal digits accepted for a numeric value.
///
/// Kitty rejects values that cannot fit a 32-bit integer, which caps a decimal
/// value at ten digits.
pub const MAX_VALUE_DIGITS: usize = 10;

/// Number of decimal digit characters (`0` through `9`).
const DIGITS: u8 = 10;

/// Separator between control data entries.
const ENTRY_SEPARATOR: u8 = b',';

/// Separator between a key and its value.
const VALUE_SEPARATOR: u8 = b'=';

// Control data keys, named after the protocol's own single-letter identifiers.
const KEY_ACTION: u8 = b'a';
const KEY_DELETE: u8 = b'd';
const KEY_FORMAT: u8 = b'f';
const KEY_MEDIUM: u8 = b't';
const KEY_COMPRESSION: u8 = b'o';
const KEY_MORE: u8 = b'm';
const KEY_IMAGE_ID: u8 = b'i';
const KEY_IMAGE_NUMBER: u8 = b'I';
const KEY_PLACEMENT_ID: u8 = b'p';
const KEY_QUIET: u8 = b'q';
const KEY_SOURCE_WIDTH: u8 = b's';
const KEY_SOURCE_HEIGHT: u8 = b'v';
const KEY_DATA_SIZE: u8 = b'S';
const KEY_DATA_OFFSET: u8 = b'O';
const KEY_SRC_X: u8 = b'x';
const KEY_SRC_Y: u8 = b'y';
const KEY_SRC_WIDTH: u8 = b'w';
const KEY_SRC_HEIGHT: u8 = b'h';
const KEY_COLUMNS: u8 = b'c';
const KEY_ROWS: u8 = b'r';
const KEY_CELL_OFFSET_X: u8 = b'X';
const KEY_CELL_OFFSET_Y: u8 = b'Y';
const KEY_Z_INDEX: u8 = b'z';
const KEY_CURSOR_MOVEMENT: u8 = b'C';
const KEY_VIRTUAL: u8 = b'U';
const KEY_PARENT_IMAGE: u8 = b'P';
const KEY_PARENT_PLACEMENT: u8 = b'Q';
const KEY_TRANSIENT: u8 = b'N';
const KEY_PARENT_OFFSET_X: u8 = b'H';
const KEY_PARENT_OFFSET_Y: u8 = b'V';

// Numeric key values.
const FORMAT_RGB: u32 = 24;
const FORMAT_RGBA: u32 = 32;
const FORMAT_PNG: u32 = 100;
const QUIET_MAX: u32 = 2;

/// Action requested by the client (`a`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Action {
    /// `t`: store image data without displaying it.
    #[default]
    Transmit,
    /// `T`: store image data and display it.
    TransmitAndDisplay,
    /// `p`: display an already stored image.
    Put,
    /// `d`: delete images or placements.
    Delete,
    /// `q`: validate an image and respond without storing it.
    Query,
    /// `f`: add or edit an animation frame.
    Frame,
    /// `a`: control animation playback.
    Animate,
    /// `c`: compose animation frames.
    Compose,
}

impl Action {
    fn parse(value: u8) -> Result<Self, ParseError> {
        match value {
            b't' => Ok(Self::Transmit),
            b'T' => Ok(Self::TransmitAndDisplay),
            b'p' => Ok(Self::Put),
            b'd' => Ok(Self::Delete),
            b'q' => Ok(Self::Query),
            b'f' => Ok(Self::Frame),
            b'a' => Ok(Self::Animate),
            b'c' => Ok(Self::Compose),
            _ => Err(ParseError::Unsupported),
        }
    }
}

/// Pixel format of the transmitted data (`f`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    /// Three bytes per pixel, no alpha.
    Rgb,
    /// Four bytes per pixel, straight alpha.
    #[default]
    Rgba,
    /// A PNG stream.
    Png,
}

impl Format {
    fn parse(value: u32) -> Result<Self, ParseError> {
        match value {
            FORMAT_RGB => Ok(Self::Rgb),
            FORMAT_RGBA => Ok(Self::Rgba),
            FORMAT_PNG => Ok(Self::Png),
            _ => Err(ParseError::Unsupported),
        }
    }

    /// Number of bytes each pixel occupies, if the format has a fixed one.
    pub const fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            Self::Rgb => Some(3),
            Self::Rgba => Some(4),
            Self::Png => None,
        }
    }
}

/// How the image data reaches the terminal (`t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Medium {
    /// `d`: the payload is sent inline.
    #[default]
    Direct,
    /// `f`: the payload names a file to read.
    File,
    /// `t`: the payload names a temporary file the terminal should delete.
    TempFile,
    /// `s`: the payload names a shared memory object.
    SharedMemory,
}

impl Medium {
    fn parse(value: u8) -> Result<Self, ParseError> {
        match value {
            b'd' => Ok(Self::Direct),
            b'f' => Ok(Self::File),
            b't' => Ok(Self::TempFile),
            b's' => Ok(Self::SharedMemory),
            _ => Err(ParseError::Unsupported),
        }
    }
}

/// Compression applied to the payload before base64 encoding (`o`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// RFC 1950 zlib.
    Zlib,
}

impl Compression {
    fn parse(value: u8) -> Result<Self, ParseError> {
        match value {
            b'z' => Ok(Self::Zlib),
            _ => Err(ParseError::Unsupported),
        }
    }
}

/// Whether displaying an image moves the cursor (`C`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorMovement {
    /// Move the cursor past the placement.
    #[default]
    Move,
    /// Leave the cursor where it was.
    Stay,
}

/// Reason a command's control data could not be understood.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The control data contains a key this implementation does not know.
    UnknownKey(u8),
    /// An entry was not of the form `key=value`.
    Malformed,
    /// A value was empty or not the expected shape.
    InvalidValue,
    /// A numeric value did not fit its type.
    ValueOutOfRange,
    /// The control data exceeded [`MAX_CONTROL_DATA_LEN`].
    TooLong,
    /// The key was understood but the value is not supported.
    Unsupported,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey(key) => write!(f, "unknown key {:?}", *key as char),
            Self::Malformed => f.write_str("malformed key/value pair"),
            Self::InvalidValue => f.write_str("invalid value"),
            Self::ValueOutOfRange => f.write_str("value out of range"),
            Self::TooLong => f.write_str("control data too long"),
            Self::Unsupported => f.write_str("unsupported value"),
        }
    }
}

impl std::error::Error for ParseError {}

/// A parsed Kitty graphics command.
///
/// Unset numeric keys default to `0`, matching the protocol's documented
/// defaults; in particular an image id or number of `0` means "not supplied".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    pub action: Action,
    pub format: Format,
    pub medium: Medium,
    pub compression: Option<Compression>,
    pub quiet: u8,
    pub more: bool,
    pub transient: bool,
    pub image_id: u32,
    pub image_number: u32,
    pub placement_id: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub data_size: u32,
    pub data_offset: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_width: u32,
    pub src_height: u32,
    pub columns: u32,
    pub rows: u32,
    pub cell_offset_x: u32,
    pub cell_offset_y: u32,
    pub z_index: i32,
    pub cursor_movement: CursorMovement,
    pub virtual_placement: bool,
    pub delete: Option<u8>,
    pub parent_image_id: u32,
    pub parent_placement_id: u32,
    pub parent_offset_x: i32,
    pub parent_offset_y: i32,
}

impl Command {
    /// Parse the control data of a graphics command.
    ///
    /// `control` must exclude the leading `G` and the `;` payload separator.
    pub fn parse(control: &[u8]) -> Result<Self, ParseError> {
        if control.len() > MAX_CONTROL_DATA_LEN {
            return Err(ParseError::TooLong);
        }

        let mut command = Self::default();
        let mut rest = control;

        while !rest.is_empty() {
            let (entry, remaining) = split_entry(rest);
            rest = remaining;

            // Tolerate stray separators rather than rejecting the command.
            if entry.is_empty() {
                continue;
            }

            let separator = entry
                .iter()
                .position(|byte| *byte == VALUE_SEPARATOR)
                .ok_or(ParseError::Malformed)?;
            let (key, value) = entry.split_at(separator);
            let value = &value[1..];

            match *key {
                [key] => command.set_key(key, value)?,
                _ => return Err(ParseError::Malformed),
            }
        }

        Ok(command)
    }

    /// Whether the client expects a response.
    ///
    /// Kitty only replies when an image id or number was supplied, so that
    /// anonymous transmissions stay silent.
    pub const fn wants_response(&self) -> bool {
        self.image_id != 0 || self.image_number != 0
    }

    /// Whether the quiet level suppresses success responses.
    pub const fn suppresses_ok(&self) -> bool {
        self.quiet >= 1
    }

    /// Whether the quiet level suppresses every response.
    pub const fn suppresses_all(&self) -> bool {
        self.quiet >= 2
    }

    /// Apply a single `key=value` entry.
    fn set_key(&mut self, key: u8, value: &[u8]) -> Result<(), ParseError> {
        match key {
            KEY_ACTION => self.action = Action::parse(flag(value)?)?,
            KEY_COMPRESSION => self.compression = Some(Compression::parse(flag(value)?)?),
            KEY_DELETE => self.delete = Some(flag(value)?),
            KEY_FORMAT => self.format = Format::parse(unsigned(value)?)?,
            KEY_MEDIUM => self.medium = Medium::parse(flag(value)?)?,
            KEY_MORE => self.more = boolean(value)?,
            KEY_TRANSIENT => self.transient = boolean(value)?,
            KEY_VIRTUAL => self.virtual_placement = boolean(value)?,
            KEY_CURSOR_MOVEMENT => {
                self.cursor_movement =
                    if boolean(value)? { CursorMovement::Stay } else { CursorMovement::Move };
            },
            KEY_IMAGE_ID => self.image_id = unsigned(value)?,
            KEY_IMAGE_NUMBER => self.image_number = unsigned(value)?,
            KEY_PLACEMENT_ID => self.placement_id = unsigned(value)?,
            KEY_SOURCE_WIDTH => self.source_width = unsigned(value)?,
            KEY_SOURCE_HEIGHT => self.source_height = unsigned(value)?,
            KEY_DATA_SIZE => self.data_size = unsigned(value)?,
            KEY_DATA_OFFSET => self.data_offset = unsigned(value)?,
            KEY_SRC_X => self.src_x = unsigned(value)?,
            KEY_SRC_Y => self.src_y = unsigned(value)?,
            KEY_SRC_WIDTH => self.src_width = unsigned(value)?,
            KEY_SRC_HEIGHT => self.src_height = unsigned(value)?,
            KEY_COLUMNS => self.columns = unsigned(value)?,
            KEY_ROWS => self.rows = unsigned(value)?,
            KEY_CELL_OFFSET_X => self.cell_offset_x = unsigned(value)?,
            KEY_CELL_OFFSET_Y => self.cell_offset_y = unsigned(value)?,
            KEY_Z_INDEX => self.z_index = signed(value)?,
            KEY_PARENT_IMAGE => self.parent_image_id = unsigned(value)?,
            KEY_PARENT_PLACEMENT => self.parent_placement_id = unsigned(value)?,
            KEY_PARENT_OFFSET_X => self.parent_offset_x = signed(value)?,
            KEY_PARENT_OFFSET_Y => self.parent_offset_y = signed(value)?,
            KEY_QUIET => {
                let quiet = unsigned(value)?;
                if quiet > QUIET_MAX {
                    return Err(ParseError::Unsupported);
                }
                self.quiet = quiet as u8;
            },
            _ => return Err(ParseError::UnknownKey(key)),
        }

        Ok(())
    }
}

/// Split the first `key=value` entry off `input`.
fn split_entry(input: &[u8]) -> (&[u8], &[u8]) {
    match input.iter().position(|byte| *byte == ENTRY_SEPARATOR) {
        Some(index) => (&input[..index], &input[index + 1..]),
        None => (input, &[]),
    }
}

/// Read a value that must be exactly one character.
fn flag(value: &[u8]) -> Result<u8, ParseError> {
    match *value {
        [flag] => Ok(flag),
        _ => Err(ParseError::InvalidValue),
    }
}

/// Read a value that must be `0` or `1`.
fn boolean(value: &[u8]) -> Result<bool, ParseError> {
    match unsigned(value)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ParseError::Unsupported),
    }
}

/// Read an unsigned 32-bit value.
fn unsigned(value: &[u8]) -> Result<u32, ParseError> {
    let magnitude = magnitude(value)?;
    u32::try_from(magnitude).map_err(|_| ParseError::ValueOutOfRange)
}

/// Read a signed 32-bit value.
fn signed(value: &[u8]) -> Result<i32, ParseError> {
    let (negative, magnitude) = match value.first() {
        Some(b'-') => (true, magnitude(&value[1..])?),
        _ => (false, magnitude(value)?),
    };

    let signed = if negative { -(magnitude as i64) } else { magnitude as i64 };
    i32::try_from(signed).map_err(|_| ParseError::ValueOutOfRange)
}

/// Read the decimal magnitude of a numeric value.
fn magnitude(value: &[u8]) -> Result<u64, ParseError> {
    if value.is_empty() {
        return Err(ParseError::InvalidValue);
    }
    if value.len() > MAX_VALUE_DIGITS {
        return Err(ParseError::ValueOutOfRange);
    }

    value.iter().try_fold(0u64, |accumulator, byte| {
        let digit = byte
            .checked_sub(b'0')
            .filter(|digit| *digit < DIGITS)
            .ok_or(ParseError::InvalidValue)?;
        accumulator
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit)))
            .ok_or(ParseError::ValueOutOfRange)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let command = Command::parse(b"").unwrap();
        assert_eq!(command.action, Action::Transmit);
        assert_eq!(command.format, Format::Rgba);
        assert_eq!(command.medium, Medium::Direct);
        assert_eq!(command.quiet, 0);
        assert!(!command.more);
        assert!(!command.wants_response());
    }

    #[test]
    fn parses_transmit_and_display_png() {
        let command = Command::parse(b"a=T,f=100,i=31").unwrap();
        assert_eq!(command.action, Action::TransmitAndDisplay);
        assert_eq!(command.format, Format::Png);
        assert_eq!(command.image_id, 31);
        assert!(command.wants_response());
    }

    #[test]
    fn parses_signed_z_index() {
        assert_eq!(Command::parse(b"z=-5").unwrap().z_index, -5);
        assert_eq!(Command::parse(b"z=7").unwrap().z_index, 7);
    }

    #[test]
    fn parses_flags_and_booleans() {
        let command = Command::parse(b"t=d,o=z,m=1,q=2,C=1,U=1,N=1").unwrap();
        assert_eq!(command.medium, Medium::Direct);
        assert_eq!(command.compression, Some(Compression::Zlib));
        assert!(command.more);
        assert_eq!(command.quiet, 2);
        assert_eq!(command.cursor_movement, CursorMovement::Stay);
        assert!(command.virtual_placement);
        assert!(command.transient);
    }

    #[test]
    fn parses_delete_selector_and_placement() {
        let command = Command::parse(b"a=d,d=i,i=5,p=3").unwrap();
        assert_eq!(command.action, Action::Delete);
        assert_eq!(command.delete, Some(b'i'));
        assert_eq!(command.image_id, 5);
        assert_eq!(command.placement_id, 3);
    }

    #[test]
    fn tolerates_stray_separators() {
        assert_eq!(Command::parse(b",i=1,").unwrap().image_id, 1);
    }

    #[test]
    fn rejects_unknown_key() {
        assert_eq!(Command::parse(b"Z=1"), Err(ParseError::UnknownKey(b'Z')));
    }

    #[test]
    fn rejects_malformed_and_invalid_values() {
        assert_eq!(Command::parse(b"i"), Err(ParseError::Malformed));
        assert_eq!(Command::parse(b"i="), Err(ParseError::InvalidValue));
        assert_eq!(Command::parse(b"ii=1"), Err(ParseError::Malformed));
        assert_eq!(Command::parse(b"f=x"), Err(ParseError::InvalidValue));
        assert_eq!(Command::parse(b"o=x"), Err(ParseError::Unsupported));
    }

    #[test]
    fn rejects_unsupported_and_out_of_range() {
        assert_eq!(Command::parse(b"f=1"), Err(ParseError::Unsupported));
        assert_eq!(Command::parse(b"a=Z"), Err(ParseError::Unsupported));
        assert_eq!(Command::parse(b"q=3"), Err(ParseError::Unsupported));
        assert_eq!(Command::parse(b"i=4294967296"), Err(ParseError::ValueOutOfRange));
        assert_eq!(Command::parse(b"z=2147483648"), Err(ParseError::ValueOutOfRange));
        assert_eq!(Command::parse(b"i=12345678901"), Err(ParseError::ValueOutOfRange));
    }

    #[test]
    fn rejects_oversized_control_data() {
        let control = vec![b' '; MAX_CONTROL_DATA_LEN + 1];
        assert_eq!(Command::parse(&control), Err(ParseError::TooLong));
    }

    #[test]
    fn quiet_levels() {
        assert!(!Command::parse(b"i=1").unwrap().suppresses_ok());
        assert!(Command::parse(b"i=1,q=1").unwrap().suppresses_ok());
        assert!(Command::parse(b"i=1,q=2").unwrap().suppresses_all());
    }
}
