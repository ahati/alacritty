//! End-to-end tests for Kitty graphics protocol support.
//!
//! These drive the real VT parser into a real [`Term`], so they cover the
//! parser change that makes APC payloads reachable as well as the protocol
//! handling itself.

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

/// Encoded payload of a single opaque white pixel.
const WHITE_PIXEL: &str = "/////w==";

/// Terminal size used by every test.
const COLUMNS: usize = 80;
const LINES: usize = 24;

/// Records the events a terminal emits.
#[derive(Clone, Default)]
struct Recorder {
    events: Arc<Mutex<Vec<Event>>>,
}

impl Recorder {
    /// Every PTY write the terminal requested.
    fn pty_writes(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::PtyWrite(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

impl EventListener for Recorder {
    fn send_event(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

/// Build a terminal together with its event recorder.
fn terminal() -> (Term<Recorder>, Recorder) {
    let recorder = Recorder::default();
    let size = TermSize::new(COLUMNS, LINES);
    let term = Term::new(Config::default(), &size, recorder.clone());
    (term, recorder)
}

/// Feed raw bytes through the real parser, then run any queued decode.
///
/// Decoding is deferred so it can happen with the terminal unlocked; the event
/// loop does it immediately afterwards, which is what these tests settle here.
fn feed(term: &mut Term<Recorder>, parser: &mut Processor, bytes: &[u8]) {
    parser.advance(term, bytes);
    settle(term);
}

/// Run the decodes the parser queued and apply their outcomes.
fn settle(term: &mut Term<Recorder>) {
    for job in term.take_graphics_jobs() {
        term.install_graphics(job.decode());
    }
}

/// Build a complete `a=T` graphics escape sequence.
fn transmit(image_id: u32, payload: &str) -> Vec<u8> {
    format!("\x1b_Gf=32,s=1,v=1,a=T,i={image_id};{payload}\x1b\\").into_bytes()
}

#[test]
fn apc_payload_reaches_the_terminal() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, &transmit(7, WHITE_PIXEL));

    // The image was stored and placed at the cursor.
    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(term.graphics().memory_used(), 4);

    // And the client was acknowledged.
    assert_eq!(recorder.pty_writes(), vec!["\x1b_Gi=7;OK\x1b\\".to_owned()]);
}

#[test]
fn payload_split_across_parser_calls_reassembles() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    let sequence = transmit(9, WHITE_PIXEL);
    for byte in &sequence {
        feed(&mut term, &mut parser, &[*byte]);
    }

    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(recorder.pty_writes(), vec!["\x1b_Gi=9;OK\x1b\\".to_owned()]);
}

#[test]
fn chunked_transmission_reassembles_across_apc_strings() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    // Two chunks: the first declares the image, the second completes it.
    let first = format!("\x1b_Gf=32,s=1,v=1,a=T,i=4,m=1;{}\x1b\\", &WHITE_PIXEL[..4]);
    let second = format!("\x1b_Gm=0;{}\x1b\\", &WHITE_PIXEL[4..]);

    feed(&mut term, &mut parser, first.as_bytes());
    assert!(term.graphics().placements().is_empty());

    feed(&mut term, &mut parser, second.as_bytes());

    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(recorder.pty_writes(), vec!["\x1b_Gi=4;OK\x1b\\".to_owned()]);
}

#[test]
fn graphics_scroll_with_the_text() {
    let (mut term, _recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A 20x30 image covers 2x2 cells, so it stays partly visible after one line
    // of scrolling.
    let pixels = vec![0u8; 20 * 30 * 3];
    let sequence = format!("\x1b_Gf=24,s=20,v=30,a=T,i=1;{}\x1b\\", BASE64.encode(&pixels));
    feed(&mut term, &mut parser, sequence.as_bytes());
    let anchor = term.graphics().placements()[0].anchor;

    feed(&mut term, &mut parser, b"\x1b[24;1H\n");

    let moved = term.graphics().placements()[0].anchor;
    assert_eq!(moved.line, anchor.line - 1);
}

#[test]
fn a_placement_outlives_the_screen_but_not_the_buffer() {
    let recorder = Recorder::default();
    let mut config = Config::default();
    config.scrolling_history = 4;
    let mut term = Term::new(config, &TermSize::new(COLUMNS, LINES), recorder);
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, &transmit(1, WHITE_PIXEL));
    assert_eq!(term.graphics().placements().len(), 1);

    // Scrolling the image off the *screen* keeps it, because the line it is
    // anchored to is still in the scrollback and scrolling back shows it again.
    feed(&mut term, &mut parser, b"\x1b[24;1H\n");
    assert_eq!(term.graphics().placements().len(), 1);

    // It goes once that line is discarded from the buffer.
    for _ in 0..8 {
        feed(&mut term, &mut parser, b"\x1b[24;1H\n");
    }
    assert!(
        term.graphics().placements().is_empty(),
        "a placement whose line left the buffer must be dropped"
    );
}

#[test]
fn a_column_resize_keeps_placements() {
    let (mut term, _recorder) = terminal();
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, &transmit(1, WHITE_PIXEL));
    assert_eq!(term.graphics().placements().len(), 1);
    let used = term.graphics().memory_used();
    assert!(used > 0);

