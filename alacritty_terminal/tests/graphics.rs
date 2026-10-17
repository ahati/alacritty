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
