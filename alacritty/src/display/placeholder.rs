//! Unicode placeholder decoding for the Kitty graphics protocol.
//!
//! Kitty specifies image display via `U+10EEEE` cells with combining
//! diacritical marks indicating row, column, and high byte of image ID.

use alacritty_terminal::vte::ansi::Color;

use crate::display::content::RenderableCell;

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
        Color::Named(named) => named as u8 as u32,
    };

    // Spec: Placement ID is specified using the underline color. If omitted, placement ID is 0.
    let placement_id = match cell.raw_underline {
        None => 0,
        Some(Color::Indexed(idx)) => u32::from(idx),
        Some(Color::Spec(rgb)) => {
            (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
        },
        Some(Color::Named(named)) => named as u8 as u32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Point};
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
}
