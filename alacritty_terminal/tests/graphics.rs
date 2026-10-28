//! End-to-end tests for Kitty graphics protocol support.
//!
//! These drive the real VT parser into a real [`Term`], so they cover the
//! parser change that makes APC payloads reachable as well as the protocol
//! handling itself.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    let config = Config { scrolling_history: 4, ..Config::default() };
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

    // RIS takes the placement but keeps the pixels of an image the client can
    // still address by its identifier, as the reference implementation does.
    feed(&mut term, &mut parser, b"\x1bc");

    assert!(term.graphics().placements().is_empty());
    assert_eq!(term.graphics().memory_used(), 4);
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
    let config = Config { scrolling_history: 10, ..Config::default() };
    let mut term = Term::new(config, &TermSize::new(COLUMNS, LINES), recorder);
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, &transmit(1, WHITE_PIXEL));
    assert_eq!(term.graphics().placements().len(), 1);

    // Scroll the image's line into the history, where it survives.
    feed(&mut term, &mut parser, b"\x1b[24;1H\n");
    assert_eq!(term.graphics().placements().len(), 1);

    // Shrinking the history discards that line, and with it the placement.
    let smaller = Config { scrolling_history: 0, ..Config::default() };
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

/// Base64 payload of an opaque raw RGB image of the given pixel size.
fn rgb_payload(width: u32, height: u32) -> String {
    BASE64.encode(vec![0u8; (width * height * 3) as usize])
}

/// Base64 payload of an opaque raw RGBA image of the given pixel size.
fn rgba_payload(width: u32, height: u32) -> String {
    BASE64.encode(vec![0u8; (width * height * 4) as usize])
}

/// Build a complete graphics escape sequence from control data and payload.
fn sequence(control: &str, payload: &str) -> Vec<u8> {
    if payload.is_empty() {
        format!("\x1b_G{control}\x1b\\").into_bytes()
    } else {
        format!("\x1b_G{control};{payload}\x1b\\").into_bytes()
    }
}

/// Send one graphics command and return the responses it produced.
fn command(
    term: &mut Term<Recorder>,
    parser: &mut Processor,
    recorder: &Recorder,
    control: &str,
    payload: &str,
) -> Vec<String> {
    let sent = recorder.pty_writes().len();
    feed(term, parser, &sequence(control, payload));
    recorder.pty_writes().split_off(sent)
}

/// Transmit and display (`a=T`) a raw RGB image, with extra control keys.
fn put(
    term: &mut Term<Recorder>,
    parser: &mut Processor,
    recorder: &Recorder,
    image_id: u32,
    width: u32,
    height: u32,
    extra: &str,
) -> Vec<String> {
    let control = format!("a=T,f=24,s={width},v={height},i={image_id}{extra}");
    command(term, parser, recorder, &control, &rgb_payload(width, height))
}

/// Display an already stored image again (`a=p`), with extra control keys.
fn put_reference(
    term: &mut Term<Recorder>,
    parser: &mut Processor,
    recorder: &Recorder,
    image_id: u32,
    extra: &str,
) -> Vec<String> {
    command(term, parser, recorder, &format!("a=p,i={image_id}{extra}"), "")
}

/// Number of stored images, whether placed or not.
fn image_count(term: &Term<Recorder>) -> usize {
    term.graphics().images().count()
}

/// Client identifiers of the stored images, ascending.
fn image_ids(term: &Term<Recorder>) -> Vec<u32> {
    let mut ids: Vec<u32> = term.graphics().images().map(|(_, image)| image.client_id).collect();
    ids.sort_unstable();
    ids
}

/// Client identifiers the current placements refer to, ascending.
fn placed_image_ids(term: &Term<Recorder>) -> Vec<u32> {
    let mut ids: Vec<u32> = term
        .graphics()
        .placements()
        .into_iter()
        .filter_map(|placement| term.graphics().image(placement.internal_id))
        .map(|image| image.client_id)
        .collect();
    ids.sort_unstable();
    ids
}

/// Index the whole screen `count` times, one line each.
fn scroll_screen(term: &mut Term<Recorder>, parser: &mut Processor, count: usize) {
    for _ in 0..count {
        feed(term, parser, format!("\x1b[{LINES};1H\n").as_bytes());
    }
}

/// Index the scrolling region ending on 1-based row `bottom` `count` times.
fn scroll_margin(term: &mut Term<Recorder>, parser: &mut Processor, bottom: usize, count: usize) {
    feed(term, parser, format!("\x1b[{bottom};1H").as_bytes());
    for _ in 0..count {
        feed(term, parser, b"\n");
    }
}

/// Encode a PNG image with the given pixel layout.
fn png_bytes(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
    }
    output
}

/// Base64 payload of a PNG image with the given pixel layout.
fn png_payload(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> String {
    BASE64.encode(png_bytes(width, height, color, data))
}

#[test]
fn images_outside_a_scroll_region_are_never_scrolled() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // One image at the top of the screen and one at the bottom.
    put(&mut term, &mut parser, &recorder, 1, 10, 20, "");
    feed(&mut term, &mut parser, b"\x1b[24;1H");
    put(&mut term, &mut parser, &recorder, 2, 10, 20, "");
    assert_eq!(term.graphics().placements().len(), 2);

    // A seven-line region (rows 2 to 6, 1-based) that covers neither image.
    feed(&mut term, &mut parser, b"\x1b[2;6r");
    for _ in 0..LINES + 5 {
        scroll_margin(&mut term, &mut parser, 6, 1);
        assert_eq!(term.graphics().placements().len(), 2, "a region scroll must not drop them");
    }

    let mut anchors: Vec<i32> =
        term.graphics().placements().into_iter().map(|p| p.anchor.line.0).collect();
    anchors.sort_unstable();
    assert_eq!(anchors, vec![0, LINES as i32 - 1], "a region scroll must not move them");
}

#[test]
#[ignore = "divergence: the fork pins an image to the top margin, kitty clips it and drops it"]
fn a_placement_scrolled_past_a_margin_is_dropped() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // Rows 2 to 4 (1-based), so lines 1 to 4.
    feed(&mut term, &mut parser, b"\x1b[2;4r");
    feed(&mut term, &mut parser, b"\x1b[2;1H");

    // A two-cell-tall image inside the region, at its top.
    put(&mut term, &mut parser, &recorder, 1, 10, 40, ",z=-1");
    assert_eq!(term.graphics().placements()[0].footprint.rows, 2);

    // Kitty clips the top row on the first two scrolls and drops the placement
    // on the third, because an image may not leave the scrolling region. This
    // fork has no margin clipping: it clamps the anchor to the region start and
    // keeps the placement, so the count never drops.
    scroll_margin(&mut term, &mut parser, 4, 3);
    assert!(
        term.graphics().placements().is_empty(),
        "kitty drops a placement that scrolled past the top margin"
    );
}

