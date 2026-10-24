use std::collections::hash_map::Entry;
use std::collections::HashMap;

use ahash::RandomState;
use crossfont::{
    Error as RasterizerError, FontDesc, FontKey, GlyphKey, Metrics, Rasterize, RasterizedGlyph,
    Rasterizer, Size, Slant, Style, Weight,
};
use harfrust::{Feature, Tag};
use log::{error, info, trace, warn};
use unicode_width::UnicodeWidthChar;

use crate::config::font::{Font, FontDescription};
use crate::config::ui_config::Delta;
use crate::gl::types::*;

use super::builtin_font;

/// `LoadGlyph` allows for copying a rasterized glyph into graphics memory.
pub trait LoadGlyph {
    /// Load the rasterized glyph into GPU memory.
    fn load_glyph(&mut self, rasterized: &RasterizedGlyph) -> Glyph;

    /// Clear any state accumulated from previous loaded glyphs.
    ///
    /// This can, for instance, be used to reset the texture Atlas.
    fn clear(&mut self);
}

#[derive(Copy, Clone, Debug)]
pub struct Glyph {
    pub tex_id: GLuint,
    pub multicolor: bool,
    pub top: i16,
    pub left: i16,
    pub width: i16,
    pub height: i16,
    pub uv_bot: f32,
    pub uv_left: f32,
    pub uv_width: f32,
    pub uv_height: f32,
}

/// A glyph as produced by shaping a text run.
#[derive(Copy, Clone, Debug)]
pub enum ShapedGlyphSpec {
    /// A raw glyph index within the shaped font.
    GlyphIndex(u32),

    /// A character, used when the shaped glyph is unavailable (like the
    /// `.notdef` glyph) so that fallbacks apply as usual.
    Char(char),
}

/// A shaped glyph and its cluster within the shaped text.
#[derive(Copy, Clone, Debug)]
pub struct ShapedGlyph {
    pub glyph: Glyph,

    /// UTF-8 byte offset of the cluster this glyph belongs to.
    pub cluster: usize,
}

/// The maximum number of shaped runs kept around.
const SHAPED_RUNS_CAP: usize = 10_000;

/// Naïve glyph cache.
///
/// The cache is keyed by [`GlyphKey`], which uniquely identifies a glyph
/// within a font at a specific size.
pub struct GlyphCache {
    /// Cache of buffered glyphs.
    cache: HashMap<GlyphKey, Glyph, RandomState>,

    /// Rasterizer for loading new glyphs.
    rasterizer: Rasterizer,

    /// Regular font.
    pub font_key: FontKey,

    /// Bold font.
    pub bold_key: FontKey,

    /// Italic font.
    pub italic_key: FontKey,

    /// Bold italic font.
    pub bold_italic_key: FontKey,

    /// Font size.
    pub font_size: crossfont::Size,

    /// Font offset.
    font_offset: Delta<i8>,

    /// Glyph offset.
    glyph_offset: Delta<i8>,

    /// Font metrics.
    metrics: Metrics,

    /// Whether to use the built-in font for box drawing characters.
    builtin_box_drawing: bool,

    /// Whether font ligatures should be shaped.
    ligatures: bool,

    /// OpenType features used during shaping.
    shape_features: Vec<Feature>,

    /// Fonts loaded for shaping, keyed by [`FontKey`].
    ///
    /// Loading can fail, for instance when the font file is inaccessible, in
    /// which case runs are shaped char-by-char instead.
    shaped_fonts: HashMap<FontKey, Option<harfrust::Font>>,

    /// Cache of shaped runs, keyed by font and text.
    shaped_runs: HashMap<(FontKey, String), Vec<(ShapedGlyphSpec, usize)>>,

    /// Cache of glyphs rasterized by their index, keyed by font, size and
    /// glyph index.
    shaped_cache: HashMap<(FontKey, crossfont::Size, u32), Glyph>,

    /// Reusable buffer for shaping.
    shape_buffer: harfrust::Buffer,
}