    term.resize(TermSize::new(COLUMNS - 20, LINES));
    assert_eq!(term.graphics().placements().len(), 1, "narrower must keep the image");

    term.resize(TermSize::new(COLUMNS + 20, LINES));
    assert_eq!(term.graphics().placements().len(), 1, "wider must keep the image");

    // Widening far enough to wrap the text must not disturb it either.
    term.resize(TermSize::new(COLUMNS / 4, LINES));
    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(term.graphics().memory_used(), used, "the pixels must survive too");
}

#[test]
fn non_graphics_apc_is_ignored() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    // An unrelated APC must not be mistaken for a graphics command, and text
    // after it must still be printed.
    feed(&mut term, &mut parser, b"\x1b_Xnot graphics\x1b\\hello");

    assert!(term.graphics().placements().is_empty());
    assert!(recorder.pty_writes().is_empty());

    let cell =
        &term.grid()[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(0)];
    assert_eq!(cell.c, 'h');
}

#[test]
fn query_is_answered_without_storing() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    let query = format!("\x1b_Gf=32,s=1,v=1,a=q,i=31;{WHITE_PIXEL}\x1b\\");
    feed(&mut term, &mut parser, query.as_bytes());

    assert_eq!(recorder.pty_writes(), vec!["\x1b_Gi=31;OK\x1b\\".to_owned()]);
    assert!(term.graphics().placements().is_empty());
    assert_eq!(term.graphics().memory_used(), 0);
}

#[test]
fn malformed_graphics_command_is_dropped_silently() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b_Gf=32,bogus=1,i=3;/////w==\x1b\\");

    assert!(term.graphics().placements().is_empty());
    assert!(recorder.pty_writes().is_empty());
}

/// Encode a base64 PNG of a single opaque pixel, the format real clients use.
fn png_pixel() -> String {
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[10, 20, 30, 255]).unwrap();
    }

    BASE64.encode(&png)
}

#[test]
fn transmits_a_png_image() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    // This is the shape of the command emitted by icat, timg and chafa: a PNG
    // direct transmission with no explicit source dimensions.
    let sequence = format!("\x1b_Gf=100,a=T,i=12;{}\x1b\\", png_pixel());
    feed(&mut term, &mut parser, sequence.as_bytes());

    assert_eq!(recorder.pty_writes(), vec!["\x1b_Gi=12;OK\x1b\\".to_owned()]);
    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(term.graphics().memory_used(), 4);
}