#[test]
fn image_data_is_freed_when_its_last_placement_leaves_the_buffer() {
    let recorder = Recorder::default();
    let config = Config { scrolling_history: 4, ..Config::default() };
    let mut term = Term::new(config, &TermSize::new(COLUMNS, LINES), recorder.clone());
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // Anonymous: once nothing refers to it, no client can address it again,
    // so the reference implementation frees it with its last placement. An
    // image with an identifier survives, addressable for a later `a=p`.
    command(&mut term, &mut parser, &recorder, "a=T,f=24,s=10,v=20", &rgb_payload(10, 20));

    // Scrolling the image off the screen keeps it, because its line now lives
    // in the history buffer and scrolling back shows it again.
    scroll_screen(&mut term, &mut parser, 2);
    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(image_count(&term), 1, "its line is still in the history");

    // Once the history has been discarded the reference drops the image data.
    scroll_screen(&mut term, &mut parser, 4);
    assert_eq!(image_count(&term), 0, "kitty frees the data with its last placement");
}

#[test]
fn reset_keeps_images_that_scrolled_into_the_history() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // An image on screen loses its placement to the reset; its pixels stay,
    // because the client can still address the image by its identifier.
    put(&mut term, &mut parser, &recorder, 1, 10, 20, "");
    feed(&mut term, &mut parser, b"\x1bc");
    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_count(&term), 1);

    // An image scrolled into the history survives it, because kitty's reset only
    // removes placements that are still visible on screen.
    put(&mut term, &mut parser, &recorder, 2, 10, 20, "");
    scroll_screen(&mut term, &mut parser, LINES);
    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(term.graphics().placements()[0].anchor.line.0, -(LINES as i32));

    feed(&mut term, &mut parser, b"\x1bc");
    assert_eq!(term.graphics().placements().len(), 1, "only the scrollback placement survives");
    assert_eq!(image_count(&term), 2, "both named images keep their pixels");
}

#[test]
fn delete_selector_a_clears_placements_without_freeing_data() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    put(&mut term, &mut parser, &recorder, 1, 10, 20, "");

    // `a=d` without a selector clears every placement and keeps the data.
    command(&mut term, &mut parser, &recorder, "a=d", "");
    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_count(&term), 1);

    // The capital form frees the data of what it deletes; there is no placement
    // left to delete, so nothing is freed, as in the reference implementation.
    command(&mut term, &mut parser, &recorder, "a=d,d=A", "");
    assert_eq!(image_ids(&term), vec![1]);
}

#[test]
fn delete_selector_capital_a_frees_the_data_of_what_it_deletes() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    put(&mut term, &mut parser, &recorder, 2, 10, 20, "");
    command(&mut term, &mut parser, &recorder, "a=d,d=A", "");

    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_ids(&term), Vec::<u32>::new(), "the placement's image is freed with it");
}

#[test]
fn delete_selector_i_removes_placements_and_capital_frees_data() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    put(&mut term, &mut parser, &recorder, 7, 10, 20, "");

    // A placement id that does not exist leaves both the placement and the data.
    command(&mut term, &mut parser, &recorder, "a=d,d=I,i=7,p=7", "");
    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(image_ids(&term), vec![7]);

    // The lowercase form only removes the placement, so the data stays usable.
    command(&mut term, &mut parser, &recorder, "a=d,d=i,i=7", "");
    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_ids(&term), vec![7]);
    assert_eq!(
        put_reference(&mut term, &mut parser, &recorder, 7, ""),
        vec!["\x1b_Gi=7;OK\x1b\\".to_owned()],
    );

    // The capital form frees the data, and the image is gone for good.
    command(&mut term, &mut parser, &recorder, "a=d,d=I,i=7", "");
    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_count(&term), 0);
    assert_eq!(
        put_reference(&mut term, &mut parser, &recorder, 7, ""),
        vec!["\x1b_Gi=7;ENOENT:image not found\x1b\\".to_owned()],
    );

    // A named placement is deleted on its own, and its image then freed with it.
    put(&mut term, &mut parser, &recorder, 8, 10, 20, ",p=9");
    assert_eq!(term.graphics().placements()[0].placement_id, 9);
    command(&mut term, &mut parser, &recorder, "a=d,d=I,i=8,p=9", "");
    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_count(&term), 0);
}

#[test]
fn delete_selector_r_covers_a_range_of_image_ids() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // Transmitted-only images: the range frees their data.
    for image_id in [11u32, 12] {
        let control = format!("a=t,f=24,s=10,v=20,i={image_id}");
        command(&mut term, &mut parser, &recorder, &control, &rgb_payload(10, 20));
    }
    assert!(term.graphics().placements().is_empty(), "a=t must not display anything");
    assert_eq!(image_count(&term), 2);

    command(&mut term, &mut parser, &recorder, "a=d,d=R,x=11,y=12", "");
    assert_eq!(image_count(&term), 0);

    // Placed images: `y` alone covers every id from zero up to it.
    for image_id in [1u32, 2, 3] {
        put(&mut term, &mut parser, &recorder, image_id, 10, 20, "");
    }
    assert_eq!(term.graphics().placements().len(), 3);

    command(&mut term, &mut parser, &recorder, "a=d,d=R,y=2", "");
    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(image_ids(&term), vec![3]);

    command(&mut term, &mut parser, &recorder, "a=d,d=R,x=3,y=3", "");
    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_count(&term), 0);
}

#[test]
fn delete_at_the_cursor_cell_and_by_z_index() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // Two one-cell images, at (0, 0) and (1, 0), with the cursor now at (2, 0).
    put(&mut term, &mut parser, &recorder, 1, 10, 20, "");
    put(&mut term, &mut parser, &recorder, 2, 10, 20, "");

    // Nothing is under the cursor.
    command(&mut term, &mut parser, &recorder, "a=d,d=C", "");
    assert_eq!(term.graphics().placements().len(), 2);

    // The first image is under the home cell.
    feed(&mut term, &mut parser, b"\x1b[H");
    command(&mut term, &mut parser, &recorder, "a=d,d=C", "");
    assert_eq!(placed_image_ids(&term), vec![2]);

    // A placement carries its stacking order, and `d=Z` selects on it.
    put(&mut term, &mut parser, &recorder, 3, 10, 20, ",z=9");
    assert_eq!(term.graphics().placements().len(), 2);
    command(&mut term, &mut parser, &recorder, "a=d,d=Z,z=9", "");
    assert_eq!(placed_image_ids(&term), vec![2]);
}