impl GlyphCache {
    pub fn new(mut rasterizer: Rasterizer, font: &Font) -> Result<GlyphCache, crossfont::Error> {
        let (regular, bold, italic, bold_italic) = Self::compute_font_keys(font, &mut rasterizer)?;

        let metrics = GlyphCache::load_font_metrics(&mut rasterizer, font, regular)?;
        Ok(Self {
            cache: Default::default(),
            rasterizer,
            font_size: font.size(),
            font_key: regular,
            bold_key: bold,
            italic_key: italic,
            bold_italic_key: bold_italic,
            font_offset: font.offset,
            glyph_offset: font.glyph_offset,
            metrics,
            builtin_box_drawing: font.builtin_box_drawing,
            ligatures: font.ligatures,
            shape_features: Self::shape_features(font.ligatures),
            shaped_fonts: Default::default(),
            shaped_runs: Default::default(),
            shaped_cache: Default::default(),
            shape_buffer: Default::default(),
        })
    }

    // Load font metrics and adjust for glyph offset.
    fn load_font_metrics(
        rasterizer: &mut Rasterizer,
        font: &Font,
        key: FontKey,
    ) -> Result<Metrics, crossfont::Error> {
        // Need to load at least one glyph for the face before calling metrics.
        // The glyph requested here ('m' at the time of writing) has no special
        // meaning.
        rasterizer.get_glyph(GlyphKey { font_key: key, character: 'm', size: font.size() })?;

        let mut metrics = rasterizer.metrics(key, font.size())?;
        metrics.strikeout_position += font.glyph_offset.y as f32;
        Ok(metrics)
    }

    fn load_glyphs_for_font<L: LoadGlyph>(&mut self, font: FontKey, loader: &mut L) {
        let size = self.font_size;

        // Cache all ascii characters.
        for i in 32u8..=126u8 {
            self.get(GlyphKey { font_key: font, character: i as char, size }, loader, true);
        }
    }

    /// Computes font keys for (Regular, Bold, Italic, Bold Italic).
    fn compute_font_keys(
        font: &Font,
        rasterizer: &mut Rasterizer,
    ) -> Result<(FontKey, FontKey, FontKey, FontKey), crossfont::Error> {
        let size = font.size();

        // Load regular font.
        let regular_desc = Self::make_desc(font.normal(), Slant::Normal, Weight::Normal);

        let regular = Self::load_regular_font(rasterizer, &regular_desc, size)?;

        // Helper to load a description if it is not the `regular_desc`.
        let mut load_or_regular = |desc: FontDesc| {
            if desc == regular_desc {
                regular
            } else {
                rasterizer.load_font(&desc, size).unwrap_or(regular)
            }
        };

        // Load bold font.
        let bold_desc = Self::make_desc(&font.bold(), Slant::Normal, Weight::Bold);

        let bold = load_or_regular(bold_desc);

        // Load italic font.
        let italic_desc = Self::make_desc(&font.italic(), Slant::Italic, Weight::Normal);

        let italic = load_or_regular(italic_desc);

        // Load bold italic font.
        let bold_italic_desc = Self::make_desc(&font.bold_italic(), Slant::Italic, Weight::Bold);

        let bold_italic = load_or_regular(bold_italic_desc);

        Ok((regular, bold, italic, bold_italic))
    }

    fn load_regular_font(
        rasterizer: &mut Rasterizer,
        description: &FontDesc,
        size: Size,
    ) -> Result<FontKey, crossfont::Error> {
        match rasterizer.load_font(description, size) {
            Ok(font) => Ok(font),
            Err(err) => {
                error!("{err}");

                let fallback_desc =
                    Self::make_desc(Font::default().normal(), Slant::Normal, Weight::Normal);
                rasterizer.load_font(&fallback_desc, size)
            },
        }
    }

    fn make_desc(desc: &FontDescription, slant: Slant, weight: Weight) -> FontDesc {
        let style = if let Some(ref spec) = desc.style {
            Style::Specific(spec.to_owned())
        } else {
            Style::Description { slant, weight }
        };
        FontDesc::new(desc.family.clone(), style)
    }

