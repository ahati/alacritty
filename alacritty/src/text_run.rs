use std::mem;

use alacritty_terminal::index::{Column, Point};
use alacritty_terminal::term::cell::Flags;

use crate::display::color::Rgb;
use crate::display::content::RenderableCell;

/// A run of renderable cells which share all of their rendering properties.
///
/// The assumption is that all cells of a text run can be sent off to be shaped
/// together, which is what makes ligatures possible. Since cells only belong to
/// the same run while they share all their attributes, anything breaking up a
/// ligature (like a selection changing a single cell's style) also breaks up
/// the text run, and thus the ligature.
#[derive(Debug)]
pub struct TextRun {
    /// A run never spans multiple lines.
    pub line: usize,

    /// Span of columns the text run covers.
    pub span: (Column, Column),

    /// Contents of the text run.
    pub content: TextRunContent,

    /// Foreground color of the text run.
    pub fg: Rgb,

    /// Background color of the text run.
    pub bg: Rgb,

    /// Opacity of the text run's background.
    pub bg_alpha: f32,

    /// Underline color of the text run.
    pub underline: Rgb,

    /// Attributes of this text run.
    pub flags: Flags,
}

impl TextRun {
    /// The text of this run, with tabs replaced by spaces to prevent shaping
    /// issues.
    pub fn text(&self) -> String {
        self.content.text.replace('\t', " ")
    }

    /// Number of columns covered by this run.
    pub fn num_columns(&self) -> usize {
        self.span.1.0 - self.span.0.0 + 1
    }

    /// Column of the character at `index` within the run's text.
    ///
    /// Characters inside a [`Flags::WIDE_CHAR`] run occupy two columns.
    pub fn column_of_char(&self, index: usize) -> Column {
        let width = if self.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
        Column(self.span.0.0 + index * width)
    }

    /// Returns a dummy [`RenderableCell`] without content, carrying only the
    /// positioning and color information of this run.
    pub fn dummy_cell_at(&self, column: Column) -> RenderableCell {
        RenderableCell {
            point: Point::new(self.line, column),
            character: ' ',
            extra: None,
            fg: self.fg,
            bg: self.bg,
            bg_alpha: self.bg_alpha,
            underline: self.underline,
            flags: self.flags,
        }
    }

    /// Iterate over each column in the run's span, skipping the spacers of
    /// wide characters.
    ///
    /// The yielded cells are dummies without content; the text of the run is
    /// available through [`TextRun::text`].
    pub fn cells(&self) -> impl Iterator<Item = RenderableCell> + '_ {
        let step = if self.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
        let (Column(start), Column(end)) = self.span;
        (start..=end).step_by(step).map(move |col| self.dummy_cell_at(Column(col)))
    }
}

/// The contents of a [`TextRun`].
#[derive(Default, Debug)]
pub struct TextRunContent {
    /// The text of the run, one character per covered cell.
    pub text: String,

    /// Zero-width characters attached to each character of the run.
    pub zero_widths: Vec<Option<Vec<char>>>,
}

/// Start of a text run.
#[derive(Debug)]
struct RunStart {
    line: usize,
    column: Column,
    fg: Rgb,
    bg: Rgb,
    bg_alpha: f32,
    underline: Rgb,
    flags: Flags,
}

impl RunStart {
    fn new(cell: &RenderableCell) -> Self {
        Self {
            line: cell.point.line,
            column: cell.point.column,
            fg: cell.fg,
            bg: cell.bg,
            bg_alpha: cell.bg_alpha,
            underline: cell.underline,
            flags: cell.flags,
        }
    }

    /// Check if `cell` belongs to the same run.
    #[inline]
    fn belongs_to_text_run(&self, cell: &RenderableCell) -> bool {
        self.line == cell.point.line
            && self.fg == cell.fg
            && self.bg == cell.bg
            && self.underline == cell.underline
            && self.flags == cell.flags
            && (self.bg_alpha - cell.bg_alpha).abs() < f32::EPSILON
    }
}

/// The last column seen while building a run.
#[derive(Debug, Default)]
struct LatestCol {
    column: Column,
    is_wide: bool,
}

