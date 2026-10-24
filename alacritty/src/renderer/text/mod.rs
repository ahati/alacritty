use bitflags::bitflags;
use crossfont::{FontKey, GlyphKey, RasterizedGlyph};

use alacritty_terminal::index::Column;
use alacritty_terminal::term::cell::Flags;

use crate::display::SizeInfo;
use crate::display::content::RenderableCell;
use crate::gl;
use crate::gl::types::*;
use crate::text_run::TextRun;

mod atlas;
mod builtin_font;
mod gles2;
mod glsl3;
pub mod glyph_cache;

use atlas::Atlas;
pub use gles2::Gles2Renderer;
pub use glsl3::Glsl3Renderer;
pub use glyph_cache::GlyphCache;
use glyph_cache::{Glyph, LoadGlyph};

// NOTE: These flags must be in sync with their usage in the text.*.glsl shaders.
bitflags! {
    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct RenderingGlyphFlags: u8 {
        const COLORED   = 0b0000_0001;
        const WIDE_CHAR = 0b0000_0010;
    }
}

/// Rendering passes, for both GLES2 and GLSL3 renderer.
#[repr(u8)]
enum RenderingPass {
    /// Rendering pass used to render background color in text shaders.
    Background = 0,

    /// The first pass to render text with both GLES2 and GLSL3 renderers.
    SubpixelPass1 = 1,

    /// The second pass to render text with GLES2 renderer.
    SubpixelPass2 = 2,

    /// The third pass to render text with GLES2 renderer.
    SubpixelPass3 = 3,
}

pub trait TextRenderer<'a> {
    type Shader: TextShader;
    type RenderBatch: TextRenderBatch;
    type RenderApi: TextRenderApi<Self::RenderBatch>;

    /// Get loader API for the renderer.
    fn loader_api(&mut self) -> LoaderApi<'_>;

    /// Draw text runs.
    fn draw_runs<'b: 'a, I: Iterator<Item = TextRun>>(
        &'b mut self,
        size_info: &'b SizeInfo,
        glyph_cache: &'a mut GlyphCache,
        runs: I,
    ) {
        self.with_api(size_info, |mut api| {
            for run in runs {
                api.draw_run(run, glyph_cache, size_info);
            }
        })
    }

    fn with_api<'b: 'a, F, T>(&'b mut self, size_info: &'b SizeInfo, func: F) -> T
    where
        F: FnOnce(Self::RenderApi) -> T;

    fn program(&self) -> &Self::Shader;

    /// Forget the cached texture binding.
    ///
    /// The text renderer skips redundant `GL_TEXTURE_2D` binds, so any code
    /// outside it that changes the binding on the active texture unit must call
    /// this. Otherwise the next batch skips its own bind and samples whatever
    /// texture was left bound.
    fn invalidate_texture_binding(&mut self);

    /// Resize the text rendering.
    fn resize(&self, size: &SizeInfo) {
        unsafe {
            let program = self.program();
            gl::UseProgram(program.id());
            update_projection(program.projection_uniform(), size);
            gl::UseProgram(0);
        }
    }

    /// Invoke renderer with the loader.
    fn with_loader<F: FnOnce(LoaderApi<'_>) -> T, T>(&mut self, func: F) -> T {
        unsafe {
            gl::ActiveTexture(gl::TEXTURE0);
        }

        func(self.loader_api())
    }
}

pub trait TextRenderBatch {
    /// Check if `Batch` is empty.
    fn is_empty(&self) -> bool;

    /// Check whether the `Batch` is full.
    fn full(&self) -> bool;

    /// Get texture `Batch` is using.
    fn tex(&self) -> GLuint;

    /// Add item to the batch.
    fn add_item(&mut self, cell: &RenderableCell, glyph: &Glyph, size_info: &SizeInfo);
}

pub trait TextRenderApi<T: TextRenderBatch>: LoadGlyph {
    /// Get `Batch` the api is using.
    fn batch(&mut self) -> &mut T;
    /// Render the underlying data.
    fn render_batch(&mut self);

    /// Add item to the rendering queue.
    #[inline]
    fn add_render_item(&mut self, cell: &RenderableCell, glyph: &Glyph, size_info: &SizeInfo) {
        // Flush batch if tex changing.
        if !self.batch().is_empty() && self.batch().tex() != glyph.tex_id {
            self.render_batch();
        }

        self.batch().add_item(cell, glyph, size_info);

        // Render batch and clear if it's full.
        if self.batch().full() {
            self.render_batch();
        }
    }

    /// Get the [`FontKey`] for drawing cells with `flags`.
    fn determine_font_key(flags: Flags, glyph_cache: &GlyphCache) -> FontKey {
        match flags & Flags::BOLD_ITALIC {
            Flags::BOLD_ITALIC => glyph_cache.bold_italic_key,
            Flags::ITALIC => glyph_cache.italic_key,
            Flags::BOLD => glyph_cache.bold_key,
            _ => glyph_cache.font_key,
        }
    }