    /// Get a glyph from the font.
    ///
    /// If the glyph has never been loaded before, it will be rasterized and inserted into the
    /// cache.
    ///
    /// # Errors
    ///
    /// This will fail when the glyph could not be rasterized. Usually this is due to the glyph
    /// not being present in any font.
    pub fn get<L>(&mut self, glyph_key: GlyphKey, loader: &mut L, show_missing: bool) -> Glyph
    where
        L: LoadGlyph + ?Sized,
    {
        // Try to load glyph from cache.
        if let Some(glyph) = self.cache.get(&glyph_key) {
            return *glyph;
        };

        // Rasterize the glyph using the built-in font for special characters or the user's font
        // for everything else.
        let rasterized = self
            .builtin_box_drawing
            .then(|| {
                builtin_font::builtin_glyph(
                    glyph_key.character,
                    &self.metrics,
                    &self.font_offset,
                    &self.glyph_offset,
                )
            })
            .flatten()
            .map_or_else(|| self.rasterizer.get_glyph(glyph_key), Ok);

        let glyph = match rasterized {
            Ok(rasterized) => self.load_glyph(loader, rasterized),
            // Load fallback glyph.
            Err(RasterizerError::MissingGlyph(rasterized)) if show_missing => {
                // Use `\0` as "missing" glyph to cache it only once.
                let missing_key = GlyphKey { character: '\0', ..glyph_key };
                if let Some(glyph) = self.cache.get(&missing_key) {
                    *glyph
                } else {
                    // If no missing glyph was loaded yet, insert it as `\0`.
                    let glyph = self.load_glyph(loader, rasterized);
                    self.cache.insert(missing_key, glyph);

                    glyph
                }
            },
            Err(_) => self.load_glyph(loader, Default::default()),
        };

        // Cache rasterized glyph.
        *self.cache.entry(glyph_key).or_insert(glyph)
    }

    /// Load glyph into the atlas.
    ///
    /// This will apply all transforms defined for the glyph cache to the rasterized glyph before
    pub fn load_glyph<L>(&self, loader: &mut L, mut glyph: RasterizedGlyph) -> Glyph
    where
        L: LoadGlyph + ?Sized,
    {
        glyph.left += i32::from(self.glyph_offset.x);
        glyph.top += i32::from(self.glyph_offset.y);
        glyph.top -= self.metrics.descent as i32;

        // The metrics of zero-width characters are based on rendering
        // the character after the current cell, with the anchor at the
        // right side of the preceding character. Since we render the
        // zero-width characters inside the preceding character, the
        // anchor has been moved to the right by one cell.
        if glyph.character.width() == Some(0) {
            glyph.left += self.metrics.average_advance as i32;
        }

        // Add glyph to cache.
        loader.load_glyph(&glyph)
    }

    /// Reset currently cached data in both GL and the registry to default state.
    pub fn reset_glyph_cache<L: LoadGlyph>(&mut self, loader: &mut L) {
        loader.clear();
        self.cache = Default::default();
        self.shaped_cache = Default::default();

        self.load_common_glyphs(loader);
    }

    /// Update the inner font size.
    ///
    /// NOTE: To reload the renderers's fonts [`Self::reset_glyph_cache`] should be called
    /// afterwards.
    pub fn update_font_size(&mut self, font: &Font) -> Result<(), crossfont::Error> {
        // Update dpi scaling.
        self.font_offset = font.offset;
        self.glyph_offset = font.glyph_offset;

        // Recompute font keys.
        let (regular, bold, italic, bold_italic) =
            Self::compute_font_keys(font, &mut self.rasterizer)?;

        let metrics = GlyphCache::load_font_metrics(&mut self.rasterizer, font, regular)?;

        info!("Font size changed to {:?} px", font.size().as_px());

        self.font_size = font.size();
        self.font_key = regular;
        self.bold_key = bold;
        self.italic_key = italic;
        self.bold_italic_key = bold_italic;
        self.metrics = metrics;
        self.builtin_box_drawing = font.builtin_box_drawing;
        self.ligatures = font.ligatures;
        self.shape_features = Self::shape_features(font.ligatures);

        // Shaped runs are only valid for a single font size.
        self.shaped_runs.clear();
        self.shaped_cache.clear();

        Ok(())
    }

    pub fn font_metrics(&self) -> crossfont::Metrics {
        self.metrics
    }

