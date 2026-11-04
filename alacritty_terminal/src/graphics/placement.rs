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
    /// Parent placement if this is a relative placement (parent_internal_id, parent_placement_id).
    pub parent: Option<(u32, u32)>,
    /// Relative offset from parent anchor in cells (H, V).
    pub relative_offset: (i32, i32),
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
            let existing_idx = self.placements.iter().position(|existing| {
                existing.internal_id == placement.internal_id
                    && existing.placement_id == placement.placement_id
            });
            if let Some(idx) = existing_idx {
                let old_anchor = self.placements[idx].anchor;
                self.placements[idx] = placement;
                let delta_line = self.placements[idx].anchor.line.0 - old_anchor.line.0;
                let delta_col = self.placements[idx].anchor.column.0 as i32 - old_anchor.column.0 as i32;
                if delta_line != 0 || delta_col != 0 {
                    let parent_id = self.placements[idx].internal_id;
                    let parent_plid = self.placements[idx].placement_id;
                    self.shift_children(parent_id, parent_plid, delta_line, delta_col);
                }
                return;
            }
        }

        self.placements.push(placement);
    }

    fn shift_children(&mut self, parent_id: u32, parent_placement_id: u32, delta_line: i32, delta_col: i32) {
        for i in 0..self.placements.len() {
            if let Some((pid, plid)) = self.placements[i].parent {
                if pid == parent_id && (plid == parent_placement_id || plid == 0) {
                    let old_line = self.placements[i].anchor.line.0;
                    let old_col = self.placements[i].anchor.column.0 as i32;
                    self.placements[i].anchor.line = Line(old_line + delta_line);
                    self.placements[i].anchor.column = Column((old_col + delta_col).max(0) as usize);
                    let child_id = self.placements[i].internal_id;
                    let child_plid = self.placements[i].placement_id;
                    self.shift_children(child_id, child_plid, delta_line, delta_col);
                }
            }
        }
    }

    /// Prune any relative placement whose parent no longer exists.
    /// Runs iteratively until all transitive orphans are removed.
    pub fn prune_orphans(&mut self) {
        loop {
            let initial_len = self.placements.len();
            let parent_keys: std::collections::HashSet<(u32, u32)> = self
                .placements
                .iter()
                .map(|p| (p.internal_id, p.placement_id))
                .collect();
            self.placements.retain(|placement| {
                if let Some((parent_id, parent_placement_id)) = placement.parent {
                    if parent_placement_id == 0 {
                        parent_keys.iter().any(|(pid, _)| *pid == parent_id)
                    } else {
                        parent_keys.contains(&(parent_id, parent_placement_id))
                    }
                } else {
                    true
                }
            });
            if self.placements.len() == initial_len {
                break;
            }
        }
    }

    /// Find a placement by internal image id and placement id. If placement_id is 0,
    /// returns the first placement for internal_id.
    pub fn find(&self, internal_id: u32, placement_id: u32) -> Option<&Placement> {
        self.placements.iter().find(|p| {
            p.internal_id == internal_id
                && (placement_id == 0 || p.placement_id == placement_id)
        })
    }

    /// Remove every placement of one image, and any relative children.
    pub fn remove_image(&mut self, internal_id: u32) {
        self.placements.retain(|placement| placement.internal_id != internal_id);
        self.prune_orphans();
    }

    /// Remove a single placement of an image, and any relative children.
    pub fn remove_placement(&mut self, internal_id: u32, placement_id: u32) {
        self.placements.retain(|placement| {
            !(placement.internal_id == internal_id && placement.placement_id == placement_id)
        });
        self.prune_orphans();
    }

    /// Remove every placement covering `point`.
    pub fn remove_at(&mut self, point: Point) {
        self.placements.retain(|placement| placement.is_virtual || !placement.contains(point));
        self.prune_orphans();
    }

    /// Remove every placement with the given stacking order.
    pub fn remove_with_z(&mut self, z_index: i32) {
        self.placements.retain(|placement| placement.is_virtual || placement.z_index != z_index);
        self.prune_orphans();
    }

    /// Remove every placement covering `column`.
    pub fn remove_column(&mut self, column: Column) {
        self.placements.retain(|placement| {
            placement.is_virtual || {
                let start = placement.anchor.column.0;
                let end = start + usize::from(placement.columns.max(1));
                !(start..end).contains(&column.0)
            }
        });
        self.prune_orphans();
    }

    /// Remove every placement covering `line`.
    pub fn remove_row(&mut self, line: Line) {
        self.placements.retain(|placement| {
            placement.is_virtual || {
                let start = placement.anchor.line;
                let end = start + i32::from(placement.rows.max(1));
                !(start..end).contains(&line)
            }
        });
        self.prune_orphans();
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

        self.placements.retain_mut(|placement| {
            // A virtual placement backs Unicode placeholder cells rather than
            // being anchored to the grid, so scrolling never moves it and only
            // an explicit delete or a reset removes it, as in the reference.
            if placement.is_virtual {
                return true;
            }

            let line = placement.anchor.line;
            if !(carries_scrollback || line >= region.start) || line >= region.end {
                return true;
            }

            let new_line = Line(placement.anchor.line.0 - delta);

            // With page margins in use, an image may not leave the region; the
            // protocol requires it to be clipped instead, or dropped once scrolled past.
            if !carries_scrollback {
                let bottom_line = Line(new_line.0 + i32::from(placement.footprint.rows));
                if bottom_line <= region.start || new_line >= region.end {
                    return false;
                }

                if new_line < region.start {
                    let clipped_rows = (region.start.0 - new_line.0) as u16;
                    if clipped_rows >= placement.footprint.rows {
                        return false;
                    }
                    let total_rows = placement.footprint.rows.max(1);
                    placement.footprint.rows = placement.footprint.rows.saturating_sub(clipped_rows);
                    if placement.rows > 0 {
                        placement.rows = placement.rows.saturating_sub(clipped_rows);
                    }
                    let frac = clipped_rows as f32 / total_rows as f32;
                    let clip_src_y = (placement.src.height as f32 * frac).round() as u32;
                    placement.src.y = placement.src.y.saturating_add(clip_src_y);
                    placement.src.height = placement.src.height.saturating_sub(clip_src_y);
                    placement.anchor.line = region.start;
                    return true;
                }

                if bottom_line > region.end {
                    let clipped_rows = (bottom_line.0 - region.end.0) as u16;
                    if clipped_rows >= placement.footprint.rows {
                        return false;
                    }
                    let total_rows = placement.footprint.rows.max(1);
                    placement.footprint.rows = placement.footprint.rows.saturating_sub(clipped_rows);
                    if placement.rows > 0 {
                        placement.rows = placement.rows.saturating_sub(clipped_rows);
                    }
                    let frac = clipped_rows as f32 / total_rows as f32;
                    let clip_src_y = (placement.src.height as f32 * frac).round() as u32;
                    placement.src.height = placement.src.height.saturating_sub(clip_src_y);
                }
            }

            placement.anchor.line = new_line;
            true
        });

        self.prune_orphans();
    }

    /// Pull pixel offsets back inside a cell.
    ///
    /// An offset is measured inside the cell a placement is anchored to, so a
    /// cell that has just changed size can leave it outside it. The reference
    /// implementation clamps on its equivalent of a font size change.
    pub fn clamp_offsets(&mut self, cell: (u16, u16)) {
        let (width, height) = cell;

        for placement in &mut self.placements {
            placement.offset_x = placement.offset_x.min(width.saturating_sub(1));
            placement.offset_y = placement.offset_y.min(height.saturating_sub(1));
        }
    }

    /// Drop placements that no longer overlap the given line range.
    ///
    /// A placement is measured by its whole extent rather than by its anchor,
    /// because one whose anchor has scrolled above the top of the buffer is
    /// still on screen while any of its rows remain. The reference
    /// implementation keeps a placement until its bottom edge leaves the
    /// buffer, which is what makes an image taller than the screen survive.
    pub fn retain_within(&mut self, top: Line, bottom: Line) {
        self.placements.retain(|placement| {
            // Virtual placements are pinned to their cells by the client, so
            // buffer bounds do not apply to them.
            if placement.is_virtual {
                return true;
            }

            let start = placement.anchor.line;
            let end = Line(start.0 + i32::from(placement.footprint.rows));

            start <= bottom && end > top
        });
    }

    /// Drop the placements that are on the screen, keeping the ones that live
    /// entirely in the scrollback.
    ///
    /// Screen clears and resets take only what is visible, as the reference
    /// implementation's `clear_filter_func` does: a placement whose bottom
    /// edge is at or above the top of the buffer belongs to history.
    pub fn retain_scrollback(&mut self) {
        self.placements.retain(|placement| {
            let end = Line(placement.anchor.line.0 + i32::from(placement.footprint.rows));
            end <= Line(0)
        });
    }

    /// Remove visible placements for `d=a/A`, leaving scrollback and virtual placements.
    pub fn delete_all_visible(&mut self) {
        self.placements.retain(|placement| {
            placement.is_virtual || {
                let end = Line(placement.anchor.line.0 + i32::from(placement.footprint.rows));
                end <= Line(0)
            }
        });
        self.prune_orphans();
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
            parent: None,
            relative_offset: (0, 0),
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
    fn a_placement_outlives_the_viewport() {
        let mut store = PlacementStore::new();
        store.put(placement(1, 0, 1));

        // Still partly on screen after scrolling up by two.
        store.scroll(&(Line(0)..Line(24)), 2);
        assert_eq!(store.len(), 1);
        assert_eq!(store.iter().next().unwrap().anchor.line, Line(-1));

        // Now entirely above the top of the screen. It is kept, because the line
        // it is anchored to is still in the scrollback and scrolling back has to
        // bring the image into view again.
        store.scroll(&(Line(0)..Line(24)), 2);
        assert_eq!(store.len(), 1);
        assert_eq!(store.iter().next().unwrap().anchor.line, Line(-3));

        // It goes once its bottom edge leaves the buffer. A placement taller
        // than the buffer keeps overlapping the visible range, so it outlives
        // one that fits entirely above it.
        store.retain_within(Line(-1), Line(23));
        assert!(store.is_empty());
    }

    #[test]
    fn a_cell_size_change_pulls_offsets_back_inside_a_cell() {
        let mut store = PlacementStore::new();
        let mut image = placement(0, 0, 1);
        image.offset_x = 40;
        image.offset_y = 30;
        store.put(image);

        store.clamp_offsets((12, 20));

        let placement = store.iter().next().unwrap();
        assert_eq!(placement.offset_x, 11);
        assert_eq!(placement.offset_y, 19);
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
