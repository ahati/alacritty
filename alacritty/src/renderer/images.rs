//! OpenGL rendering of textured quads.
//!
//! This module contains the GPU-side building blocks used to draw inline images: an
//! [`ImageTextureCache`] which owns one GL texture object per image, and an [`ImageRenderer`]
//! which draws textured quads.
//!
//! Images are uploaded in the layout the terminal stores them in, with straight (unpremultiplied)
//! alpha where present, which means the caller is responsible for setting up a blend function that
//! multiplies the source color by the source alpha, exactly like [`RectRenderer`] is set up by
//! `Renderer::draw_rects`.
//!
//! [`RectRenderer`]: crate::renderer::rects::RectRenderer

use std::collections::{HashMap, HashSet};
use std::{mem, ptr};

use ahash::RandomState;

use alacritty_terminal::graphics::image::PixelFormat;

use crate::display::SizeInfo;
use crate::gl::types::*;
use crate::renderer::shader::{ShaderProgram, ShaderVersion};
use crate::{gl, renderer};

/// Opaque identity for a cached image texture.
pub type ImageKey = u64;

/// Shader sources for image rendering program.
const IMAGE_SHADER_F: &str = include_str!("../../res/image.f.glsl");
const IMAGE_SHADER_V: &str = include_str!("../../res/image.v.glsl");

/// Attribute slot of the vertex position in the image shader.
const POSITION_ATTRIB: GLuint = 0;

/// Attribute slot of the vertex texture coordinate in the image shader.
const UV_ATTRIB: GLuint = 1;

/// Number of `f32` components in a vertex position.
const POSITION_COMPONENTS: GLint = 2;

/// Number of `f32` components in a vertex texture coordinate.
const UV_COMPONENTS: GLint = 2;

/// Number of vertices required to draw a single quad as two triangles.
const VERTICES_PER_QUAD: usize = 6;

/// GL pixel format matching the layout of stored image data.
///
/// `TexImage2D` gives three-channel data an alpha of one when it is sampled, so
/// an image without an alpha channel is drawn opaque.
fn gl_format(format: PixelFormat) -> GLenum {
    match format {
        PixelFormat::Rgb => gl::RGB,
        PixelFormat::Rgba => gl::RGBA,
    }
}

/// Index of the texture unit the image sampler reads from.
const IMAGE_TEXTURE_UNIT_INDEX: GLint = 0;

/// Texture unit the image sampler reads from.
const IMAGE_TEXTURE_UNIT: GLenum = gl::TEXTURE0 + IMAGE_TEXTURE_UNIT_INDEX as GLenum;

/// A single textured quad, in window pixel coordinates.
#[derive(Clone, Copy, Debug)]
pub struct ImageQuad {
    /// Left edge in pixels, relative to the window (padding already applied).
    pub x: f32,

    /// Top edge in pixels, relative to the window.
    pub y: f32,

    pub width: f32,
    pub height: f32,

    /// Source rectangle in texture coordinates: `[u0, v0, u1, v1]`.
    ///
    /// `(u0, v0)` is the top-left corner of the source rectangle and `(u1, v1)` its bottom-right
    /// corner, matching the layout of the uploaded data (its first row is at `v = 0`).
    pub uv: [f32; 4],

    /// GL texture handle returned by [`ImageTextureCache::upload`].
    pub texture: GLuint,
}

/// A single vertex of a textured quad.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Vertex {
    // Position in normalized device coordinates.
    x: f32,
    y: f32,

    // Texture coordinates.
    u: f32,
    v: f32,
}

/// Owns GL texture objects for images.
#[derive(Debug)]
pub struct ImageTextureCache {
    /// All uploaded textures, indexed by their image key.
    textures: HashMap<ImageKey, GLuint, RandomState>,

    /// Maximum texture dimension supported by the driver.
    ///
    /// This is queried lazily on the first upload, since it requires a current GL context.
    max_texture_size: Option<u32>,
}