impl LatestCol {
    #[inline]
    fn new(cell: &RenderableCell) -> Self {
        Self { column: cell.point.column, is_wide: cell.flags.contains(Flags::WIDE_CHAR) }
    }

    fn width(&self) -> usize {
        if self.is_wide { 2 } else { 1 }
    }
}

/// Wraps an iterator of [`RenderableCell`] and batches them into [`TextRun`]s.
///
/// Cells belong to the same run while their line, adjacency, colors and flags
/// are equal. Since runs are shaped together, this is also the point where
/// ligatures are broken up by styles like selection or the terminal cursor.
pub struct TextRunIter<I> {
    iter: I,
    run_start: RunStart,
    latest_col: LatestCol,
    buffer: TextRunContent,
}

impl<I> TextRunIter<I>
where
    I: Iterator<Item = RenderableCell>,
{
    /// Create a new run iterator over `iter`.
    pub fn new(mut iter: I) -> Self {
        if let Some(cell) = iter.next() {
            let run_start = RunStart::new(&cell);
            let latest_col = LatestCol::new(&cell);
            let buffer = Self::buffer_cell(TextRunContent::default(), &cell);

            Self { iter, run_start, latest_col, buffer }
        } else {
            // There are no cells in the grid. This rarely happens.
            #[cold]
            #[inline]
            fn dummy<I>(iter: I) -> TextRunIter<I> {
                TextRunIter {
                    iter,
                    run_start: RunStart {
                        line: 0,
                        column: Column(0),
                        fg: Rgb::default(),
                        bg: Rgb::default(),
                        bg_alpha: 0.,
                        underline: Rgb::default(),
                        flags: Flags::empty(),
                    },
                    latest_col: LatestCol::default(),
                    buffer: TextRunContent::default(),
                }
            }

            dummy(iter)
        }
    }

    /// Add the contents of `cell` to the run buffer.
    fn buffer_cell(mut buffer: TextRunContent, cell: &RenderableCell) -> TextRunContent {
        buffer.text.push(cell.character);
        let zerowidth = cell.extra.as_ref().and_then(|extra| extra.zerowidth.clone());
        buffer.zero_widths.push(zerowidth);
        buffer
    }

    /// Check whether the current run ends with the incoming `cell`.
    fn is_end_of_run(&self, cell: &RenderableCell) -> bool {
        !self.run_start.belongs_to_text_run(cell)
            || self.latest_col.column + self.latest_col.width() != cell.point.column
    }

    /// Build a [`TextRun`] from its parts.
    fn build_text_run(start: &RunStart, latest: &LatestCol, content: TextRunContent) -> TextRun {
        let end_column = latest.column + if latest.is_wide { 1 } else { 0 };

        TextRun {
            line: start.line,
            span: (start.column, end_column),
            content,
            fg: start.fg,
            bg: start.bg,
            bg_alpha: start.bg_alpha,
            underline: start.underline,
            flags: start.flags,
        }
    }
}

impl<I> Iterator for TextRunIter<I>
where
    I: Iterator<Item = RenderableCell>,
{
    type Item = TextRun;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(cell) = self.iter.next() {
            if self.is_end_of_run(&cell) {
                // If the current run ends here, emit it and start a new one.
                let buffer = mem::take(&mut self.buffer);
                let run = Self::build_text_run(&self.run_start, &self.latest_col, buffer);

                self.run_start = RunStart::new(&cell);
                self.latest_col = LatestCol::new(&cell);
                self.buffer = Self::buffer_cell(TextRunContent::default(), &cell);

                return Some(run);
            }

            // Build up the buffer and track the latest column we've seen.
            self.latest_col = LatestCol::new(&cell);
            let buffer = mem::take(&mut self.buffer);
            self.buffer = Self::buffer_cell(buffer, &cell);
        }

        // Emit any remaining buffered content; the iterator is exhausted
        // afterwards.
        if !self.buffer.text.is_empty() {
            let buffer = mem::take(&mut self.buffer);
            Some(Self::build_text_run(&self.run_start, &self.latest_col, buffer))
        } else {
            None
        }
    }
}
