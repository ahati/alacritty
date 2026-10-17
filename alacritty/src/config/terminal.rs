use serde::{Deserialize, Deserializer, Serialize, de};
use toml::Value;

use alacritty_config_derive::{ConfigDeserialize, SerdeReplace};
use alacritty_terminal::graphics::{
    DEFAULT_MAX_IMAGE_DIMENSION, DEFAULT_MEMORY_LIMIT, GraphicsConfig,
};
use alacritty_terminal::term::Osc52;

use crate::config::ui_config::{Program, StringVisitor};

#[derive(ConfigDeserialize, Serialize, Default, Clone, Debug, PartialEq)]
pub struct Terminal {
    /// OSC52 support mode.
    pub osc52: SerdeOsc52,
    /// Kitty graphics protocol support.
    pub graphics: Graphics,
    /// Path to a shell program to run on startup.
    pub shell: Option<Program>,
}

#[derive(SerdeReplace, Serialize, Default, Copy, Clone, Debug, PartialEq)]
pub struct SerdeOsc52(pub Osc52);

impl<'de> Deserialize<'de> for SerdeOsc52 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserializer.deserialize_str(StringVisitor)?;
        Osc52::deserialize(Value::String(value)).map(SerdeOsc52).map_err(de::Error::custom)
    }
}

/// Kitty graphics protocol settings.
#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Graphics {
    /// Whether inline images are handled at all.
    pub enabled: bool,

    /// Maximum width or height of an image, in pixels.
    pub max_image_dimension: u32,

    /// Maximum bytes of decoded image data kept in memory.
    ///
    /// Images that no placement refers to are dropped first when the limit is
    /// reached.
    pub memory_limit: usize,
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            enabled: true,
            max_image_dimension: DEFAULT_MAX_IMAGE_DIMENSION,
            memory_limit: DEFAULT_MEMORY_LIMIT,
        }
    }
}

impl From<&Graphics> for GraphicsConfig {
    fn from(graphics: &Graphics) -> Self {
        Self {
            enabled: graphics.enabled,
            max_image_dimension: graphics.max_image_dimension,
            memory_limit: graphics.memory_limit,
        }
    }
}