#[test]
fn delete_point_column_and_row_selectors_use_one_based_cells() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    put(&mut term, &mut parser, &recorder, 1, 10, 20, "");
    put(&mut term, &mut parser, &recorder, 2, 10, 20, "");

    // `x=1,y=1` is the top left cell, where the first image sits.
    command(&mut term, &mut parser, &recorder, "a=d,d=P,x=1,y=1", "");
    assert_eq!(placed_image_ids(&term), vec![2]);

    // `x=2` is the second column, where the second image sits.
    command(&mut term, &mut parser, &recorder, "a=d,d=X,x=2", "");
    assert!(term.graphics().placements().is_empty());
}

#[test]
fn put_after_delete_shows_the_image_again() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    put(&mut term, &mut parser, &recorder, 999_999, 10, 20, "");
    assert_eq!(
        put_reference(&mut term, &mut parser, &recorder, 999_999, ""),
        vec!["\x1b_Gi=999999;OK\x1b\\".to_owned()],
    );

    // Deleting the placements keeps the data, so it can be put again.
    command(&mut term, &mut parser, &recorder, "a=d,d=i,i=999999", "");
    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_count(&term), 1);
    assert_eq!(
        put_reference(&mut term, &mut parser, &recorder, 999_999, ""),
        vec!["\x1b_Gi=999999;OK\x1b\\".to_owned()],
    );
    assert_eq!(term.graphics().placements().len(), 1);

    // Freeing the data makes the image unreachable.
    command(&mut term, &mut parser, &recorder, "a=d,d=I,i=999999", "");
    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_count(&term), 0);
    assert_eq!(
        put_reference(&mut term, &mut parser, &recorder, 999_999, ""),
        vec!["\x1b_Gi=999999;ENOENT:image not found\x1b\\".to_owned()],
    );
}

#[test]
fn deleting_one_image_keeps_the_other_images_data() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    for image_id in [9_999_999u32, 10_000_000] {
        put(&mut term, &mut parser, &recorder, image_id, 10, 20, "");
        put_reference(&mut term, &mut parser, &recorder, image_id, "");
    }
    assert_eq!(term.graphics().placements().len(), 4);

    // Two placements of one image go together; the other image keeps its data.
    command(&mut term, &mut parser, &recorder, "a=d,d=i,i=9999999", "");
    assert_eq!(placed_image_ids(&term), vec![10_000_000; 2]);
    assert_eq!(image_ids(&term), vec![9_999_999, 10_000_000]);

    command(&mut term, &mut parser, &recorder, "a=d,d=I,i=10000000", "");
    assert!(term.graphics().placements().is_empty());
    assert_eq!(image_ids(&term), vec![9_999_999]);
}

#[test]
fn image_numbers_address_the_newest_image() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A number alone is enough to store and display an image.
    let control = "a=T,f=24,s=10,v=20,I=1";
    assert_eq!(
        command(&mut term, &mut parser, &recorder, control, &rgb_payload(10, 20)),
        vec!["\x1b_Gi=1,I=1;OK\x1b\\".to_owned()],
    );
    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(image_count(&term), 1);

    // Transmitting the number again stores a second image, with its own
    // internal identifier; the number now refers to that newer one.
    let older: Vec<u32> = term.graphics().images().map(|(internal_id, _)| internal_id).collect();
    command(&mut term, &mut parser, &recorder, control, &rgb_payload(10, 20));
    assert_eq!(image_count(&term), 2);

    // A reference by number lands on the newest image.
    assert_eq!(
        put_reference_by_number(&mut term, &mut parser, &recorder, 1),
        vec!["\x1b_GI=1;OK\x1b\\".to_owned()],
    );
    let placements = term.graphics().placements();
    // Both transmissions placed their image, and the reference added a third
    // placement of the newer one.
    assert_eq!(placements.len(), 3);
    let referenced = placements.last().unwrap().internal_id;
    assert!(!older.contains(&referenced), "the number must resolve to the newest image");

    // An unknown number is not found.
    assert_eq!(
        put_reference_by_number(&mut term, &mut parser, &recorder, 94),
        vec!["\x1b_GI=94;ENOENT:image not found\x1b\\".to_owned()],
    );
    assert_eq!(term.graphics().placements().len(), 3);

    // Deleting by number takes the newest image's placements, keeping the
    // data and the older image's placement.
    command(&mut term, &mut parser, &recorder, "a=d,d=i,I=1", "");
    assert_eq!(image_count(&term), 2);
    assert_eq!(term.graphics().placements().len(), 1, "the older image keeps its placement");

    // The capital form frees the data as well.
    command(&mut term, &mut parser, &recorder, "a=d,d=N,I=1", "");
    assert_eq!(image_count(&term), 1, "the older image with that number stays stored");
}

#[test]
fn a_number_only_transmission_gets_a_server_assigned_id() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // Kitty answers with the image id it assigned, and that id addresses the
    // image afterwards.
    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=t,f=24,s=10,v=20,I=1", &rgb_payload(10, 20)),
        vec!["\x1b_Gi=1,I=1;OK\x1b\\".to_owned()],
    );
    let assigned = term.graphics().images().next().unwrap().1.client_id;
    assert_ne!(assigned, 0, "the terminal assigns an id to a numbered image");
    assert_eq!(
        put_reference(&mut term, &mut parser, &recorder, assigned, ""),
        vec![format!("\x1b_Gi={assigned};OK\x1b\\")],
    );
}

#[test]
fn reusing_a_number_keeps_the_older_images_placements() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    let control = "a=T,f=24,s=10,v=20,I=1";
    command(&mut term, &mut parser, &recorder, control, &rgb_payload(10, 20));
    command(&mut term, &mut parser, &recorder, control, &rgb_payload(10, 20));

    assert_eq!(image_count(&term), 2);
    assert_eq!(term.graphics().placements().len(), 2, "kitty keeps both placements");
}

