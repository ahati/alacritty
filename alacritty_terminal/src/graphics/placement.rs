//! Grid-anchored placements of images.
//!
//! A placement records the cell it was put at, so that it scrolls with the text
//! the way kitty's protocol requires. The scroll transform deliberately mirrors
//! [`Selection::rotate`](crate::selection::Selection::rotate), which solves the
//! same problem for text selections.

use std::ops::Range;

use crate::index::{Column, Line, Point};

/// Source rectangle within an image, in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Size of a placement in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellFootprint {
    pub columns: u16,
    pub rows: u16,
}

/// A placement of an image, anchored to the cell it was put at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Internal identifier of the image being placed.
    pub internal_id: u32,
    /// Client placement identifier, or `0` when anonymous.
    pub placement_id: u32,
    /// Cell the placement is anchored to, in absolute grid coordinates.
    pub anchor: Point,
    /// Resolved size in cells, used for scrolling and cursor movement.
    pub footprint: CellFootprint,
    /// Horizontal offset inside the anchor cell, in pixels.
    pub offset_x: u16,
    /// Vertical offset inside the anchor cell, in pixels.
    pub offset_y: u16,
    /// Number of columns the placement occupies.
    pub columns: u16,
    /// Number of rows the placement occupies.
    pub rows: u16,
    /// Region of the source image to display.
    pub src: SourceRect,
    /// Stacking order relative to text and other images.
    pub z_index: i32,
    /// Virtual placements back Unicode placeholder cells and are never drawn
    /// directly.
    pub is_virtual: bool,
}

impl Placement {
    /// Whether this placement covers `point`.
    pub fn contains(&self, point: Point) -> bool {
        let line = point.line.0 - self.anchor.line.0;
        if line < 0 || line >= i32::from(self.rows.max(1)) {
            return false;
        }

        let column = point.column.0 as i64 - self.anchor.column.0 as i64;
        column >= 0 && column < i64::from(self.columns.max(1))
    }
}

/// The set of placements for one screen.
#[derive(Debug, Default)]
pub struct PlacementStore {
    placements: Vec<Placement>,
}

