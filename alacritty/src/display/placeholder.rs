//! Unicode placeholder decoding for the Kitty graphics protocol.
//!
//! Kitty specifies image display via `U+10EEEE` cells with combining
//! diacritical marks indicating row, column, and high byte of image ID.

use std::sync::Arc;

use alacritty_terminal::graphics::image::ImageData;
use alacritty_terminal::graphics::placement::Placement;
use alacritty_terminal::vte::ansi::Color;

use crate::display::content::RenderableCell;
use crate::display::SizeInfo;
use crate::renderer::images::ImageQuad;

pub use alacritty_terminal::graphics::ROWCOLUMN_DIACRITICS;

/// Look up the 0-based index for a diacritical combining character.
pub fn diacritic_to_number(c: char) -> Option<u32> {
    ROWCOLUMN_DIACRITICS.binary_search(&(c as u32)).ok().map(|idx| idx as u32)
}

/// Tracking state for left-to-right placeholder diacritic and ID inheritance.
#[derive(Clone, Copy, Debug)]
pub struct PlaceholderState {
    pub line: usize,
    pub grid_col: usize,
    pub raw_fg: Color,
    pub raw_underline: Option<Color>,
    pub row: u32,
    pub col: u32,
    pub msb: u32,
}

/// Fully decoded Unicode placeholder specification for a cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedPlaceholder {
    pub image_id: u32,
    pub placement_id: u32,
    pub row: u32,
    pub col: u32,
    pub line: usize,
    pub grid_col: usize,
}