/// Place a stored image by its number (`I`) instead of its id.
fn put_reference_by_number(
    term: &mut Term<Recorder>,
    parser: &mut Processor,
    recorder: &Recorder,
    number: u32,
) -> Vec<String> {
    command(term, parser, recorder, &format!("a=p,I={number}"), "")
}

#[test]
fn chunked_transmission_keeps_its_number() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A 2x2 RGBA image, split across two chunks that only carry `m` in the
    // continuation, so the number comes from the first one.
    let payload = rgba_payload(2, 2);
    let (first, second) = payload.split_at(12);

    let start = command(&mut term, &mut parser, &recorder, "a=t,f=32,s=2,v=2,I=93,m=1", first);
    assert!(start.is_empty(), "a chunk carrying more data is answered at the end");
    assert_eq!(
        command(&mut term, &mut parser, &recorder, "m=0", second),
        vec!["\x1b_Gi=1,I=93;OK\x1b\\".to_owned()],
    );

    assert_eq!(image_count(&term), 1);
    assert_eq!(
        put_reference_by_number(&mut term, &mut parser, &recorder, 93),
        vec!["\x1b_GI=93;OK\x1b\\".to_owned()],
    );
    assert_eq!(term.graphics().placements().len(), 1);
}

#[test]
fn deleting_the_newest_image_number_reveals_the_previous_one() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    let control = "a=t,f=24,s=10,v=20,I=1117";
    command(&mut term, &mut parser, &recorder, control, &rgb_payload(10, 20));
    let first = term.graphics().images().map(|(internal_id, _)| internal_id).min().unwrap();
    command(&mut term, &mut parser, &recorder, control, &rgb_payload(10, 20));
    let second = term.graphics().images().map(|(internal_id, _)| internal_id).max().unwrap();
    assert_ne!(first, second, "each transmission gets its own identifier");

    // Deleting the newest one leaves the older one addressable by the number.
    command(&mut term, &mut parser, &recorder, "a=d,d=N,I=1117", "");
    assert_eq!(
        put_reference_by_number(&mut term, &mut parser, &recorder, 1117),
        vec!["\x1b_GI=1117;OK\x1b\\".to_owned()],
    );
}

#[test]
fn quiet_one_suppresses_success_but_not_errors() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A quiet level of one silences the acknowledgement.
    assert!(put(&mut term, &mut parser, &recorder, 1, 10, 20, ",q=1").is_empty());
    assert_eq!(term.graphics().placements().len(), 1);

    // Failures are still reported.
    let short = BASE64.encode(vec![0u8; 4]);
    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=T,f=24,s=2,v=2,i=2,q=1", &short),
        vec!["\x1b_Gi=2;ENODATA:not enough image data\x1b\\".to_owned()],
    );
    assert_eq!(term.graphics().placements().len(), 1);

    // A quiet level of two silences the failure as well.
    assert!(
        command(&mut term, &mut parser, &recorder, "a=T,f=24,s=2,v=2,i=3,q=2", &short).is_empty()
    );
    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(image_ids(&term), vec![1], "a failed transmission stores nothing");

    // A query is answered through the same path and is silenced by `q=1`.
    let control = "a=q,f=24,s=1,v=1,i=4,q=1";
    assert!(command(&mut term, &mut parser, &recorder, control, &rgb_payload(1, 1)).is_empty());
    assert_eq!(image_count(&term), 1);
}

#[test]
fn quiet_on_the_first_chunk_sticks_to_the_whole_transmission() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    let payload = rgba_payload(2, 2);
    let (first, second) = payload.split_at(12);

    let start = command(&mut term, &mut parser, &recorder, "a=T,f=32,s=2,v=2,i=5,m=1,q=1", first);
    assert!(start.is_empty(), "a chunk carrying more data is answered at the end");
    assert!(
        command(&mut term, &mut parser, &recorder, "m=0", second).is_empty(),
        "kitty answers a quiet transmission only when the first chunk is not quiet"
    );
}

#[test]
fn quota_evicts_the_oldest_image_and_its_placement() {
    let recorder = Recorder::default();
    let mut config = Config::default();
    // Room for exactly two 3x3 RGBA images.
    config.graphics.memory_limit = 2 * 3 * 3 * 4;
    let mut term = Term::new(config, &TermSize::new(COLUMNS, LINES), recorder.clone());
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    let control = |image_id: u32| format!("a=T,f=32,s=3,v=3,i={image_id}");
    for image_id in [1u32, 2] {
        let payload = rgba_payload(3, 3);
        assert_eq!(
            command(&mut term, &mut parser, &recorder, &control(image_id), &payload),
            vec![format!("\x1b_Gi={image_id};OK\x1b\\")],
        );
    }
    assert_eq!(image_count(&term), 2);
    assert_eq!(term.graphics().memory_used(), 2 * 3 * 3 * 4);

    // A third image only fits once the oldest is evicted, together with the
    // placement that referred to it.
    let payload = rgba_payload(3, 3);
    assert_eq!(
        command(&mut term, &mut parser, &recorder, &control(3), &payload),
        vec!["\x1b_Gi=3;OK\x1b\\".to_owned()],
    );
    assert_eq!(image_ids(&term), vec![2, 3]);
    assert_eq!(placed_image_ids(&term), vec![2, 3]);
    assert_eq!(term.graphics().memory_used(), 2 * 3 * 3 * 4);
}

#[test]
fn transient_images_are_evicted_before_older_ones() {
    let recorder = Recorder::default();
    let mut config = Config::default();
    config.graphics.memory_limit = 2 * 3 * 3 * 4;
    let mut term = Term::new(config, &TermSize::new(COLUMNS, LINES), recorder.clone());
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A transient image is loaded after an older, non-transient one.
    let payload = rgba_payload(3, 3);
    command(&mut term, &mut parser, &recorder, "a=T,f=32,s=3,v=3,i=1", &payload);
    command(&mut term, &mut parser, &recorder, "a=T,f=32,s=3,v=3,i=2,N=1", &payload);
    assert_eq!(image_count(&term), 2);

    // The transient one goes first, even though it is newer.
    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=T,f=32,s=3,v=3,i=3", &payload),
        vec!["\x1b_Gi=3;OK\x1b\\".to_owned()],
    );
    assert_eq!(image_ids(&term), vec![1, 3]);
    assert_eq!(placed_image_ids(&term), vec![1, 3]);
}

