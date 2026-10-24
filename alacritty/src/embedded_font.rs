//! Registration of the default font embedded into the binary.
//!
//! The font is fetched at build time from the latest JetBrains Mono Nerd Font
//! Mono release (see `build.rs`) and registered with the rasterizer entirely
//! in-memory: it works without being installed on the system, without touching
//! the disk and without registering with the OS font stack. Glyphs missing
//! from the embedded font fall back to system fonts.

/// The platform's default `font.normal.family`.
///
/// On Linux this is the embedded font, when it was fetched at build time;
/// macOS and Windows keep their stock defaults until their rasterizer backends
/// gain in-memory support.
pub fn default_family() -> String {
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        if cfg!(embed_fonts) {
            String::from("JetBrainsMono Nerd Font Mono")
        } else {
            String::from("monospace")
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    {
        if cfg!(target_os = "macos") { String::from("Menlo") } else { String::from("Consolas") }
    }
}

/// Register the embedded font styles with the rasterizer.
///
/// This is a no-op unless the font was fetched at build time; on macOS and
/// Windows it is always a no-op for now.
pub fn register(rasterizer: &mut Rasterizer) {
    #[cfg(all(embed_fonts, not(any(target_os = "macos", windows))))]
    {
        use std::rc::Rc;

        use crossfont::ft::MemoryFontRegistration;
        use crossfont::{Slant, Weight};

        let styles = [
            (Slant::Normal, Weight::Normal, "Regular"),
            (Slant::Normal, Weight::Bold, "Bold"),
            (Slant::Italic, Weight::Normal, "Italic"),
            (Slant::Italic, Weight::Bold, "BoldItalic"),
        ];

        for (slant, weight, style) in styles {
            let data: &[u8] = match style {
                "Regular" => include_bytes!(env!("EMBEDDED_FONT_REGULAR")),
                "Bold" => include_bytes!(env!("EMBEDDED_FONT_BOLD")),
                "Italic" => include_bytes!(env!("EMBEDDED_FONT_ITALIC")),
                "BoldItalic" => include_bytes!(env!("EMBEDDED_FONT_BOLD_ITALIC")),
                _ => unreachable!(),
            };

            let font_key = rasterizer.register_memory_font(MemoryFontRegistration {
                family: String::from("JetBrainsMono Nerd Font Mono"),
                slant,
                weight,
                data: Rc::new(data.to_vec()),
            });
            debug!("Registered embedded font style {style} with key {font_key:?}");
        }
    }

    #[cfg(any(not(embed_fonts), target_os = "macos", target_os = "windows"))]
    {
        let _ = rasterizer;
    }
}

use crossfont::Rasterizer;

#[cfg(all(embed_fonts, not(any(target_os = "macos", windows))))]
use log::debug;