/// Decode a single `U+10EEEE` placeholder cell following the Kitty 0.50 protocol specification.
pub fn decode_placeholder(
    cell: &RenderableCell,
    last_state: &mut Option<PlaceholderState>,
) -> Option<DecodedPlaceholder> {
    if cell.character != '\u{10EEEE}' {
        return None;
    }

    // Spec: Image ID is specified using the foreground color of the placeholder character.
    // An index into 256-color palette gives the index directly. 24-bit RGB gives (R<<16)|(G<<8)|B.
    let mut image_id = match cell.raw_fg {
        Color::Indexed(idx) => u32::from(idx),
        Color::Spec(rgb) => {
            (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
        },
        Color::Named(named) => {
            let val = named as usize;
            if val < 256 {
                val as u32
            } else {
                0
            }
        },
    };

    // Spec: Placement ID is specified using the underline color. If omitted, placement ID is 0.
    let placement_id = match cell.raw_underline {
        None => 0,
        Some(Color::Indexed(idx)) => u32::from(idx),
        Some(Color::Spec(rgb)) => {
            (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
        },
        Some(Color::Named(named)) => {
            let val = named as usize;
            if val < 256 {
                val as u32
            } else {
                0
            }
        },
    };

    let mut d_row = None;
    let mut d_col = None;
    let mut d_msb = None;

    if let Some(extra) = &cell.extra {
        if let Some(zerowidth) = &extra.zerowidth {
            if !zerowidth.is_empty() {
                d_row = diacritic_to_number(zerowidth[0]);
            }
            if zerowidth.len() > 1 {
                d_col = diacritic_to_number(zerowidth[1]);
            }
            if zerowidth.len() > 2 {
                d_msb = diacritic_to_number(zerowidth[2]);
            }
        }
    }

    let is_prev = last_state.as_ref().is_some_and(|last| {
        last.line == cell.point.line
            && last.grid_col + 1 == cell.point.column.0
            && last.raw_fg == cell.raw_fg
            && last.raw_underline == cell.raw_underline
    });

    let (row, col, msb) = match (d_row, d_col, d_msb) {
        // Rule 1: If no diacritics are present, and the previous placeholder cell has the
        // same foreground and underline colors, then row = prev.row, col = prev.col + 1, msb = prev.msb.
        (None, None, None) => {
            if let (true, Some(last)) = (is_prev, last_state.as_ref()) {
                (last.row, last.col + 1, last.msb)
            } else {
                (0, 0, 0)
            }
        },
        // Rule 2: If only the row diacritic is present, and previous placeholder cell has the same row
        // and same foreground/underline colors, then col = prev.col + 1, msb = prev.msb.
        (Some(r), None, None) => {
            if let (true, Some(last)) = (is_prev, last_state.as_ref()) {
                if last.row == r {
                    (r, last.col + 1, last.msb)
                } else {
                    (r, 0, 0)
                }
            } else {
                (r, 0, 0)
            }
        },
        // Rule 3: If only row and column diacritics are present, and previous has same row and colors,
        // and its col is one less than current col, then msb = prev.msb.
        (Some(r), Some(c), None) => {
            if let (true, Some(last)) = (is_prev, last_state.as_ref()) {
                if last.row == r && last.col + 1 == c {
                    (r, c, last.msb)
                } else {
                    (r, c, 0)
                }
            } else {
                (r, c, 0)
            }
        },
        // General fallback when all three are given or non-standard combinations occur.
        (r, c, m) => {
            let r = r.unwrap_or_else(|| if is_prev { last_state.as_ref().unwrap().row } else { 0 });
            let c = c.unwrap_or_else(|| {
                if is_prev { last_state.as_ref().unwrap().col + 1 } else { 0 }
            });
            let m = m.unwrap_or_else(|| if is_prev { last_state.as_ref().unwrap().msb } else { 0 });
            (r, c, m)
        },
    };

    image_id |= (msb & 0xFF) << 24;

    *last_state = Some(PlaceholderState {
        line: cell.point.line,
        grid_col: cell.point.column.0,
        raw_fg: cell.raw_fg,
        raw_underline: cell.raw_underline,
        row,
        col,
        msb,
    });

    Some(DecodedPlaceholder {
        image_id,
        placement_id,
        row,
        col,
        line: cell.point.line,
        grid_col: cell.point.column.0,
    })
}

/// Construct textured quads for `U+10EEEE` Unicode placeholder cells, partitioned
/// into behind-text quads (`z_index < 0`) and in-front-of-text quads (`z_index >= 0`).
///
/// Each placeholder cell is decoded to determine image ID, placement ID, and tile coordinates.
/// Decoded cells have their character set to `' '` to suppress text glyph rendering.
/// Placements are rendered preserving aspect ratio matching the Kitty 0.50 reference implementation.
pub fn build_placeholder_quads<F>(
    cells: &mut [RenderableCell],
    virtual_placements: &[(Placement, Arc<ImageData>)],
    size_info: &SizeInfo,
    mut get_texture: F,
) -> (Vec<ImageQuad>, Vec<ImageQuad>)
where
    F: FnMut(&Placement, &ImageData) -> u32,
{
    let mut behind_quads = Vec::new();
    let mut front_quads = Vec::new();
    let mut last_placeholder = None;

    let cell_w = size_info.cell_width();
    let cell_h = size_info.cell_height();
    let pad_x = size_info.padding_x();
    let pad_y = size_info.padding_y();

    for cell in cells.iter_mut() {
        if cell.character == '\u{10EEEE}' {
            let decoded = decode_placeholder(cell, &mut last_placeholder);
            cell.character = ' ';

            let Some(decoded) = decoded else { continue };

            let Some((p, img)) = virtual_placements.iter().find(|(p, img)| {
                (decoded.placement_id == 0 || p.placement_id == decoded.placement_id)
                    && (img.client_id == decoded.image_id || img.number == decoded.image_id)
            }) else {
                continue;
            };

            if !img.is_ready() {
                continue;
            }

            let texture = get_texture(p, img);
            if texture == 0 {
                continue;
            }

            let total_cols = p.columns.max(1) as f32;
            let total_rows = p.rows.max(1) as f32;

            let box_w = total_cols * cell_w;
            let box_h = total_rows * cell_h;

            let src_x = p.src.x as f32;
            let src_y = p.src.y as f32;
            let src_w = (if p.src.width == 0 { img.width } else { p.src.width }) as f32;
            let src_h = (if p.src.height == 0 { img.height } else { p.src.height }) as f32;

            if src_w <= 0. || src_h <= 0. || box_w <= 0. || box_h <= 0. {
                continue;
            }

            // Aspect ratio calculation matching Kitty reference implementation (grman_put_cell_image):
            // Destination box size: box_w by box_h.
            // Fit image preserving aspect ratio:
            let (x_offset, y_offset, x_scale, y_scale) =
                if src_w * box_h > src_h * box_w {
                    // Fit to width, center vertically
                    let scale = box_w / src_w.max(1.0);
                    let y_off = (box_h - src_h * scale) / 2.0;
                    (0.0, y_off, scale, scale)
                } else {
                    // Fit to height, center horizontally
                    let scale = box_h / src_h.max(1.0);
                    let x_off = (box_w - src_w * scale) / 2.0;
                    (x_off, 0.0, scale, scale)
                };

            // Destination coordinates for this placeholder tile:
            let x_dst = decoded.col as f32 * cell_w;
            let y_dst = decoded.row as f32 * cell_h;
            let w_dst = cell_w;
            let h_dst = cell_h;

            // Compute intersection between cell rectangle and image rectangle in box coordinates
            let intersect_x0 = x_dst.max(x_offset);
            let intersect_x1 = (x_dst + w_dst).min(x_offset + src_w * x_scale);
            let intersect_y0 = y_dst.max(y_offset);
            let intersect_y1 = (y_dst + h_dst).min(y_offset + src_h * y_scale);

            if intersect_x0 >= intersect_x1 || intersect_y0 >= intersect_y1 {
                // Cell is in the letterbox / pillarbox margin
                continue;
            }

            // Map intersection to screen pixel coordinates, including placement pixel offsets X/Y
            let cell_screen_x = cell.point.column.0 as f32 * cell_w + pad_x + p.offset_x as f32;
            let cell_screen_y = cell.point.line as f32 * cell_h + pad_y + p.offset_y as f32;

            let quad_x = cell_screen_x + (intersect_x0 - x_dst);
            let quad_y = cell_screen_y + (intersect_y0 - y_dst);
            let quad_w = intersect_x1 - intersect_x0;
            let quad_h = intersect_y1 - intersect_y0;

            // Map intersection to image source coordinates and UVs:
            let s0 = src_x + (intersect_x0 - x_offset) / x_scale;
            let s1 = src_x + (intersect_x1 - x_offset) / x_scale;
            let t0 = src_y + (intersect_y0 - y_offset) / y_scale;
            let t1 = src_y + (intersect_y1 - y_offset) / y_scale;

            let img_w = img.width as f32;
            let img_h = img.height as f32;
            let u0 = s0 / img_w;
            let u1 = s1 / img_w;
            let v0 = t0 / img_h;
            let v1 = t1 / img_h;

            let quad = ImageQuad {
                x: quad_x,
                y: quad_y,
                width: quad_w,
                height: quad_h,
                uv: [u0, v0, u1, v1],
                texture,
            };

            if p.z_index < 0 {
                behind_quads.push(quad);
            } else {
                front_quads.push(quad);
            }
        }
    }

    (behind_quads, front_quads)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::Rgb as VteRgb;
    use crate::display::color::Rgb;
    use crate::display::content::RenderableCellExtra;

    fn make_placeholder_cell(
        line: usize,
        col: usize,
        raw_fg: Color,
        raw_underline: Option<Color>,
        zerowidth: Option<Vec<char>>,
    ) -> RenderableCell {
        RenderableCell {
            point: Point::new(line, Column(col)),
            character: '\u{10EEEE}',
            extra: zerowidth.map(|zw| {
                Box::new(RenderableCellExtra {
                    zerowidth: Some(zw),
                    hyperlink: None,
                })
            }),
            flags: Flags::empty(),
            bg_alpha: 1.,
            fg: Rgb::default(),
            bg: Rgb::default(),
            underline: Rgb::default(),
            raw_fg,
            raw_underline,
        }
    }

    #[test]
    fn rowcolumn_diacritic_indices() {
        assert_eq!(ROWCOLUMN_DIACRITICS.len(), 297);
        assert_eq!(ROWCOLUMN_DIACRITICS[0], 0x0305);
        assert_eq!(ROWCOLUMN_DIACRITICS[1], 0x030D);
        assert_eq!(ROWCOLUMN_DIACRITICS[2], 0x030E);
        assert_eq!(ROWCOLUMN_DIACRITICS[15], 0x0357);
        assert_eq!(ROWCOLUMN_DIACRITICS[255], 0xA8E5);

        assert_eq!(diacritic_to_number('\u{0305}'), Some(0));
        assert_eq!(diacritic_to_number('\u{030D}'), Some(1));
        assert_eq!(diacritic_to_number('\u{030E}'), Some(2));
        assert_eq!(diacritic_to_number('\u{0357}'), Some(15));
        assert_eq!(diacritic_to_number('\u{A8E5}'), Some(255));
    }

    #[test]
    fn decode_palette_indexed_foreground_and_default_underline() {
        let mut last_state = None;
        // Spec canonical 2x2 example: printf "\e[38;5;42m\U10EEEE\U0305\U0305...\n"
        let cell = make_placeholder_cell(
            0,
            0,
            Color::Indexed(42),
            None,
            Some(vec!['\u{0305}', '\u{0305}']),
        );
        let decoded = decode_placeholder(&cell, &mut last_state).expect("must decode");
        assert_eq!(decoded.image_id, 42);
        assert_eq!(decoded.placement_id, 0);
        assert_eq!(decoded.row, 0);
        assert_eq!(decoded.col, 0);
    }

    #[test]
    fn decode_truecolor_foreground_and_indexed_underline() {
        let mut last_state = None;
        let cell = make_placeholder_cell(
            1,
            2,
            Color::Spec(VteRgb { r: 1, g: 2, b: 3 }),
            Some(Color::Indexed(7)),
            Some(vec!['\u{030D}', '\u{030E}']),
        );
        let decoded = decode_placeholder(&cell, &mut last_state).expect("must decode");
        assert_eq!(decoded.image_id, (1 << 16) | (2 << 8) | 3);
        assert_eq!(decoded.placement_id, 7);
        assert_eq!(decoded.row, 1);
        assert_eq!(decoded.col, 2);
    }

    #[test]
    fn decode_third_diacritic_msb() {
        let mut last_state = None;
        // Spec: 33554474 = 42 + (2 << 24). Diacritic index 2 is U+030E.
        let cell = make_placeholder_cell(
            0,
            0,
            Color::Indexed(42),
            None,
            Some(vec!['\u{0305}', '\u{0305}', '\u{030E}']),
        );
        let decoded = decode_placeholder(&cell, &mut last_state).expect("must decode");
        assert_eq!(decoded.image_id, 33554474);
        assert_eq!(decoded.row, 0);
        assert_eq!(decoded.col, 0);
    }

    #[test]
    fn left_to_right_inheritance_2x3_matrix() {
        let mut last_state = None;
        let fg = Color::Indexed(42);

        // Row 0: \e[38;5;42m\U10EEEE\U0305\U10EEEE\U10EEEE
        let c00 = make_placeholder_cell(0, 0, fg, None, Some(vec!['\u{0305}']));
        let d00 = decode_placeholder(&c00, &mut last_state).unwrap();
        assert_eq!((d00.row, d00.col), (0, 0));

        let c01 = make_placeholder_cell(0, 1, fg, None, None);
        let d01 = decode_placeholder(&c01, &mut last_state).unwrap();
        assert_eq!((d01.row, d01.col), (0, 1));

        let c02 = make_placeholder_cell(0, 2, fg, None, None);
        let d02 = decode_placeholder(&c02, &mut last_state).unwrap();
        assert_eq!((d02.row, d02.col), (0, 2));

        // Row 1: \e[38;5;42m\U10EEEE\U030D\U10EEEE\U10EEEE
        let c10 = make_placeholder_cell(1, 0, fg, None, Some(vec!['\u{030D}']));
        let d10 = decode_placeholder(&c10, &mut last_state).unwrap();
        assert_eq!((d10.row, d10.col), (1, 0));

        let c11 = make_placeholder_cell(1, 1, fg, None, None);
        let d11 = decode_placeholder(&c11, &mut last_state).unwrap();
        assert_eq!((d11.row, d11.col), (1, 1));

        let c12 = make_placeholder_cell(1, 2, fg, None, None);
        let d12 = decode_placeholder(&c12, &mut last_state).unwrap();
        assert_eq!((d12.row, d12.col), (1, 2));
    }

    #[test]
    fn display_placeholder_quads_2x2_matrix_and_glyph_suppression() {
        let size_info = SizeInfo::new(100.0, 100.0, 10.0, 20.0, 0.0, 0.0, true);
        let fg = Color::Indexed(42);

        // 2x2 placeholder cells with \u{10EEEE}
        let mut cells = vec![
            make_placeholder_cell(0, 0, fg, None, Some(vec!['\u{0305}', '\u{0305}'])),
            make_placeholder_cell(0, 1, fg, None, None),
            make_placeholder_cell(1, 0, fg, None, Some(vec!['\u{030D}'])),
            make_placeholder_cell(1, 1, fg, None, None),
        ];

        // Virtual placement backing the placeholder
        let placement = Placement {
            internal_id: 1,
            placement_id: 0,
            anchor: Point::new(Line(0), Column(0)),
            footprint: alacritty_terminal::graphics::placement::CellFootprint { columns: 2, rows: 2 },
            offset_x: 0,
            offset_y: 0,
            columns: 2,
            rows: 2,
            src: alacritty_terminal::graphics::placement::SourceRect { x: 0, y: 0, width: 20, height: 40 },
            z_index: 0,
            is_virtual: true,
            parent: None,
            relative_offset: (0, 0),
        };

        let image = Arc::new(alacritty_terminal::graphics::image::ImageData::new(
            42,
            0,
            20,
            40,
            alacritty_terminal::graphics::image::PixelFormat::Rgba,
            Arc::new([0u8; 20 * 40 * 4]),
        ));

        let virtual_placements = vec![(placement, image)];

        let (behind, front) = build_placeholder_quads(
            &mut cells,
            &virtual_placements,
            &size_info,
            |_p, _img| 42,
        );

        // Glyphs must be suppressed to space
        for cell in &cells {
            assert_eq!(cell.character, ' ', "placeholder glyph must be suppressed to space");
        }

        assert!(behind.is_empty());
        assert_eq!(front.len(), 4);

        // Cell (0,0): top-left
        assert_eq!((front[0].x, front[0].y, front[0].width, front[0].height), (0.0, 0.0, 10.0, 20.0));
        assert_eq!(front[0].uv, [0.0, 0.0, 0.5, 0.5]);
        assert_eq!(front[0].texture, 42);

        // Cell (0,1): top-right
        assert_eq!((front[1].x, front[1].y, front[1].width, front[1].height), (10.0, 0.0, 10.0, 20.0));
        assert_eq!(front[1].uv, [0.5, 0.0, 1.0, 0.5]);

        // Cell (1,0): bottom-left
        assert_eq!((front[2].x, front[2].y, front[2].width, front[2].height), (0.0, 20.0, 10.0, 20.0));
        assert_eq!(front[2].uv, [0.0, 0.5, 0.5, 1.0]);

        // Cell (1,1): bottom-right
        assert_eq!((front[3].x, front[3].y, front[3].width, front[3].height), (10.0, 20.0, 10.0, 20.0));
        assert_eq!(front[3].uv, [0.5, 0.5, 1.0, 1.0]);
    }

    #[test]
    fn display_placeholder_quads_truecolor_and_negative_z_index() {
        let size_info = SizeInfo::new(100.0, 100.0, 10.0, 20.0, 0.0, 0.0, true);
        let fg = Color::Spec(VteRgb { r: 10, g: 20, b: 30 });
        let expected_image_id = (10 << 16) | (20 << 8) | 30;

        let mut cells = vec![
            make_placeholder_cell(2, 3, fg, Some(Color::Indexed(5)), Some(vec!['\u{0305}', '\u{0305}'])),
        ];

        let placement = Placement {
            internal_id: 1,
            placement_id: 5,
            anchor: Point::new(Line(2), Column(3)),
            footprint: alacritty_terminal::graphics::placement::CellFootprint { columns: 1, rows: 1 },
            offset_x: 2,
            offset_y: 4,
            columns: 1,
            rows: 1,
            src: alacritty_terminal::graphics::placement::SourceRect { x: 0, y: 0, width: 10, height: 20 },
            z_index: -1,
            is_virtual: true,
            parent: None,
            relative_offset: (0, 0),
        };

        let image = Arc::new(alacritty_terminal::graphics::image::ImageData::new(
            expected_image_id,
            0,
            10,
            20,
            alacritty_terminal::graphics::image::PixelFormat::Rgba,
            Arc::new([0u8; 10 * 20 * 4]),
        ));

        let virtual_placements = vec![(placement, image)];

        let (behind, front) = build_placeholder_quads(
            &mut cells,
            &virtual_placements,
            &size_info,
            |_p, _img| 99,
        );

        assert_eq!(cells[0].character, ' ');
        assert_eq!(front.len(), 0);
        assert_eq!(behind.len(), 1);
        // Cell col 3 * 10 + offset_x 2 = 32. Line 2 * 20 + offset_y 4 = 44.
        assert_eq!((behind[0].x, behind[0].y, behind[0].width, behind[0].height), (32.0, 44.0, 10.0, 20.0));
        assert_eq!(behind[0].uv, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(behind[0].texture, 99);
    }

    #[test]
    fn display_placeholder_quads_aspect_ratio_letterboxing() {
        let size_info = SizeInfo::new(100.0, 100.0, 10.0, 20.0, 0.0, 0.0, true);
        let fg = Color::Indexed(42);

        // 2x2 placeholder cells (box is 20px wide by 40px high)
        // Image is 20x20 square, so it fits to width and centers vertically:
        // centered in 40px box means y_offset = (40 - 20) / 2 = 10px.
        // Image spans y in [10, 30].
        let mut cells = vec![
            make_placeholder_cell(0, 0, fg, None, Some(vec!['\u{0305}', '\u{0305}'])),
            make_placeholder_cell(1, 0, fg, None, Some(vec!['\u{030D}'])),
        ];

        let placement = Placement {
            internal_id: 1,
            placement_id: 0,
            anchor: Point::new(Line(0), Column(0)),
            footprint: alacritty_terminal::graphics::placement::CellFootprint { columns: 2, rows: 2 },
            offset_x: 0,
            offset_y: 0,
            columns: 2,
            rows: 2,
            src: alacritty_terminal::graphics::placement::SourceRect { x: 0, y: 0, width: 20, height: 20 },
            z_index: 0,
            is_virtual: true,
            parent: None,
            relative_offset: (0, 0),
        };

        let image = Arc::new(alacritty_terminal::graphics::image::ImageData::new(
            42,
            0,
            20,
            20,
            alacritty_terminal::graphics::image::PixelFormat::Rgba,
            Arc::new([0u8; 20 * 20 * 4]),
        ));

        let (behind, front) = build_placeholder_quads(
            &mut cells,
            &[(placement, image)],
            &size_info,
            |_p, _img| 1,
        );

        assert!(behind.is_empty());
        assert_eq!(front.len(), 2);

        // Row 0 covers y in [0, 20]. The image spans y in [10, 30].
        // Intersection is y in [10, 20]: quad y = 10, height = 10.
        // Source y in [0, 10]: v0 = 0.0, v1 = 0.5.
        assert_eq!((front[0].x, front[0].y, front[0].width, front[0].height), (0.0, 10.0, 10.0, 10.0));
        assert_eq!(front[0].uv, [0.0, 0.0, 0.5, 0.5]);

        // Row 1 covers y in [20, 40]. The image spans y in [10, 30].
        // Intersection is y in [20, 30]: quad y = 20, height = 10.
        // Source y in [10, 20]: v0 = 0.5, v1 = 1.0.
        assert_eq!((front[1].x, front[1].y, front[1].width, front[1].height), (0.0, 20.0, 10.0, 10.0));
        assert_eq!(front[1].uv, [0.0, 0.5, 0.5, 1.0]);
    }
}
