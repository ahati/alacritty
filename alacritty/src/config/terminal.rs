use serde::{Deserialize, Deserializer, Serialize, de};
use toml::Value;

use alacritty_config_derive::{ConfigDeserialize, SerdeReplace};
use alacritty_terminal::clipboard::{
    DEFAULT_MAX_WRITE_SIZE, KittyClipboardConfig, Permission as TerminalPermission,
};
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
    /// Kitty clipboard protocol support.
    pub kitty_clipboard: KittyClipboard,
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

    /// Whether a file transmission may name a path that leaves this machine.
    ///
    /// On Windows a share names the host to read from, so a program in the
    /// terminal could make it contact, and authenticate to, a machine of its
    /// choosing. Disabling this refuses such a path before it is resolved, and
    /// refuses a path that resolves to a share too. A junction or a folder
    /// redirect to a share can still reach one while the path is resolved, and
    /// a mapped drive cannot be told from a local one, so neither is prevented.
    ///
    /// Other platforms mount network filesystems as ordinary directories, where
    /// the setting has no effect.
    pub allow_network_paths: bool,
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            enabled: true,
            max_image_dimension: DEFAULT_MAX_IMAGE_DIMENSION,
            memory_limit: DEFAULT_MEMORY_LIMIT,
            allow_network_paths: true,
        }
    }
}

impl From<&Graphics> for GraphicsConfig {
    fn from(graphics: &Graphics) -> Self {
        Self {
            enabled: graphics.enabled,
            max_image_dimension: graphics.max_image_dimension,
            memory_limit: graphics.memory_limit,
            allow_network_paths: graphics.allow_network_paths,
        }
    }
}

/// Whether terminal programs may use a clipboard operation.
#[derive(ConfigDeserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    /// The operation is refused.
    #[default]
    Deny,
    /// The operation is carried out.
    Allow,
}

/// Kitty clipboard protocol settings.
#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct KittyClipboard {
    /// Whether terminal programs may read clipboard content through OSC 5522.
    pub read: Permission,
    /// Whether terminal programs may write clipboard content through OSC 5522.
    pub write: Permission,
    /// The largest write accepted before it is refused with `EFBIG`.
    pub max_write_size: usize,
}

impl Default for KittyClipboard {
    fn default() -> Self {
        Self {
            // The same compromise as `osc52 = "OnlyCopy"`: a program can
            // only hand over what it already produced, while reading would
            // give clipboard content to a program that did not have it.
            read: Permission::Deny,
            write: Permission::Allow,
            max_write_size: DEFAULT_MAX_WRITE_SIZE,
        }
    }
}

impl From<&KittyClipboard> for KittyClipboardConfig {
    fn from(kitty_clipboard: &KittyClipboard) -> Self {
        Self {
            read: match kitty_clipboard.read {
                Permission::Deny => TerminalPermission::Deny,
                Permission::Allow => TerminalPermission::Allow,
            },
            write: match kitty_clipboard.write {
                Permission::Deny => TerminalPermission::Deny,
                Permission::Allow => TerminalPermission::Allow,
            },
            max_write_size: kitty_clipboard.max_write_size,
        }
    }
}