    /// Prefetch glyphs that are almost guaranteed to be loaded anyways.
    pub fn load_common_glyphs<L: LoadGlyph>(&mut self, loader: &mut L) {
        self.load_glyphs_for_font(self.font_key, loader);
        self.load_glyphs_for_font(self.bold_key, loader);
        self.load_glyphs_for_font(self.italic_key, loader);
        self.load_glyphs_for_font(self.bold_italic_key, loader);
    }

    /// Shape `text` in `font_key` and load all of its glyphs.
    ///
    /// The returned glyphs are ordered like the characters of the shaped text,
    /// with their cluster identifying the position of the character they were
    /// derived from.
    pub fn shape_run<L: LoadGlyph + ?Sized>(
        &mut self,
        text: &str,
        font_key: FontKey,
        loader: &mut L,
    ) -> Vec<ShapedGlyph> {
        self.shape_text(text, font_key)
            .into_iter()
            .map(|(spec, cluster)| {
                let glyph = match spec {
                    ShapedGlyphSpec::GlyphIndex(index) => self.get_shaped(index, font_key, loader),
                    ShapedGlyphSpec::Char(c) => {
                        let glyph_key = GlyphKey { font_key, size: self.font_size, character: c };
                        self.get(glyph_key, loader, true)
                    },
                };

                ShapedGlyph { cluster, glyph }
            })
            .collect()
    }

    /// Get a glyph rasterized by its index within `font_key`.
    ///
    /// Shaped glyphs (like ligatures) are identified by their index in the
    /// font instead of a character, so they are cached separately.
    fn get_shaped<L: LoadGlyph + ?Sized>(
        &mut self,
        glyph_index: u32,
        font_key: FontKey,
        loader: &mut L,
    ) -> Glyph {
        let cache_key = (font_key, self.font_size, glyph_index);

        // Try to load glyph from cache.
        if let Some(glyph) = self.shaped_cache.get(&cache_key) {
            return *glyph;
        }

        let glyph = match self.rasterizer.get_glyph_by_index(font_key, self.font_size, glyph_index)
        {
            Ok(rasterized) => self.load_glyph(loader, rasterized),
            // Load the default glyph on error.
            Err(_) => self.load_glyph(loader, Default::default()),
        };

        *self.shaped_cache.entry(cache_key).or_insert(glyph)
    }

    /// Shape `text`, using a cache for runs shaped before.
    fn shape_text(&mut self, text: &str, font_key: FontKey) -> Vec<(ShapedGlyphSpec, usize)> {
        // Without ligatures there is no need for shaping at all.
        if !self.ligatures {
            return Self::shape_char_by_char(text);
        }

        // Since shaping is per-frame while a line is visible, reuse previous
        // results for identical runs.
        let key = (font_key, text.to_owned());
        if let Some(shaped) = self.shaped_runs.get(&key) {
            return shaped.clone();
        }

        if self.shaped_runs.len() >= SHAPED_RUNS_CAP {
            self.shaped_runs.clear();
        }

        let shaped = self.shape_with_harfrust(text, font_key);
        self.shaped_runs.insert(key, shaped.clone());

        shaped
    }

    /// Shape `text` with harfrust.
    fn shape_with_harfrust(
        &mut self,
        text: &str,
        font_key: FontKey,
    ) -> Vec<(ShapedGlyphSpec, usize)> {
        // Load the font for shaping, if possible.
        if let Entry::Vacant(entry) = self.shaped_fonts.entry(font_key) {
            let font = Self::load_shaped_font(&mut self.rasterizer, font_key);
            if font.is_none() {
                trace!("Unable to load font {:?} for shaping", font_key);
            }
            entry.insert(font);
        }

        // Clone the font to avoid borrowing the cache while shaping.
        let Some(font) = self.shaped_fonts.get(&font_key).and_then(Option::as_ref).cloned() else {
            return Self::shape_char_by_char(text);
        };

        let shaper = harfrust::ShaperFont::new(&font);

        let buffer = &mut self.shape_buffer;
        buffer.reset();
        buffer.push_str(text);

        // Infer the script of the text, but always shape in logical order to
        // match the terminal's grid.
        buffer.guess_segment_properties();
        buffer.set_direction(harfrust::Direction::LeftToRight);

        let features = self.shape_features.clone();
        let options = harfrust::ShapeOptions::new().features(&features);
        if let Err(err) = harfrust::shape(&shaper, buffer, options) {
            warn!("Shaping failed: {err}");
            return Self::shape_char_by_char(text);
        }

        buffer
            .glyph_infos()
            .iter()
            .map(|info| {
                let cluster = info.cluster as usize;

                // Glyph `0` is the `.notdef` glyph; rasterize the cluster's
                // character instead so that the usual fallbacks apply.
                let spec = if info.glyph_id == 0 {
                    let c = text[cluster..].chars().next().unwrap_or(' ');
                    ShapedGlyphSpec::Char(c)
                } else {
                    ShapedGlyphSpec::GlyphIndex(info.glyph_id)
                };

                (spec, cluster)
            })
            .collect()
    }