#[test]
fn quota_counts_a_raw_rgb_image_as_three_bytes_per_pixel() {
    let recorder = Recorder::default();
    let mut config = Config::default();
    // Room for exactly two 3x3 RGB images, as in the reference test.
    config.graphics.memory_limit = 2 * 3 * 3 * 3;
    let mut term = Term::new(config, &TermSize::new(COLUMNS, LINES), recorder.clone());
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    let payload = rgb_payload(3, 3);
    for image_id in [1u32, 2] {
        let control = format!("a=T,f=24,s=3,v=3,i={image_id}");
        assert_eq!(
            command(&mut term, &mut parser, &recorder, &control, &payload),
            vec![format!("\x1b_Gi={image_id};OK\x1b\\")],
        );
    }
    assert_eq!(term.graphics().memory_used(), 2 * 3 * 3 * 3);

    // Only the oldest of the two has to go to make room for a third.
    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=T,f=24,s=3,v=3,i=3", &payload),
        vec!["\x1b_Gi=3;OK\x1b\\".to_owned()],
    );
    assert_eq!(image_ids(&term), vec![2, 3]);
}

#[test]
fn put_records_the_source_rect_and_pixel_offsets() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A one-cell image at the top left, which moves the cursor past it.
    put(&mut term, &mut parser, &recorder, 1, 10, 20, "");
    assert_eq!(term.grid().cursor.point.column.0, 1);

    // A second placement of the same image, with a source rectangle, pixel
    // offsets, a stacking order and its own placement id.
    let control = "a=p,i=1,c=80,r=1,x=2,y=1,w=3,h=5,X=3,Y=1,z=-1,p=17";
    assert_eq!(
        command(&mut term, &mut parser, &recorder, control, ""),
        vec!["\x1b_Gi=1,p=17;OK\x1b\\".to_owned()],
    );

    assert_eq!(term.graphics().placements().len(), 2);
    let second = term
        .graphics()
        .placements()
        .into_iter()
        .find(|placement| placement.placement_id == 17)
        .expect("the new placement must exist");

    assert_eq!((second.src.x, second.src.y, second.src.width, second.src.height), (2, 1, 3, 5));
    assert_eq!((second.offset_x, second.offset_y), (3, 1));
    assert_eq!(second.z_index, -1);
    assert_eq!((second.footprint.columns, second.footprint.rows), (80, 1));
    // Anchored where the cursor was, one cell past the first image.
    assert_eq!((second.anchor.line.0, second.anchor.column.0), (0, 1));

    // Placing 80 columns from column one wraps the cursor onto the next line.
    assert_eq!(term.grid().cursor.point.line.0, 1);
    assert_eq!(term.grid().cursor.point.column.0, 0);
}

#[test]
fn putting_the_same_placement_id_moves_the_placement() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    put(&mut term, &mut parser, &recorder, 1, 10, 20, "");
    put_reference(&mut term, &mut parser, &recorder, 1, ",p=5");
    assert_eq!(term.graphics().placements().len(), 2);

    // Re-putting the same image and placement id replaces it in place.
    feed(&mut term, &mut parser, b"\x1b[5;5H");
    put_reference(&mut term, &mut parser, &recorder, 1, ",p=5");
    let placements = term.graphics().placements();
    assert_eq!(placements.len(), 2);
    let moved = placements
        .into_iter()
        .find(|placement| placement.placement_id == 5)
        .expect("the named placement must exist");
    assert_eq!((moved.anchor.line.0, moved.anchor.column.0), (4, 4));

    // Anonymous placements of the same image coexist instead.
    put_reference(&mut term, &mut parser, &recorder, 1, "");
    assert_eq!(term.graphics().placements().len(), 3);
    put_reference(&mut term, &mut parser, &recorder, 1, "");
    assert_eq!(term.graphics().placements().len(), 4);
}

#[test]
fn put_with_pixel_offsets_records_them() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A 10x20 image pushed 5px right and down covers 2x2 cells, and leaves the
    // cursor on the second of them.
    put(&mut term, &mut parser, &recorder, 1, 10, 20, ",X=5,Y=5");

    let placement = term.graphics().placements()[0];
    assert_eq!((placement.offset_x, placement.offset_y), (5, 5));
    assert_eq!((placement.footprint.columns, placement.footprint.rows), (2, 2));
    assert_eq!((term.grid().cursor.point.column.0, term.grid().cursor.point.line.0), (2, 1));
}

#[test]
fn placements_record_their_z_index() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    put(&mut term, &mut parser, &recorder, 1, 10, 20, "");
    for placement_id in [2u32, 3, 4] {
        let z = if placement_id > 2 { ",z=-2" } else { "" };
        let extra = format!(",c=2,r=1,p={placement_id}{z}");
        put_reference(&mut term, &mut parser, &recorder, 1, &extra);
    }
    put(&mut term, &mut parser, &recorder, 2, 10, 20, ",z=-1");
    assert_eq!(term.graphics().placements().len(), 5);

    // Back to front: by stacking order, then by image and placement identity.
    let ordered: Vec<(i32, u32)> = term
        .graphics()
        .placements()
        .into_iter()
        .map(|placement| (placement.z_index, placement.placement_id))
        .collect();
    assert_eq!(ordered, vec![(-2, 3), (-2, 4), (-1, 0), (0, 0), (0, 2)]);
}

#[test]
fn virtual_placements_are_flagged_and_pinned_to_their_cells() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // `U=1` backs Unicode placeholder cells: four columns wide, two rows tall.
    put(&mut term, &mut parser, &recorder, 42, 20, 20, ",U=1,c=4,r=2");

    let placements = term.graphics().placements();
    assert_eq!(placements.len(), 1, "a virtual placement is still recorded");
    let placement = placements[0];
    assert!(placement.is_virtual, "the renderer skips it rather than drawing it");
    assert_eq!((placement.columns, placement.rows), (4, 2));
    assert_eq!((placement.footprint.columns, placement.footprint.rows), (4, 2));

    // It backs cells rather than being drawn at the cursor, which stays put.
    assert_eq!((term.grid().cursor.point.line.0, term.grid().cursor.point.column.0), (0, 0));

    // A reset clears it with everything else.
    feed(&mut term, &mut parser, b"\x1bc");
    assert!(term.graphics().placements().is_empty());
}

#[test]
fn virtual_placements_do_not_scroll_with_the_text() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    put(&mut term, &mut parser, &recorder, 42, 20, 20, ",U=1,c=4,r=2");
    scroll_screen(&mut term, &mut parser, 3);

    let placements = term.graphics().placements();
    assert_eq!(placements.len(), 1, "scrolling never removes a virtual placement");
    assert_eq!(placements[0].anchor.line.0, 0, "a virtual placement must not move");
}