#[test]
fn placing_an_image_moves_the_cursor_past_it() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A 30x40 image covers 3x2 cells at a 10x20 cell size.
    let pixels = vec![0u8; 30 * 40 * 3];
    let sequence = format!("\x1b_Gf=24,s=30,v=40,a=T,i=1;{}\x1b\\", BASE64.encode(&pixels));

    let before = term.grid().cursor.point;
    feed(&mut term, &mut parser, sequence.as_bytes());
    let after = term.grid().cursor.point;

    assert_eq!(after.column.0, before.column.0 + 3);
    assert_eq!(after.line, before.line + 1);
    assert_eq!(recorder.pty_writes(), vec!["\x1b_Gi=1;OK\x1b\\".to_owned()]);
}

#[test]
fn pinning_the_cursor_keeps_it_in_place() {
    let (mut term, _recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    let pixels = vec![0u8; 30 * 40 * 3];
    let sequence = format!("\x1b_Gf=24,s=30,v=40,a=T,i=1,C=1;{}\x1b\\", BASE64.encode(&pixels));

    let before = term.grid().cursor.point;
    feed(&mut term, &mut parser, sequence.as_bytes());

    assert_eq!(term.grid().cursor.point, before);
}

#[test]
fn reset_clears_graphics() {
    let (mut term, _recorder) = terminal();
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, &transmit(5, WHITE_PIXEL));
    assert_eq!(term.graphics().placements().len(), 1);

    // RIS.
    feed(&mut term, &mut parser, b"\x1bc");

    assert!(term.graphics().placements().is_empty());
    assert_eq!(term.graphics().memory_used(), 0);
}

/// Transmit an image the way a viewer or gallery does when it positions the
/// cursor itself: auto-sized, with no trailing newline to absorb the
/// cursor-advance discrepancy.
fn back_to_back(term: &mut Term<Recorder>, parser: &mut Processor, id: u32, w: u32, h: u32) {
    let header = format!("\x1b_Ga=T,f=24,s={w},v={h},i={id},q=2;");
    feed(term, parser, header.as_bytes());
    feed(term, parser, BASE64.encode(vec![0u8; (w * h * 3) as usize]).as_bytes());
    feed(term, parser, b"\x1b\\");
}

