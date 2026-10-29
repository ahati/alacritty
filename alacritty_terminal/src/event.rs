use std::borrow::Cow;
use std::fmt::{self, Debug, Formatter};
use std::process::ExitStatus;
use std::sync::Arc;

use crate::clipboard::StoreOutcome;
use crate::term::ClipboardType;
use crate::vte::ansi::Rgb;

/// The formatter a kitty clipboard read hands to the front end, turning the
/// clipboard text it reads — or the absence of the requested location — into
/// the PTY reply.
pub type KittyReadFormat = Arc<dyn for<'a> Fn(Option<&'a str>) -> String + Sync + Send + 'static>;

/// The formatter turning a kitty clipboard store's outcome into the reply.
pub type KittyStoreFormat = Arc<dyn Fn(StoreOutcome) -> String + Sync + Send + 'static>;

/// Terminal event.
///
/// These events instruct the UI over changes that can't be handled by the terminal emulation layer
/// itself.
#[derive(Clone)]
pub enum Event {
    /// Grid has changed possibly requiring a mouse cursor shape change.
    MouseCursorDirty,

    /// Window title change.
    Title(String),

    /// Reset to the default window title.
    ResetTitle,

    /// Request to store a text string in the clipboard.
    ClipboardStore(ClipboardType, String),

    /// Request to write the contents of the clipboard to the PTY.
    ///
    /// The attached function is a formatter which will correctly transform the clipboard content
    /// into the expected escape sequence format.
    ClipboardLoad(ClipboardType, Arc<dyn Fn(&str) -> String + Sync + Send + 'static>),

    /// Request to read the clipboard for the kitty clipboard protocol (OSC 5522).
    ///
    /// The attached function formats the whole reply; it receives `None` when the requested
    /// clipboard location does not exist on this platform, such as a primary selection on
    /// Windows or macOS, and must then produce the `ENOSYS` reply.
    KittyClipboardRead(ClipboardType, KittyReadFormat),

    /// Request to store clipboard text for the kitty clipboard protocol (OSC 5522).
    ///
    /// The attached function turns the outcome of the store into the reply packet.
    KittyClipboardStore(ClipboardType, String, KittyStoreFormat),

    /// Request to write the RGB value of a color to the PTY.
    ///
    /// The attached function is a formatter which will correctly transform the RGB color into the
    /// expected escape sequence format.
    ColorRequest(usize, Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>),

    /// Write some text to the PTY.
    PtyWrite(String),

    /// Request to write the text area size.
    TextAreaSizeRequest(Arc<dyn Fn(WindowSize) -> String + Sync + Send + 'static>),

    /// Cursor blinking state has changed.
    CursorBlinkingChange,

    /// New terminal content available.
    Wakeup,

    /// Terminal bell ring.
    Bell,

    /// Shutdown request.
    Exit,

    /// Child process exited.
    ChildExit(ExitStatus),
}

impl Debug for Event {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Event::ClipboardStore(ty, text) => write!(f, "ClipboardStore({ty:?}, {text})"),
            Event::ClipboardLoad(ty, _) => write!(f, "ClipboardLoad({ty:?})"),
            Event::KittyClipboardRead(ty, _) => write!(f, "KittyClipboardRead({ty:?})"),
            Event::KittyClipboardStore(ty, _, _) => write!(f, "KittyClipboardStore({ty:?})"),
            Event::TextAreaSizeRequest(_) => write!(f, "TextAreaSizeRequest"),
            Event::ColorRequest(index, _) => write!(f, "ColorRequest({index})"),
            Event::PtyWrite(text) => write!(f, "PtyWrite({text})"),
            Event::Title(title) => write!(f, "Title({title})"),
            Event::CursorBlinkingChange => write!(f, "CursorBlinkingChange"),
            Event::MouseCursorDirty => write!(f, "MouseCursorDirty"),
            Event::ResetTitle => write!(f, "ResetTitle"),
            Event::Wakeup => write!(f, "Wakeup"),
            Event::Bell => write!(f, "Bell"),
            Event::Exit => write!(f, "Exit"),
            Event::ChildExit(status) => write!(f, "ChildExit({status:?})"),
        }
    }
}

/// Byte sequences are sent to a `Notify` in response to some events.
pub trait Notify {
    /// Notify that an escape sequence should be written to the PTY.
    ///
    /// TODO this needs to be able to error somehow.
    fn notify<B: Into<Cow<'static, [u8]>>>(&self, _: B);
}

#[derive(Copy, Clone, Debug)]
pub struct WindowSize {
    pub num_lines: u16,
    pub num_cols: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// Types that are interested in when the display is resized.
pub trait OnResize {
    fn on_resize(&mut self, window_size: WindowSize);
}

/// Event Loop for notifying the renderer about terminal events.
pub trait EventListener {
    fn send_event(&self, _event: Event) {}
}

/// Null sink for events.
pub struct VoidListener;

impl EventListener for VoidListener {}