#[test]
fn png_payloads_report_their_own_dimensions() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // RGBA, RGB and greyscale sources all report the size from the header, and
    // are charged for the layout they are stored in.
    let cases = [
        (1u32, png::ColorType::Rgba, 4usize, 4 * 5 * 3),
        (2, png::ColorType::Rgb, 3, 3 * 5 * 3),
        (3, png::ColorType::Grayscale, 1, 3 * 5 * 3),
    ];
    for (image_id, color, channels, charge) in cases {
        let data = vec![7u8; 5 * 3 * channels];
        let payload = png_payload(5, 3, color, &data);
        let control = format!("a=T,f=100,i={image_id}");
        assert_eq!(
            command(&mut term, &mut parser, &recorder, &control, &payload),
            vec![format!("\x1b_Gi={image_id};OK\x1b\\")],
        );

        let image = term
            .graphics()
            .images()
            .map(|(_, image)| image)
            .find(|image| image.client_id == image_id)
            .expect("the image must be stored");
        assert_eq!((image.width, image.height), (5, 3));
        assert_eq!(image.charge(), charge);
    }

    assert_eq!(term.graphics().placements().len(), 3);
    assert_eq!(term.graphics().memory_used(), 4 * 5 * 3 + 2 * 3 * 5 * 3);
}

#[test]
fn rejects_unreadable_png_payloads() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // Neither random bytes nor a PNG whose trailing chunks were cut off are
    // accepted, and a rejected payload stores nothing.
    let garbage = BASE64.encode(b"xxxxxxxxxxxxxxxxxxxxxxxxx");
    let truncated = {
        let full = png_bytes(3, 3, png::ColorType::Rgba, &[7u8; 3 * 3 * 4]);
        BASE64.encode(&full[..full.len() - 16])
    };

    for (image_id, payload) in [(1u32, garbage), (2, truncated)] {
        let control = format!("a=T,f=100,i={image_id}");
        assert_eq!(
            command(&mut term, &mut parser, &recorder, &control, &payload),
            vec![format!("\x1b_Gi={image_id};EBADPNG:could not decode image data\x1b\\")],
        );
        assert!(term.graphics().placements().is_empty());
        assert_eq!(term.graphics().memory_used(), 0);
    }
}

#[test]
#[ignore = "benchmark: run explicitly with --release -- --ignored --nocapture"]
fn bench_real_stream_phases() {
    let data = match std::fs::read("/tmp/icat_bytes.bin") {
        Ok(data) => data,
        Err(_) => return,
    };
    let recorder = Recorder::default();
    let mut term = Term::new(Config::default(), &TermSize::new(COLUMNS, 2 * LINES - 8), recorder);
    let mut parser: Processor = Processor::new();
    term.set_cell_size(11, 23);

    let t0 = std::time::Instant::now();
    let mut parse = std::time::Duration::ZERO;
    let mut decode = std::time::Duration::ZERO;
    for chunk in data.chunks(4096) {
        let p0 = std::time::Instant::now();
        parser.advance(&mut term, chunk);
        parse += p0.elapsed();
        let d0 = std::time::Instant::now();
        settle(&mut term);
        decode += d0.elapsed();
    }
    eprintln!(
        "stream={}MB parse={parse:?} decode={decode:?} total={:?}",
        data.len() / 1_000_000,
        t0.elapsed()
    );
}

/// Raw RGB bytes whose pixels are all distinct.
///
/// The file-medium tests read a window out of a larger buffer, so the pixels
/// have to differ from the surrounding garbage for a wrong offset or size to
/// show up in the stored image.
fn rgb_bytes(width: u32, height: u32) -> Vec<u8> {
    (0..width * height * 3).map(|byte| byte as u8).collect()
}

/// The base64 payload a client sends to name `path` for a file medium.
///
/// `kitten icat` base64-encodes the path and the parser decodes it again before
/// the file is opened, so a path travels exactly like image data. The decoded
/// payload carries no NUL terminator.
fn path_payload(path: &Path) -> String {
    BASE64.encode(path.as_os_str().as_encoded_bytes())
}

/// A temporary file that removes itself when the guard is dropped.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    /// Path of the file, as a client would name it in a `t=f` command.
    fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the file is still on disk.
    fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Base64 payload naming this file.
    fn payload(&self) -> String {
        path_payload(&self.path)
    }

    /// Number of bytes the path occupies once the payload is decoded.
    fn payload_len(&self) -> usize {
        self.path.as_os_str().as_encoded_bytes().len()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Write `bytes` to a fresh file under the system temporary directory.
///
/// The reference implementation only removes a `t=t` file whose path contains
/// `tty-graphics-protocol`, so `name` selects whether the path is marked for
/// deletion.
fn temp_file(name: &str, bytes: &[u8]) -> TempFile {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir()
        .join(format!("alacritty-graphics-{}-{unique}-{name}", std::process::id()));
    std::fs::write(&path, bytes).expect("failed to write the temporary test file");
    TempFile { path }
}

/// Dimensions and pixels of the stored image with the given client id.
fn stored_image(term: &Term<Recorder>, client_id: u32) -> (u32, u32, Vec<u8>) {
    let image = term
        .graphics()
        .images()
        .map(|(_, image)| image)
        .find(|image| image.client_id == client_id)
        .expect("the image must be stored");
    (image.width, image.height, image.pixels.to_vec())
}

#[test]
fn reads_a_raw_image_from_a_file() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // `kitten icat` can name a file (the `f` medium) instead of sending the
    // pixels inline, leaving `S` out so the whole file is read. The file
    // belongs to the client, so the terminal never deletes a `t=f` file, even
    // one it is allowed to clean up.
    let pixels = rgb_bytes(2, 2);
    let file = temp_file("tty-graphics-protocol-raw.bin", &pixels);
    let control = "a=T,f=24,s=2,v=2,i=1,t=f";

    assert_eq!(
        command(&mut term, &mut parser, &recorder, control, &file.payload()),
        vec!["\x1b_Gi=1;OK\x1b\\".to_owned()],
    );

    assert_eq!(term.graphics().placements().len(), 1);
    assert_eq!(stored_image(&term, 1), (2, 2, pixels));
    assert!(file.exists(), "a `t=f` file must not be deleted by the terminal");
}