impl PlacementStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tracked placements.
    pub fn len(&self) -> usize {
        self.placements.len()
    }

    /// Whether no placements are tracked.
    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Iterate over all placements.
    pub fn iter(&self) -> impl Iterator<Item = &Placement> {
        self.placements.iter()
    }

    /// Whether any placement refers to `internal_id`.
    pub fn is_referenced(&self, internal_id: u32) -> bool {
        self.placements.iter().any(|placement| placement.internal_id == internal_id)
    }

    /// Add or replace a placement.
    ///
    /// Re-putting the same image and placement identifier replaces in place,
    /// which is how clients move an image without flicker.
    pub fn put(&mut self, placement: Placement) {
        // Anonymous placements (`p=0`) of the same image coexist, while a named
        // placement replaces the previous one with the same identity.
        if placement.placement_id != 0 {
            let existing = self.placements.iter_mut().find(|existing| {
                existing.internal_id == placement.internal_id
                    && existing.placement_id == placement.placement_id
            });
            if let Some(existing) = existing {
                *existing = placement;
                return;
            }
        }

        self.placements.push(placement);
    }

    /// Remove every placement of one image.
    pub fn remove_image(&mut self, internal_id: u32) {
        self.placements.retain(|placement| placement.internal_id != internal_id);
    }

    /// Remove a single placement of an image.
    pub fn remove_placement(&mut self, internal_id: u32, placement_id: u32) {
        self.placements.retain(|placement| {
            !(placement.internal_id == internal_id && placement.placement_id == placement_id)
        });
    }

    /// Remove every placement covering `point`.
    pub fn remove_at(&mut self, point: Point) {
        self.placements.retain(|placement| !placement.contains(point));
    }

    /// Remove every placement with the given stacking order.
    pub fn remove_with_z(&mut self, z_index: i32) {
        self.placements.retain(|placement| placement.z_index != z_index);
    }

    /// Remove every placement covering `column`.
    pub fn remove_column(&mut self, column: Column) {
        self.placements.retain(|placement| {
            let start = placement.anchor.column.0;
            let end = start + usize::from(placement.columns.max(1));
            !(start..end).contains(&column.0)
        });
    }

    /// Remove every placement covering `line`.
    pub fn remove_row(&mut self, line: Line) {
        self.placements.retain(|placement| {
            let start = placement.anchor.line;
            let end = start + i32::from(placement.rows.max(1));
            !(start..end).contains(&line)
        });
    }

    /// Shift placements covered by a scrolling region.
    ///
    /// `delta` is positive when text moves up (scrolling forwards), matching
    /// [`Selection::rotate`](crate::selection::Selection::rotate).
    pub fn scroll(&mut self, region: &Range<Line>, delta: i32) {
        if delta == 0 {
            return;
        }

        // A region starting at the top of the screen also carries scrollback
        // lines with it, exactly as `Selection::rotate` assumes.
        let carries_scrollback = region.start == Line(0);

        for placement in &mut self.placements {
            let line = placement.anchor.line;
            if !(carries_scrollback || line >= region.start) || line >= region.end {
                continue;
            }

            placement.anchor.line = Line(placement.anchor.line.0 - delta);

            // With page margins in use, an image may not leave the region; the
            // protocol requires it to be clipped instead.
            if !carries_scrollback && placement.anchor.line < region.start {
                placement.anchor.line = region.start;
            }
        }

        // The reference implementation destroys a placement once it has
        // scrolled entirely off the top of the screen, even though the image
        // data survives in the scrollback. Keeping them would pile up images
        // that can never come back into view, and would keep their data
        // permanently un-evictable.
        self.placements.retain(|placement| {
            let bottom = placement.anchor.line.0 + i32::from(placement.footprint.rows);
            bottom > 0
        });
    }

    /// Drop placements anchored outside the given line range.
    pub fn retain_within(&mut self, top: Line, bottom: Line) {
        self.placements.retain(|placement| {
            let line = placement.anchor.line;
            line >= top && line <= bottom
        });
    }

    /// Remove every placement.
    pub fn clear(&mut self) {
        self.placements.clear();
    }

    /// Placements ordered back to front, matching the protocol's tie-breaking
    /// rules.
    pub fn sorted(&self) -> Vec<&Placement> {
        let mut placements: Vec<&Placement> = self.placements.iter().collect();
        placements.sort_by_key(|placement| {
            (placement.z_index, placement.internal_id, placement.placement_id)
        });
        placements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(line: i32, column: usize, internal_id: u32) -> Placement {
        Placement {
            internal_id,
            placement_id: 0,
            anchor: Point::new(Line(line), Column(column)),
            footprint: CellFootprint { columns: 2, rows: 2 },
            offset_x: 0,
            offset_y: 0,
            columns: 2,
            rows: 2,
            src: SourceRect::default(),
            z_index: 0,
            is_virtual: false,
        }
    }

    #[test]
    fn re_put_replaces_in_place() {
        let mut store = PlacementStore::new();
        let mut first = placement(0, 0, 1);
        first.placement_id = 4;
        store.put(first);

        let mut moved = placement(5, 5, 1);
        moved.placement_id = 4;
        store.put(moved.clone());

        assert_eq!(store.len(), 1);
        assert_eq!(store.iter().next().unwrap().anchor, moved.anchor);
    }

    #[test]
    fn anonymous_placements_coexist() {
        let mut store = PlacementStore::new();
        store.put(placement(0, 0, 1));
        store.put(placement(3, 3, 1));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn scroll_moves_anchored_placements() {
        let mut store = PlacementStore::new();
        store.put(placement(5, 0, 1));
        store.put(placement(20, 0, 2));

        // Scroll the whole screen up by 3 lines.
        store.scroll(&(Line(0)..Line(24)), 3);

        assert_eq!(store.iter().next().unwrap().anchor.line, Line(2));
        assert_eq!(store.iter().nth(1).unwrap().anchor.line, Line(17));
    }

    #[test]
    fn scroll_ignores_lines_outside_region() {
        let mut store = PlacementStore::new();
        store.put(placement(1, 0, 1));
        store.put(placement(10, 0, 2));

        // A region that does not start at the top leaves scrollback alone.
        store.scroll(&(Line(5)..Line(20)), 2);

        assert_eq!(store.iter().next().unwrap().anchor.line, Line(1));
        assert_eq!(store.iter().nth(1).unwrap().anchor.line, Line(8));
    }

    #[test]
    fn contains_covers_occupied_cells() {
        let placement = placement(4, 3, 1);
        assert!(placement.contains(Point::new(Line(4), Column(3))));
        assert!(placement.contains(Point::new(Line(5), Column(4))));
        assert!(!placement.contains(Point::new(Line(6), Column(3))));
        assert!(!placement.contains(Point::new(Line(4), Column(5))));
        assert!(!placement.contains(Point::new(Line(3), Column(3))));
    }

    #[test]
    fn a_placement_scrolled_off_the_screen_is_destroyed() {
        let mut store = PlacementStore::new();
        store.put(placement(1, 0, 1));

        // Still partly on screen after scrolling up by two.
        store.scroll(&(Line(0)..Line(24)), 2);
        assert_eq!(store.len(), 1);
        assert_eq!(store.iter().next().unwrap().anchor.line, Line(-1));

        // Now entirely above the top of the screen.
        store.scroll(&(Line(0)..Line(24)), 2);
        assert_eq!(store.len(), 0, "the reference implementation destroys it");
    }

    #[test]
    fn sorted_orders_by_z_then_identity() {
        let mut store = PlacementStore::new();
        let mut high = placement(0, 0, 2);
        high.z_index = 5;
        let mut low = placement(0, 0, 1);
        low.z_index = -5;
        store.put(high);
        store.put(low);

        let sorted = store.sorted();
        assert_eq!(sorted[0].z_index, -5);
        assert_eq!(sorted[1].z_index, 5);
    }
}
