//! Conversion of grid-anchored image placements into renderable quads.
//!
//! The terminal core has no font metrics, so placements store the requested
//! size in cells (`c`/`r`) and leave a value of zero to mean "as many cells as
//! needed". Resolving that, and converting cells to pixels, happens here.

use std::sync::Arc;

use alacritty_terminal::graphics::GraphicsStore;
use alacritty_terminal::graphics::image::ImageData;
use alacritty_terminal::graphics::placement::Placement;
use alacritty_terminal::index::Point;
use alacritty_terminal::term;

use crate::display::SizeInfo;
use crate::gl::types::GLuint;
use crate::renderer::images::{ImageKey, ImageQuad};

/// Bits of an [`ImageKey`] reserved for the image's generation.
const GENERATION_BITS: u32 = 32;

/// Mask covering the generation part of an [`ImageKey`].
const GENERATION_MASK: u64 = u32::MAX as u64;

/// An image placement resolved into window pixel coordinates.
#[derive(Debug)]
pub struct RenderableImage {
    /// Pixel data to upload.
    pub image: Arc<ImageData>,
    /// Cache key, combining the image identity and its generation.
    pub key: ImageKey,
    /// Left edge in window pixels.
    pub x: f32,
    /// Top edge in window pixels.
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Source rectangle in texture coordinates.
    pub uv: [f32; 4],
    /// Stacking order relative to text.
    pub z_index: i32,
}

impl RenderableImage {
    /// Build a quad for an uploaded texture.
    pub fn quad(&self, texture: GLuint) -> ImageQuad {
        ImageQuad {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            uv: self.uv,
            texture,
        }
    }
}

/// Key identifying a specific revision of an image in a texture cache.
pub fn image_key(internal_id: u32, generation: u64) -> ImageKey {
    (u64::from(internal_id) << GENERATION_BITS) | (generation & GENERATION_MASK)
}

/// Collect the visible image placements of `store`, in draw order.
pub fn collect(
    store: &GraphicsStore,
    size_info: &SizeInfo,
    display_offset: usize,
) -> Vec<RenderableImage> {
    let mut images = Vec::new();

    for placement in store.placements() {
        // Virtual placements only back Unicode placeholder cells.
        if placement.is_virtual {
            continue;
        }

        let Some(image) = store.image(placement.internal_id) else {
            continue;
        };
        let Some(viewport) = term::point_to_viewport(display_offset, placement.anchor) else {
            continue;
        };
        let Some(rendered) = resolve(placement, image, viewport, size_info) else {
            continue;
        };

        images.push(rendered);
    }

    // `GraphicsStore` already orders by z-index then identity, and `sort_by_key`
    // is stable, so this keeps the protocol's ordering deterministic.
    images.sort_by_key(|image| image.z_index);

    images
}

/// Resolve one placement into pixels.
fn resolve(
    placement: &Placement,
    image: &Arc<ImageData>,
    viewport: Point<usize>,
    size_info: &SizeInfo,
) -> Option<RenderableImage> {
    let cell_width = size_info.cell_width();
    let cell_height = size_info.cell_height();
    if cell_width <= 0. || cell_height <= 0. {
        return None;
    }

    // Clip the source rectangle to the image bounds.
    let source_x = placement.src.x.min(image.width);
    let source_y = placement.src.y.min(image.height);
    let source_width = placement.src.width.min(image.width - source_x);
    let source_height = placement.src.height.min(image.height - source_y);
    if source_width == 0 || source_height == 0 {
        return None;
    }

    let columns = placement.columns;
    let rows = placement.rows;

    let box_width = if columns > 0 { f32::from(columns) * cell_width } else { source_width as f32 };
    let box_height = if rows > 0 { f32::from(rows) * cell_height } else { source_height as f32 };

    let (width, height) =
        fit(box_width, box_height, source_width as f32, source_height as f32, columns, rows);

    // Centre a letterboxed image inside the box it was given.
    let x = viewport.column.0 as f32 * cell_width
        + size_info.padding_x()
        + f32::from(placement.offset_x)
        + (box_width - width) / 2.;
    let y = viewport.line as f32 * cell_height
        + size_info.padding_y()
        + f32::from(placement.offset_y)
        + (box_height - height) / 2.;

    let image_width = image.width as f32;
    let image_height = image.height as f32;
    let uv = [
        source_x as f32 / image_width,
        source_y as f32 / image_height,
        (source_x + source_width) as f32 / image_width,
        (source_y + source_height) as f32 / image_height,
    ];

    Some(RenderableImage {
        image: Arc::clone(image),
        key: image_key(placement.internal_id, image.generation),
        x,
        y,
        width,
        height,
        uv,
        z_index: placement.z_index,
    })
}

/// Work out the on-screen size of an image.
///
/// A cell count of zero means "not specified"; when only one axis is given the
/// other is derived from the source aspect ratio, and when both are given the
/// image is letterboxed rather than distorted.
fn fit(
    box_width: f32,
    box_height: f32,
    source_width: f32,
    source_height: f32,
    columns: u16,
    rows: u16,
) -> (f32, f32) {
    let aspect = source_width / source_height;

    match (columns > 0, rows > 0) {
        // Nothing specified: use the image's natural pixel size.
        (false, false) => (source_width, source_height),
        // Only the number of columns is known.
        (true, false) => (box_width, box_width / aspect),
        // Only the number of rows is known.
        (false, true) => (box_height * aspect, box_height),
        // Both known: fit inside the box without distorting.
        (true, true) => {
            let scale = (box_width / source_width).min(box_height / source_height);
            (source_width * scale, source_height * scale)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_size_is_used_when_no_cells_are_given() {
        assert_eq!(fit(0., 0., 20., 10., 0, 0), (20., 10.));
    }

    #[test]
    fn a_single_axis_derives_the_other_from_the_aspect_ratio() {
        // 40px wide box for a 2:1 image gives a 20px height.
        assert_eq!(fit(40., 0., 20., 10., 4, 0), (40., 20.));
        // 20px tall box for a 2:1 image gives a 40px width.
        assert_eq!(fit(0., 20., 20., 10., 0, 2), (40., 20.));
    }

    #[test]
    fn both_axes_letterbox_without_distorting() {
        // A 2:1 image in a square box is limited by width.
        assert_eq!(fit(40., 40., 20., 10., 4, 4), (40., 20.));
        // A 1:2 image in the same box is limited by height.
        assert_eq!(fit(40., 40., 10., 20., 4, 4), (20., 40.));
    }

    #[test]
    fn keys_separate_generations() {
        assert_ne!(image_key(1, 1), image_key(1, 2));
        assert_ne!(image_key(1, 1), image_key(2, 1));
        assert_eq!(image_key(1, 1), image_key(1, 1));
    }
}