    /// Load the font file of `font_key` for shaping.
    fn load_shaped_font(rasterizer: &mut Rasterizer, font_key: FontKey) -> Option<harfrust::Font> {
        // Memory-registered fonts are shaped straight from their bytes.
        if let Some(data) = rasterizer.memory_font_data(font_key) {
            return harfrust::Font::new((*data).clone(), 0);
        }

        let (path, face_index) = rasterizer.font_path(font_key).ok()?;
        let data = std::fs::read(path).ok()?;

        harfrust::Font::new(data, face_index)
    }

    /// "Shape" `text` by mapping every character to its own glyph.
    fn shape_char_by_char(text: &str) -> Vec<(ShapedGlyphSpec, usize)> {
        text.char_indices().map(|(offset, c)| (ShapedGlyphSpec::Char(c), offset)).collect()
    }

    /// The OpenType features used during shaping.
    fn shape_features(ligatures: bool) -> Vec<Feature> {
        if ligatures {
            vec![Feature::new(Tag::new(b"liga"), 1, ..), Feature::new(Tag::new(b"calt"), 1, ..)]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use crossfont::{Rasterize, Rasterizer};

    use super::{Glyph, GlyphCache, LoadGlyph, RasterizedGlyph};
    use crate::config::font::Font;

    struct TestLoader;

    impl LoadGlyph for TestLoader {
        fn load_glyph(&mut self, _: &RasterizedGlyph) -> Glyph {
            Glyph {
                tex_id: 0,
                multicolor: false,
                top: 0,
                left: 0,
                width: 0,
                height: 0,
                uv_bot: 0.,
                uv_left: 0.,
                uv_width: 0.,
                uv_height: 0.,
            }
        }

        fn clear(&mut self) {}
    }

    #[test]
    fn shape_run_maps_clusters() {
        let font = Font::default();
        let rasterizer = Rasterizer::new().unwrap();
        let mut glyph_cache = GlyphCache::new(rasterizer, &font).unwrap();
        let font_key = glyph_cache.font_key;

        let mut loader = TestLoader;
        let shaped = glyph_cache.shape_run("ab", font_key, &mut loader);

        // Without ligatures in the font, every character maps to its own glyph.
        assert_eq!(shaped.len(), 2);
        assert_eq!(shaped[0].cluster, 0);
        assert_eq!(shaped[1].cluster, 1);
    }

    /// The default font resolves to the embedded font, which is shaped from
    /// its registered bytes.
    #[test]
    fn embedded_font_shapes() {
        // Only meaningful when the font was fetched at build time on a
        // platform with in-memory registration.
        if !cfg!(embed_fonts) || cfg!(any(target_os = "macos", target_os = "windows")) {
            return;
        }

        assert_eq!(Font::default().normal().family, "JetBrainsMono Nerd Font Mono");

        let font = Font::default();
        let rasterizer = Rasterizer::new().unwrap();
        let mut glyph_cache = GlyphCache::new(rasterizer, &font).unwrap();
        let font_key = glyph_cache.font_key;

        let mut loader = TestLoader;
        let shaped = glyph_cache.shape_run("ab", font_key, &mut loader);

        assert_eq!(shaped.len(), 2);
        assert_eq!(shaped[0].cluster, 0);
        assert_eq!(shaped[1].cluster, 1);
    }
}