    /// Draw a text run.
    ///
    /// The run is shaped together, so glyphs like ligatures which span multiple
    /// cells can be rendered; everything that breaks up such a glyph (like a
    /// selection) also breaks up the text run.
    fn draw_run(
        &mut self,
        run: TextRun,
        glyph_cache: &mut GlyphCache,
        size_info: &SizeInfo,
    ) {
        let font_key = Self::determine_font_key(run.flags, glyph_cache);

        // Hidden cells draw their background only.
        if run.flags.contains(Flags::HIDDEN) {
            let glyph_key =
                GlyphKey { font_key, size: glyph_cache.font_size, character: ' ' };
            let glyph = glyph_cache.get(glyph_key, self, true);
            for cell in run.cells() {
                self.add_render_item(&cell, &glyph, size_info);
            }
            return;
        }

        // Shape the entire run, so ligatures spanning multiple cells are
        // rendered as a single glyph.
        let text = run.text();
        let shaped = glyph_cache.shape_run(&text, font_key, self);

        // Map every shaped glyph to the column of its cluster. Ligatures cover
        // the columns of all their characters, while the additional glyphs of
        // expanded clusters render inside the cluster's first cell.
        let start_column = run.span.0.0;
        let mut columns: Vec<Vec<Glyph>> = vec![Vec::new(); run.num_columns()];

        let mut char_index = 0usize;
        let mut chars = text.char_indices().peekable();
        for glyph in shaped {
            while let Some((offset, _)) = chars.peek() {
                if *offset < glyph.cluster {
                    chars.next();
                    char_index += 1;
                } else {
                    break;
                }
            }

            let column = run.column_of_char(char_index);
            if column.0 < start_column || column.0 > run.span.1.0 {
                continue;
            }
            columns[column.0 - start_column].push(glyph.glyph);
        }

        // Spaces cover the cells consumed by ligatures, so that their
        // backgrounds are drawn without drawing a glyph.
        let space = GlyphKey { font_key, size: glyph_cache.font_size, character: ' ' };
        let space = glyph_cache.get(space, self, true);

        let wide = run.flags.contains(Flags::WIDE_CHAR);
        for (index, glyphs) in columns.into_iter().enumerate() {
            // The spacer of wide characters is drawn as part of the wide
            // character's item, which the shader expands over both cells.
            if wide && index % 2 == 1 {
                continue;
            }

            let cell = run.dummy_cell_at(Column(start_column + index));
            let mut glyphs = glyphs.into_iter();
            let glyph = glyphs.next().unwrap_or(space);
            self.add_render_item(&cell, &glyph, size_info);

            // Additional glyphs of expanded clusters render in the same cell.
            for glyph in glyphs {
                self.add_render_item(&cell, &glyph, size_info);
            }
        }

        // Render visible zero-width characters inside their character's cell.
        for (index, zerowidth) in run.content.zero_widths.iter().enumerate() {
            let Some(zerowidth) = zerowidth else { continue };

            let column = run.column_of_char(index);
            let cell = run.dummy_cell_at(column);
            for character in zerowidth {
                let glyph_key =
                    GlyphKey { font_key, size: glyph_cache.font_size, character: *character };
                let glyph = glyph_cache.get(glyph_key, self, false);
                self.add_render_item(&cell, &glyph, size_info);
            }
        }
    }
}

pub trait TextShader {
    fn id(&self) -> GLuint;

    /// Id of the projection uniform.
    fn projection_uniform(&self) -> GLint;
}

#[derive(Debug)]
pub struct LoaderApi<'a> {
    active_tex: &'a mut GLuint,
    atlas: &'a mut Vec<Atlas>,
    current_atlas: &'a mut usize,
}

impl LoadGlyph for LoaderApi<'_> {
    fn load_glyph(&mut self, rasterized: &RasterizedGlyph) -> Glyph {
        Atlas::load_glyph(self.active_tex, self.atlas, self.current_atlas, rasterized)
    }

    fn clear(&mut self) {
        Atlas::clear_atlas(self.atlas, self.current_atlas)
    }
}

fn update_projection(u_projection: GLint, size: &SizeInfo) {
    let width = size.width();
    let height = size.height();
    let padding_x = size.padding_x();
    let padding_y = size.padding_y();

    // Bounds check.
    if (width as u32) < (2 * padding_x as u32) || (height as u32) < (2 * padding_y as u32) {
        return;
    }

    // Compute scale and offset factors, from pixel to ndc space. Y is inverted.
    //   [0, width - 2 * padding_x] to [-1, 1]
    //   [height - 2 * padding_y, 0] to [-1, 1]
    let scale_x = 2. / (width - 2. * padding_x);
    let scale_y = -2. / (height - 2. * padding_y);
    let offset_x = -1.;
    let offset_y = 1.;

    unsafe {
        gl::Uniform4f(u_projection, offset_x, offset_y, scale_x, scale_y);
    }
}