impl Default for ImageTextureCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageTextureCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self { textures: HashMap::default(), max_texture_size: None }
    }

    /// Upload `pixels` (tightly packed 8-bit data in `format`, with straight/unpremultiplied
    /// alpha where present) as a 2D texture for `key`, replacing any previous entry.
    ///
    /// Returns the GL texture handle, which stays valid until the entry is replaced, evicted by
    /// [`retain`](Self::retain), removed by [`remove`](Self::remove), or the cache is dropped.
    ///
    /// Empty images, images whose dimensions exceed `GL_MAX_TEXTURE_SIZE` and data which does not
    /// match `width`/`height`/`format` are rejected; `0` is returned in that case.
    pub fn upload(
        &mut self,
        key: ImageKey,
        width: u32,
        height: u32,
        format: PixelFormat,
        pixels: &[u8],
    ) -> GLuint {
        if width == 0 || height == 0 {
            debug_assert!(width > 0 && height > 0, "cannot upload an empty image");
            return 0;
        }

        let expected_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(format.channels()));

        if expected_len != Some(pixels.len()) {
            debug_assert!(false, "image data length does not match its dimensions");
            return 0;
        }

        let max_texture_size = self.max_texture_size();

        if width > max_texture_size || height > max_texture_size {
            debug_assert!(false, "image dimensions exceed GL_MAX_TEXTURE_SIZE");
            return 0;
        }

        // Reuse the existing texture object, so re-uploading an image doesn't churn texture ids.
        let texture = match self.textures.get(&key) {
            Some(texture) => *texture,
            None => {
                let mut texture: GLuint = 0;
                unsafe {
                    gl::GenTextures(1, &mut texture);
                }
                self.textures.insert(key, texture);
                texture
            },
        };

        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl_format(format) as i32,
                width as i32,
                height as i32,
                0,
                gl_format(format),
                gl::UNSIGNED_BYTE,
                pixels.as_ptr() as *const _,
            );

            // Images are sampled with linear filtering, without any mipmaps.
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);

            gl::BindTexture(gl::TEXTURE_2D, 0);
        }

        texture
    }

    /// Look up an already-uploaded texture.
    pub fn get(&self, key: ImageKey) -> Option<GLuint> {
        self.textures.get(&key).copied()
    }

    /// Drop every texture whose key is absent from `live`.
    ///
    /// Use this to release GPU memory for images the terminal no longer references.
    pub fn retain(&mut self, live: &HashSet<ImageKey>) {
        let mut removed = Vec::new();

        self.textures.retain(|key, texture| {
            let keep = live.contains(key);
            if !keep {
                removed.push(*texture);
            }
            keep
        });

        Self::delete(&removed);
    }

    /// Forget every cached texture without deleting it.
    ///
    /// Used when the GL context is lost: the texture names are no longer valid,
    /// and deleting them against a new context could free unrelated objects.
    pub fn forget(&mut self) {
        self.textures.clear();
    }

    /// Delete every texture.
    pub fn clear(&mut self) {
        let textures: Vec<GLuint> = self.textures.drain().map(|(_, texture)| texture).collect();
        Self::delete(&textures);
    }

    /// Get the maximum supported texture dimension of the driver.
    fn max_texture_size(&mut self) -> u32 {
        *self.max_texture_size.get_or_insert_with(|| {
            let mut max_texture_size: GLint = 0;
            unsafe {
                gl::GetIntegerv(gl::MAX_TEXTURE_SIZE, &mut max_texture_size);
            }

            // A failed query must not prevent all uploads.
            max_texture_size.max(1) as u32
        })
    }

    /// Delete all `textures` from the active GL context.
    fn delete(textures: &[GLuint]) {
        if textures.is_empty() {
            return;
        }

        unsafe {
            gl::DeleteTextures(textures.len() as i32, textures.as_ptr());
        }
    }
}

impl Drop for ImageTextureCache {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Draws textured quads.
///
/// Assumes the caller has already set the GL viewport to the full window and configured blending,
/// mirroring how [`RectRenderer`](crate::renderer::rects::RectRenderer) is driven from
/// `Renderer::draw_rects`.
#[derive(Debug)]
pub struct ImageRenderer {
    // GL buffer objects.
    vao: GLuint,
    vbo: GLuint,

    program: ShaderProgram,

    /// Scratch buffer with the vertices of the quads of the current frame.
    vertices: Vec<Vertex>,
}

impl ImageRenderer {
    /// Build the renderer for the active GL version.
    pub fn new(shader_version: ShaderVersion) -> Result<Self, renderer::Error> {
        let mut vao: GLuint = 0;
        let mut vbo: GLuint = 0;

        let program = create_program(shader_version)?;

        unsafe {
            // Allocate buffers.
            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut vbo);

            gl::BindVertexArray(vao);

            // VBO binding is not part of VAO itself, but VBO binding is stored in attributes.
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);

            // Position.
            gl::VertexAttribPointer(
                POSITION_ATTRIB,
                POSITION_COMPONENTS,
                gl::FLOAT,
                gl::FALSE,
                mem::size_of::<Vertex>() as i32,
                ptr::null(),
            );
            gl::EnableVertexAttribArray(POSITION_ATTRIB);

            // Texture coordinates.
            gl::VertexAttribPointer(
                UV_ATTRIB,
                UV_COMPONENTS,
                gl::FLOAT,
                gl::FALSE,
                mem::size_of::<Vertex>() as i32,
                mem::offset_of!(Vertex, u) as *const _,
            );
            gl::EnableVertexAttribArray(UV_ATTRIB);