#[test]
fn no_newline_placements_overlap_by_exactly_one_row() {
    let (mut term, _recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // The reference implementation moves the cursor onto the last row of the
    // image (`rows - 1`), so a client that does not print a newline afterwards
    // overlaps the previous image by that one row. This pins the contract: the
    // overlap must be exactly one row, never more, and the client is expected
    // to emit the newline, as `kitten icat` does.
    for (id, w, h) in [(1u32, 200u32, 100u32), (2, 100, 200), (3, 200, 100)] {
        back_to_back(&mut term, &mut parser, id, w, h);
    }

    let placements = term.graphics().placements();
    let (first, second, third) = (placements[0], placements[1], placements[2]);

    assert_eq!((first.anchor.line.0, first.footprint.rows), (0, 5));
    assert_eq!((second.anchor.line.0, second.footprint.rows), (4, 10));
    assert_eq!((third.anchor.line.0, third.footprint.rows), (13, 5));

    // Each anchor sits one row above the previous bottom edge: kitty-identical.
    assert_eq!(second.anchor.line.0, first.anchor.line.0 + 5 - 1);
    assert_eq!(third.anchor.line.0, second.anchor.line.0 + 10 - 1);

    // The cursor stays on the bottom row of the last image, where the
    // reference implementation leaves it.
    assert_eq!(term.grid().cursor.point.line.0, third.anchor.line.0 + 5 - 1);
}

#[test]
fn column_advance_wraps_to_the_next_line() {
    let (mut term, _recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // An image 20 columns wide anchored at column 70 ends past the last
    // column, so the cursor must wrap to the start of the next line rather
    // than sticking to the right edge, as in the reference implementation.
    feed(&mut term, &mut parser, b"\x1b[1;71H");
    let before = term.grid().cursor.point;

    let pixels = vec![0u8; 200 * 100 * 3];
    let sequence = format!("\x1b_Gf=24,s=200,v=100,a=T,i=1,q=2;{}\x1b\\", BASE64.encode(&pixels));
    feed(&mut term, &mut parser, sequence.as_bytes());

    // Five rows tall, so the row advance reaches line 4, and the wrap adds one.
    assert_eq!(term.grid().cursor.point.column.0, 0, "must wrap to column 0");
    assert_eq!(term.grid().cursor.point.line.0, before.line.0 + 4 + 1);

    // The placement itself is anchored where the cursor was, unclipped.
    let placement = term.graphics().placements()[0];
    assert_eq!((placement.anchor.line.0, placement.anchor.column.0), (0, 70));
}

#[test]
fn cursor_advance_respects_the_scroll_region() {
    let (mut term, _recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // Mark the first line so a scroll of the region cannot have moved it.
    feed(&mut term, &mut parser, b"top\r\n");

    // A five-line region: lines 2..6.
    feed(&mut term, &mut parser, b"\x1b[3;7r");
    feed(&mut term, &mut parser, b"\x1b[3;1H");

    // Ten rows tall: the row advance alone would reach line 11, but the region
    // ends at line 6, so the excess must scroll the region and pin the cursor
    // to the region's last line.
    let pixels = vec![0u8; 100 * 200 * 3];
    let sequence = format!("\x1b_Gf=24,s=100,v=200,a=T,i=1,q=2;{}\x1b\\", BASE64.encode(&pixels));
    feed(&mut term, &mut parser, sequence.as_bytes());

    assert_eq!(term.grid().cursor.point.line.0, 6, "cursor must pin to the region end");

    let cell =
        &term.grid()[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(0)];
    assert_eq!(cell.c, 't', "a region scroll must not touch lines above it");
}

#[test]
fn config_history_shrink_prunes_stale_placements() {
    let recorder = Recorder::default();
    let mut config = Config::default();
    config.scrolling_history = 10;
    let mut term = Term::new(config, &TermSize::new(COLUMNS, LINES), recorder);
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, &transmit(1, WHITE_PIXEL));
    assert_eq!(term.graphics().placements().len(), 1);

    // Scroll the image's line into the history, where it survives.
    feed(&mut term, &mut parser, b"\x1b[24;1H\n");
    assert_eq!(term.graphics().placements().len(), 1);

    // Shrinking the history discards that line, and with it the placement.
    let mut smaller = Config::default();
    smaller.scrolling_history = 0;
    term.set_options(smaller);

    assert!(
        term.graphics().placements().is_empty(),
        "a placement whose line the config shrank away must be dropped"
    );
}

/// Transmit an image exactly as `kitten icat` does: auto-sized (`c`/`r` unset),
/// then a CR LF afterwards.
fn icat_style(term: &mut Term<Recorder>, parser: &mut Processor, id: u32, w: u32, h: u32) {
    let header = format!("\x1b_Ga=T,f=24,s={w},v={h},i={id},q=2;");
    feed(term, parser, header.as_bytes());
    feed(term, parser, BASE64.encode(vec![0u8; (w * h * 3) as usize]).as_bytes());
    feed(term, parser, b"\x1b\\");
    feed(term, parser, b"\r\n");
}

#[test]
fn consecutive_icat_images_do_not_overlap() {
    let (mut term, _recorder) = terminal();
    let mut parser = Processor::new();

    // The core has no font metrics of its own; the display reports these.
    term.set_cell_size(10, 20);

    // Landscape, portrait, landscape, portrait: the mix that was reported.
    for (id, w, h) in [(1u32, 200u32, 100u32), (2, 100, 200), (3, 200, 100), (4, 100, 200)] {
        icat_style(&mut term, &mut parser, id, w, h);
    }

    let mut placements: Vec<_> = term.graphics().placements().into_iter().cloned().collect();
    placements.sort_by_key(|p| p.anchor.line.0);

    for pair in placements.windows(2) {
        let (first, second) = (&pair[0], &pair[1]);
        let first_bottom = first.anchor.line.0 + i32::from(first.footprint.rows);
        assert!(
            second.anchor.line.0 >= first_bottom,
            "image {id1} at line {a1} with {r1} rows overlaps image {id2} at line {a2}",
            id1 = first.internal_id,
            a1 = first.anchor.line.0,
            r1 = first.footprint.rows,
            id2 = second.internal_id,
            a2 = second.anchor.line.0,
        );
    }

    // The cursor must end below the last image, or the prompt lands inside it.
    let last = placements.last().unwrap();
    let last_bottom = last.anchor.line.0 + i32::from(last.footprint.rows);
    assert!(
        term.grid().cursor.point.line.0 >= last_bottom,
        "cursor at line {} is inside the last image (bottom {last_bottom})",
        term.grid().cursor.point.line.0,
    );
}

#[test]
fn full_no_newline_sequence_keeps_every_placement() {
    let recorder = Recorder::default();
    let mut term = Term::new(Config::default(), &TermSize::new(COLUMNS, 2 * LINES - 8), recorder);
    let mut parser = Processor::new();
    term.set_cell_size(11, 23);

    // The whole reported sequence: twelve images mixing landscape and
    // portrait, back to back with no newline, on an 80x40 screen.
    let mut order = Vec::new();
    for n in 1..=6u32 {
        order.push((2 * n - 1, 200u32, 100u32));
        order.push((2 * n, 100u32, 200u32));
    }
    for (id, w, h) in order {
        back_to_back(&mut term, &mut parser, id, w, h);
    }

    let placements = term.graphics().placements();
    assert_eq!(placements.len(), 12, "every placement must survive: {:?}", placements);

    // The last portrait is anchored at column 77, where the columns have
    // accumulated to, so it overhangs the right edge like in the reference.
    // Its column advance then wraps, leaving the cursor one row below the
    // image, reset to column zero, at the screen's last line.
    let last = placements.last().unwrap();
    let bottom_row = last.anchor.line.0 + i32::from(last.footprint.rows) - 1;
    assert_eq!(last.anchor.column.0, 77, "columns accumulate without a newline");
    assert_eq!(term.grid().cursor.point.line.0, bottom_row + 1);
    assert_eq!(term.grid().cursor.point.column.0, 0, "the wrap resets the column");
}

#[test]
fn tall_images_at_the_bottom_do_not_overlap() {
    let recorder = Recorder::default();
    let mut term = Term::new(Config::default(), &TermSize::new(COLUMNS, 2 * LINES - 8), recorder);
    let mut parser = Processor::new();
    term.set_cell_size(11, 23);

    // Photos scaled to the window width are far taller than the screen: 72
    // rows on 40 lines. Placed back to back with icat's CR LF, each one has
    // to scroll by more than a screenful, which `scroll_up` clamps to the
    // region height. Before that clamp was worked around, every image after
    // the cursor first reached the bottom line under-scrolled and the next
    // one was anchored on top of it.
    for (id, w, h) in [(1u32, 880u32, 1632u32), (2, 880, 494), (3, 880, 1632)] {
        back_to_back(&mut term, &mut parser, id, w, h);
        feed(&mut term, &mut parser, b"\r\n");
    }

    let placements = term.graphics().placements();
    assert_eq!(placements.len(), 3);

    // No placement reaches into the one below it, and the last one ends
    // inside the screen rather than hanging below it.
    for pair in placements.windows(2) {
        let (first, second) = (pair[0], pair[1]);
        let first_bottom = first.anchor.line.0 + i32::from(first.footprint.rows) - 1;
        assert!(
            second.anchor.line.0 > first_bottom,
            "image at {} with {} rows overlaps the one at {}",
            first.anchor.line.0,
            first.footprint.rows,
            second.anchor.line.0,
        );
    }
    let last = placements.last().unwrap();
    let last_bottom = last.anchor.line.0 + i32::from(last.footprint.rows) - 1;
    assert!(
        last_bottom < (2 * LINES - 8) as i32,
        "last image hangs below the screen: {last_bottom}"
    );

    // The cursor waits below the last image for the client's next output.
    assert_eq!(term.grid().cursor.point.line.0, (2 * LINES - 9) as i32);
}