#[test]
fn a_file_transmission_with_only_a_number_is_assigned_an_id() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A number-only transmission is answered with the identifier the terminal
    // assigned, exactly as for an inline payload.
    let pixels = rgb_bytes(2, 2);
    let file = temp_file("tty-graphics-protocol-number.bin", &pixels);
    let control = format!("a=T,f=24,s=2,v=2,I=1,t=f,S={}", file.payload_len());

    let replies = command(&mut term, &mut parser, &recorder, &control, &file.payload());
    let assigned = term.graphics().images().next().expect("the image must be stored").1.client_id;
    assert_ne!(assigned, 0, "the terminal assigns an id to a numbered image");
    assert_eq!(replies, vec![format!("\x1b_Gi={assigned},I=1;OK\x1b\\")]);
    assert_eq!(stored_image(&term, assigned).2, pixels);
}

#[test]
fn reads_a_png_image_from_a_file() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A PNG names its own dimensions, so `S` may be left out and the whole
    // file is read; the header supplies the size.
    let pixels = vec![7u8; 5 * 3 * 3];
    let png = png_bytes(5, 3, png::ColorType::Rgb, &pixels);
    let file = temp_file("tty-graphics-protocol-png.bin", &png);

    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=T,f=100,i=2,t=f", &file.payload()),
        vec!["\x1b_Gi=2;OK\x1b\\".to_owned()],
    );

    assert_eq!(stored_image(&term, 2), (5, 3, pixels));
}

#[test]
fn reads_a_compressed_image_from_a_temporary_file() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // The reference test writes a zlib-compressed payload to a marked temp
    // file, transmits it with `t=t,o=z`, and expects the file to be gone.
    let pixels = rgb_bytes(2, 2);
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&pixels, 6);
    let file = temp_file("tty-graphics-protocol-zlib.bin", &compressed);
    let control = format!("a=T,f=24,s=2,v=2,i=3,t=t,o=z,S={}", compressed.len());

    assert_eq!(
        command(&mut term, &mut parser, &recorder, &control, &file.payload()),
        vec!["\x1b_Gi=3;OK\x1b\\".to_owned()],
    );

    assert_eq!(stored_image(&term, 3).2, pixels);
    assert!(!file.exists(), "a `t=t` file is removed once it has been read");
}

#[test]
fn removes_a_marked_temporary_file_after_reading_it() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // The `t` medium asks the terminal to delete the file after reading it.
    let pixels = rgb_bytes(2, 2);
    let file = temp_file("tty-graphics-protocol-temp.bin", &pixels);
    let control = format!("a=T,f=24,s=2,v=2,i=4,t=t,S={}", pixels.len());

    assert_eq!(
        command(&mut term, &mut parser, &recorder, &control, &file.payload()),
        vec!["\x1b_Gi=4;OK\x1b\\".to_owned()],
    );

    assert_eq!(stored_image(&term, 4).2, pixels);
    assert!(!file.exists(), "the terminal must delete the temporary file");
}

#[test]
fn keeps_a_temporary_file_whose_path_is_not_marked() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // Only files whose path contains `tty-graphics-protocol` are removed, so an
    // unmarked `t=t` path keeps the terminal from deleting an arbitrary file.
    let pixels = rgb_bytes(2, 2);
    let file = temp_file("unmarked.bin", &pixels);
    let control = format!("a=T,f=24,s=2,v=2,i=5,t=t,S={}", pixels.len());

    assert_eq!(
        command(&mut term, &mut parser, &recorder, &control, &file.payload()),
        vec!["\x1b_Gi=5;OK\x1b\\".to_owned()],
    );

    assert_eq!(stored_image(&term, 5).2, pixels);
    assert!(file.exists(), "an unmarked `t=t` path must not be deleted");
}

#[test]
fn removes_a_marked_temporary_file_even_when_loading_fails() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // The reference unlinks an opened `t=t` file whether or not its contents
    // turned out to be a usable image, so a broken transfer leaves nothing
    // behind to accumulate.
    let file = temp_file("tty-graphics-protocol-short.bin", &[1, 2, 3, 4, 5, 6, 7]);
    let control = "a=T,f=24,s=2,v=2,i=6,t=t,S=12";

    assert_eq!(
        command(&mut term, &mut parser, &recorder, control, &file.payload()),
        vec!["\x1b_Gi=6;EBADF:Failed to read image file\x1b\\".to_owned()],
    );

    assert!(!file.exists(), "an opened `t=t` file is removed even on failure");
    assert_eq!(image_count(&term), 0);
}

#[test]
fn reads_a_file_window_at_the_given_offset() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // The reference test frames the image with garbage and reads it with a non
    // page aligned offset; both the leading and trailing bytes must be skipped.
    let pixels = rgb_bytes(2, 2);
    let mut contents = vec![0xee; 3];
    contents.extend_from_slice(&pixels);
    contents.extend_from_slice(&[0xdd; 5]);
    let file = temp_file("tty-graphics-protocol-window.bin", &contents);
    let control = format!("a=T,f=24,s=2,v=2,i=7,t=f,S={},O=3", pixels.len());

    assert_eq!(
        command(&mut term, &mut parser, &recorder, &control, &file.payload()),
        vec!["\x1b_Gi=7;OK\x1b\\".to_owned()],
    );

    assert_eq!(stored_image(&term, 7).2, pixels);
}

#[test]
fn the_offset_and_size_pick_one_image_out_of_several() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // `S` bounds how much is read, so together with `O` it selects a single
    // image out of a file holding several back to back.
    let first = rgb_bytes(2, 2);
    let mut second = rgb_bytes(2, 2);
    for byte in &mut second {
        *byte ^= 0xff;
    }
    let mut contents = first.clone();
    contents.extend_from_slice(&second);
    let file = temp_file("tty-graphics-protocol-packed.bin", &contents);
    let control = format!("a=T,f=24,s=2,v=2,i=8,t=f,S={},O={}", second.len(), first.len());

    assert_eq!(
        command(&mut term, &mut parser, &recorder, &control, &file.payload()),
        vec!["\x1b_Gi=8;OK\x1b\\".to_owned()],
    );

    assert_eq!(stored_image(&term, 8).2, second);
}