            // Reset buffer bindings.
            gl::BindVertexArray(0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
        }

        Ok(Self { vao, vbo, program, vertices: Vec::new() })
    }

    /// Draw `quads` in the given order (painter's algorithm — first quad is drawn first, so the
    /// last quad ends up on top).
    ///
    /// Quads with a texture handle of `0` (a failed [`ImageTextureCache::upload`]) are skipped.
    ///
    /// The caller is expected to have set the viewport to the full window and to have configured
    /// blending for straight (unpremultiplied) alpha, i.e.
    /// `gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA)`. The text renderer's dual-source
    /// blending must not be active, since it requires premultiplied colors. Blending and the
    /// viewport are left untouched, matching how `Renderer::draw_rects` drives the rect renderer.
    pub fn draw(&mut self, size_info: &SizeInfo, quads: &[ImageQuad]) {
        if quads.is_empty() {
            return;
        }

        let half_width = size_info.width() / 2.;
        let half_height = size_info.height() / 2.;

        // Build quad vertices vector.
        self.vertices.clear();
        for quad in quads {
            Self::add_quad(&mut self.vertices, half_width, half_height, quad);
        }

        unsafe {
            // Bind VAO to enable vertex attribute slots.
            gl::BindVertexArray(self.vao);

            // Bind VBO only once for buffer data upload only.
            gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);

            gl::ActiveTexture(IMAGE_TEXTURE_UNIT);
            gl::UseProgram(self.program.id());

            // Upload all quad vertices of this frame at once.
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (self.vertices.len() * mem::size_of::<Vertex>()) as isize,
                self.vertices.as_ptr() as *const _,
                gl::STREAM_DRAW,
            );

            // Each quad can use its own texture, so a separate draw call is issued for every quad
            // to preserve the caller's ordering.
            let mut first_vertex = 0;
            for quad in quads {
                if quad.texture != 0 {
                    gl::BindTexture(gl::TEXTURE_2D, quad.texture);
                    gl::DrawArrays(gl::TRIANGLES, first_vertex, VERTICES_PER_QUAD as i32);
                }

                // Every quad contributes its vertices, even when it isn't drawn.
                first_vertex += VERTICES_PER_QUAD as i32;
            }

            // Disable program and reset buffer bindings.
            gl::UseProgram(0);
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
        }
    }

    /// Expand a single quad into the vertices of its two triangles.
    fn add_quad(vertices: &mut Vec<Vertex>, half_width: f32, half_height: f32, quad: &ImageQuad) {
        // Calculate the quad vertices positions in normalized device coordinates. The NDC range
        // goes from -1 to +1 with Y pointing up, so the Y axis has to be flipped, just like the
        // rect renderer does.
        let left = quad.x / half_width - 1.;
        let right = (quad.x + quad.width) / half_width - 1.;
        let top = -quad.y / half_height + 1.;
        let bottom = -(quad.y + quad.height) / half_height + 1.;

        let [u0, v0, u1, v1] = quad.uv;

        // Make quad vertices, with the texture coordinates matching the window corners.
        let top_left = Vertex { x: left, y: top, u: u0, v: v0 };
        let bottom_left = Vertex { x: left, y: bottom, u: u0, v: v1 };
        let top_right = Vertex { x: right, y: top, u: u1, v: v0 };
        let bottom_right = Vertex { x: right, y: bottom, u: u1, v: v1 };

        // Append the vertices to form two triangles.
        let quad_vertices: [Vertex; VERTICES_PER_QUAD] =
            [top_left, bottom_left, top_right, top_right, bottom_left, bottom_right];

        vertices.extend_from_slice(&quad_vertices);
    }
}

impl Drop for ImageRenderer {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteBuffers(1, &self.vbo);
            gl::DeleteVertexArrays(1, &self.vao);
        }
    }
}

/// Image drawing program.
///
/// The sampler is bound to [`IMAGE_TEXTURE_UNIT_INDEX`] once here, since the
/// binding is per-program state that never changes.
fn create_program(shader_version: ShaderVersion) -> Result<ShaderProgram, renderer::Error> {
    let program = ShaderProgram::new(shader_version, None, IMAGE_SHADER_V, IMAGE_SHADER_F)?;

    unsafe {
        gl::UseProgram(program.id());

        // Make sure the sampler reads from the unit quads are bound to.
        if let Ok(u_image) = program.get_uniform_location(c"image") {
            gl::Uniform1i(u_image, IMAGE_TEXTURE_UNIT_INDEX);
        }

        gl::UseProgram(0);
    }

    Ok(program)
}