#[test]
fn a_declared_size_smaller_than_the_image_is_reported_as_unreadable() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // `S` limits the read, so a size below what the declared pixels need is a
    // short read even though the file itself holds a whole image.
    let pixels = rgb_bytes(2, 2);
    let mut contents = pixels.clone();
    contents.extend_from_slice(&[0xab; 10]);
    let file = temp_file("tty-graphics-protocol-partial.bin", &contents);

    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=T,f=24,s=2,v=2,i=9,t=f,S=6", &file.payload()),
        vec!["\x1b_Gi=9;EBADF:Failed to read image file\x1b\\".to_owned()],
    );

    assert_eq!(image_count(&term), 0);
}

#[test]
fn a_short_file_without_a_declared_size_is_reported_as_unreadable() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // With no `S` the file's own length is read, and a file too short for the
    // declared pixels is still a generic failure rather than an error that
    // leaks how large it was.
    let file = temp_file("tty-graphics-protocol-truncated.bin", &[1, 2, 3, 4, 5, 6, 7]);

    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=T,f=24,s=2,v=2,i=17,t=f", &file.payload()),
        vec!["\x1b_Gi=17;EBADF:Failed to read image file\x1b\\".to_owned()],
    );

    assert_eq!(image_count(&term), 0);
}

#[test]
fn every_unreadable_file_reports_the_same_generic_error() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // The reference reports one identical error for a missing file, a
    // directory and a file that is too short, so a client cannot use the reply
    // to probe the filesystem for existence, type or size.
    let missing = temp_file("tty-graphics-protocol-gone.bin", b"");
    std::fs::remove_file(missing.path()).expect("failed to remove the temporary test file");
    let too_short = temp_file("tty-graphics-protocol-small.bin", &[1, 2, 3, 4, 5, 6, 7]);
    let directory = std::env::temp_dir();

    for (image_id, path) in
        [(10u32, missing.path()), (11, directory.as_path()), (12, too_short.path())]
    {
        let control = format!("a=T,f=24,s=2,v=2,i={image_id},t=f,S=12");
        assert_eq!(
            command(&mut term, &mut parser, &recorder, &control, &path_payload(path)),
            vec![format!("\x1b_Gi={image_id};EBADF:Failed to read image file\x1b\\")],
            "unexpected reply for path {path:?}",
        );
    }

    assert_eq!(image_count(&term), 0);
}

#[test]
fn an_offset_past_the_end_of_a_file_reports_the_generic_error() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // Past the end there is nothing left to read, so the command fails the same
    // way whether or not the client declared a size.
    let file = temp_file("tty-graphics-protocol-past-end.bin", &rgb_bytes(2, 2));

    assert_eq!(
        command(
            &mut term,
            &mut parser,
            &recorder,
            "a=T,f=24,s=2,v=2,i=13,t=f,O=4096",
            &file.payload()
        ),
        vec!["\x1b_Gi=13;EBADF:Failed to read image file\x1b\\".to_owned()],
    );
    assert_eq!(
        command(
            &mut term,
            &mut parser,
            &recorder,
            "a=T,f=24,s=2,v=2,i=14,t=f,S=12,O=4096",
            &file.payload(),
        ),
        vec!["\x1b_Gi=14;EBADF:Failed to read image file\x1b\\".to_owned()],
    );

    assert_eq!(image_count(&term), 0);
}

#[test]
fn rejects_a_filename_longer_than_two_kilobytes() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    // A payload naming a file is refused once it decodes to more than 2048
    // bytes, before the path is interpreted at all.
    let too_long = "a".repeat(2049);
    assert_eq!(
        command(
            &mut term,
            &mut parser,
            &recorder,
            "a=T,f=24,s=2,v=2,i=15,t=f",
            &path_payload(Path::new(&too_long)),
        ),
        vec!["\x1b_Gi=15;EINVAL:Filename too long\x1b\\".to_owned()],
    );

    // Exactly 2048 bytes is not too long, so the command proceeds and fails
    // only because no such file exists.
    let boundary = "b".repeat(2048);
    assert_eq!(
        command(
            &mut term,
            &mut parser,
            &recorder,
            "a=T,f=24,s=2,v=2,i=16,t=f",
            &path_payload(Path::new(&boundary)),
        ),
        vec!["\x1b_Gi=16;EBADF:Failed to read image file\x1b\\".to_owned()],
    );

    assert_eq!(image_count(&term), 0);
}

#[test]
fn a_query_without_an_identifier_reads_nothing_and_answers_nothing() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    let file = temp_file("tty-graphics-protocol-query", &[1, 2, 3]);
    let payload = path_payload(&file.path);

    // No `i=` and no `I=`: the reference logs the command, reads no file and
    // sends no response. A number does not qualify either; only an id does.
    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=q,f=24,s=1,v=1,t=t", &payload),
        Vec::<String>::new()
    );
    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=q,f=24,s=1,v=1,t=t,I=9", &payload),
        Vec::<String>::new()
    );
    assert!(file.exists(), "the file was never read, so it is not removed");
    assert_eq!(image_count(&term), 0);
}

#[test]
fn a_zero_dimension_image_is_rejected_before_any_file_is_read() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    let file = temp_file("tty-graphics-protocol-zero", &[1, 2, 3]);
    let payload = path_payload(&file.path);

    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=T,f=24,s=0,v=1,t=t,i=1", &payload),
        vec!["\x1b_Gi=1;EINVAL:Zero width/height not allowed\x1b\\".to_owned()],
    );
    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=T,f=24,s=20001,v=1,t=t,i=2", &payload),
        vec![
            "\x1b_Gi=2;EINVAL:Image too large, width or height greater than 10000\x1b\\".to_owned()
        ],
    );
    assert!(file.exists(), "the dimension checks precede the read");
}

#[test]
fn compressed_file_data_inflating_short_is_its_own_error() {
    // A zlib stream that inflates to 6 of the 12 bytes a 2x2 raw image needs:
    // the file was read completely, so the reference reports the inflation
    // mismatch rather than an unreadable file.
    let compressed: [u8; 11] = [120, 156, 99, 96, 0, 1, 0, 0, 6, 0, 1];

    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    term.set_cell_size(10, 20);

    let file = temp_file("tty-graphics-protocol-short-inflate", &compressed);
    let payload = path_payload(&file.path);

    assert_eq!(
        command(&mut term, &mut parser, &recorder, "a=T,f=24,s=2,v=2,t=t,i=80,o=z", &payload),
        vec![
            "\x1b_Gi=80;EINVAL:Image data size post inflation does not match expected size\x1b\\"
                .to_owned()
        ],
    );
    assert!(!file.exists(), "a marked temporary file is removed once opened");
}
