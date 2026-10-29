//! End-to-end tests for kitty clipboard protocol support.
//!
//! These drive the real VT parser into a real [`Term`], covering the OSC
//! dispatch the protocol rides on, the packet handling, the private mode,
//! and the replies the terminal writes back.

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use alacritty_terminal::clipboard::{KittyClipboardConfig, Permission, StoreOutcome};
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{ClipboardType, Config, Term};
use alacritty_terminal::vte::ansi::Processor;

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

    /// The single event matching a pattern.
    fn get<R>(&self, pattern: impl Fn(&Event) -> Option<R>) -> Option<R> {
        self.events.lock().unwrap().iter().filter_map(pattern).next()
    }
}

impl EventListener for Recorder {
    fn send_event(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

/// Build a terminal with a clipboard config, together with its recorder.
fn terminal(kitty_clipboard: KittyClipboardConfig) -> (Term<Recorder>, Recorder) {
    let recorder = Recorder::default();
    let size = TermSize::new(80, 24);
    let config = Config { kitty_clipboard, ..Config::default() };
    let term = Term::new(config, &size, recorder.clone());
    (term, recorder)
}

/// The default config, with every operation permitted.
fn permissive() -> KittyClipboardConfig {
    KittyClipboardConfig {
        read: Permission::Allow,
        write: Permission::Allow,
        ..KittyClipboardConfig::default()
    }
}

/// Feed raw bytes through the real parser.
fn feed(term: &mut Term<Recorder>, parser: &mut Processor, bytes: &(impl AsRef<[u8]> + ?Sized)) {
    parser.advance(term, bytes.as_ref());
}

/// Encode text as the protocol payloads it.
fn payload(text: &str) -> String {
    BASE64.encode(text)
}

#[test]
fn listing_is_answered_without_the_front_end() {
    let (mut term, recorder) = terminal(permissive());
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b]5522;type=read;Lg==\x1b\\");

    let reply = &recorder.pty_writes()[0];
    assert!(reply.starts_with("\x1b]5522;type=read:status=OK\x1b\\"));
    assert!(reply.contains("status=DATA:mime=Lg==;"));
    assert!(reply.ends_with("\x1b]5522;type=read:status=DONE\x1b\\"));
}

#[test]
fn reads_are_denied_by_default() {
    let (mut term, recorder) = terminal(KittyClipboardConfig::default());
    let mut parser = Processor::new();

    feed(
        &mut term,
        &mut parser,
        format!("\x1b]5522;type=read;{}\x1b\\", payload("text/plain")).as_bytes(),
    );

    assert_eq!(recorder.pty_writes(), vec!["\x1b]5522;type=read:status=EPERM\x1b\\"]);
}

#[test]
fn reads_hand_the_clipboard_to_the_front_end() {
    let (mut term, recorder) = terminal(permissive());
    let mut parser = Processor::new();

    feed(
        &mut term,
        &mut parser,
        &format!("\x1b]5522;type=read:id=w1;{}\x1b\\", payload("text/plain image/png")),
    );

    // The read became a front end event, not an immediate reply.
    let format = recorder
        .get(|event| match event {
            Event::KittyClipboardRead(loc, format) => Some((*loc, format.clone())),
            _ => None,
        })
        .expect("the read request reached the front end");
    assert_eq!(format.0, ClipboardType::Clipboard);

    // The formatter turns text into the chunked reply.
    assert_eq!(
        format.1(Some("hi")),
        format!(
            "\x1b]5522;type=read:status=OK:id=w1\x1b\\\x1b]5522;type=read:status=DATA:mime={}:id=w1;{}\x1b\\\x1b]5522;type=read:status=DONE:id=w1\x1b\\",
            payload("text/plain"),
            payload("hi"),
        )
    );
    // `image/png` was skipped: only served types are answered.

    // A missing location, such as a primary selection on Windows or macOS,
    // becomes `ENOSYS`.
    assert_eq!(format.1(None), "\x1b]5522;type=read:status=ENOSYS:id=w1\x1b\\");
}

#[test]
fn primary_reads_reach_the_front_end_as_selections() {
    let (mut term, recorder) = terminal(permissive());
    let mut parser = Processor::new();

    feed(
        &mut term,
        &mut parser,
        &format!("\x1b]5522;type=read:loc=primary;{}\x1b\\", payload("text/plain")),
    );

    let loc = recorder
        .get(|event| match event {
            Event::KittyClipboardRead(loc, _) => Some(*loc),
            _ => None,
        })
        .expect("the read request reached the front end");
    assert_eq!(loc, ClipboardType::Selection);
}

#[test]
fn writes_store_text_and_answer_through_the_outcome() {
    let (mut term, recorder) = terminal(permissive());
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b]5522;type=write:id=a1\x1b\\");
    feed(
        &mut term,
        &mut parser,
        &format!("\x1b]5522;type=wdata:mime={};{}\x1b\\", payload("text/plain"), payload("hel")),
    );
    feed(
        &mut term,
        &mut parser,
        &format!("\x1b]5522;type=wdata:mime={};{}\x1b\\", payload("text/plain"), payload("lo")),
    );
    feed(&mut term, &mut parser, b"\x1b]5522;type=wdata\x1b\\");

    // Nothing was written to the PTY during the write; the store is a front
    // end event whose formatter produces the final reply.
    assert!(recorder.pty_writes().is_empty());
    let (loc, text, format) = recorder
        .get(|event| match event {
            Event::KittyClipboardStore(loc, text, format) => {
                Some((*loc, text.clone(), format.clone()))
            },
            _ => None,
        })
        .expect("the store reached the front end");
    assert_eq!((loc, text.as_str()), (ClipboardType::Clipboard, "hello"));

    assert_eq!(format(StoreOutcome::Stored), "\x1b]5522;type=write:status=DONE:id=a1\x1b\\");
    assert_eq!(format(StoreOutcome::Failed), "\x1b]5522;type=write:status=EIO:id=a1\x1b\\");
    assert_eq!(format(StoreOutcome::Unavailable), "\x1b]5522;type=write:status=ENOSYS:id=a1\x1b\\");
}

#[test]
fn writes_are_denied_by_config() {
    let (mut term, recorder) =
        terminal(KittyClipboardConfig { write: Permission::Deny, ..permissive() });
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b]5522;type=write:id=a2\x1b\\");

    assert_eq!(recorder.pty_writes(), vec!["\x1b]5522;type=write:status=EPERM:id=a2\x1b\\"]);
}

#[test]
fn corrupt_writes_fail_with_einval_and_are_dropped() {
    let (mut term, recorder) = terminal(permissive());
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b]5522;type=write\x1b\\");
    // `aGk` is missing its padding.
    feed(
        &mut term,
        &mut parser,
        &format!("\x1b]5522;type=wdata:mime={};aGk\x1b\\", payload("text/plain")),
    );
    assert_eq!(recorder.pty_writes(), vec!["\x1b]5522;type=write:status=EINVAL\x1b\\"]);

    // The error abandons the write: chunks are ignored until the next start,
    // and the end marker does not store anything.
    feed(
        &mut term,
        &mut parser,
        &format!("\x1b]5522;type=wdata:mime={};{}\x1b\\", payload("text/plain"), payload("x")),
    );
    feed(&mut term, &mut parser, b"\x1b]5522;type=wdata\x1b\\");
    // No reply beyond the EINVAL, and nothing stored.
    assert_eq!(recorder.pty_writes().len(), 1);
    assert!(
        recorder
            .get(|event| match event {
                Event::KittyClipboardStore(..) => Some(()),
                _ => None,
            })
            .is_none(),
        "the aborted write must not store"
    );
}

#[test]
fn oversized_writes_fail_with_efbig() {
    let (mut term, recorder) = terminal(KittyClipboardConfig { max_write_size: 4, ..permissive() });
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b]5522;type=write:id=big\x1b\\");
    feed(
        &mut term,
        &mut parser,
        &format!("\x1b]5522;type=wdata:mime={};{}\x1b\\", payload("text/plain"), payload("hello")),
    );

    assert_eq!(recorder.pty_writes(), vec!["\x1b]5522;type=write:status=EFBIG:id=big\x1b\\"]);
}

#[test]
fn the_protocol_reports_itself_through_decrqm() {
    let (mut term, recorder) = terminal(permissive());
    let mut parser = Processor::new();

    // Recognized, not set.
    feed(&mut term, &mut parser, b"\x1b[?5522$p");
    assert_eq!(recorder.pty_writes(), vec!["\x1b[?5522;2$y"]);

    // Set through DECSET.
    feed(&mut term, &mut parser, b"\x1b[?5522h");
    feed(&mut term, &mut parser, b"\x1b[?5522$p");
    assert_eq!(recorder.pty_writes()[1], "\x1b[?5522;1$y");

    // And reset again.
    feed(&mut term, &mut parser, b"\x1b[?5522l");
    feed(&mut term, &mut parser, b"\x1b[?5522$p");
    assert_eq!(recorder.pty_writes()[2], "\x1b[?5522;2$y");
}

#[test]
fn a_disabled_protocol_is_not_supported() {
    let (mut term, recorder) = terminal(KittyClipboardConfig {
        read: Permission::Deny,
        write: Permission::Deny,
        ..KittyClipboardConfig::default()
    });
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b[?5522$p");
    feed(&mut term, &mut parser, b"\x1b[?5522h");
    feed(
        &mut term,
        &mut parser,
        format!("\x1b]5522;type=read;{}\x1b\\", payload("text/plain")).as_bytes(),
    );

    // The mode reports itself unsupported, DECSET is ignored, and packets
    // are dropped without a reply.
    assert_eq!(recorder.pty_writes(), vec!["\x1b[?5522;0$y"]);
    assert!(
        recorder
            .get(|event| match event {
                Event::KittyClipboardRead(..) => Some(()),
                _ => None,
            })
            .is_none()
    );
}

#[test]
fn reset_abandons_a_write_in_progress() {
    let (mut term, recorder) = terminal(permissive());
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b]5522;type=write:id=r1\x1b\\");
    feed(
        &mut term,
        &mut parser,
        &format!("\x1b]5522;type=wdata:mime={};{}\x1b\\", payload("text/plain"), payload("half")),
    );
    feed(&mut term, &mut parser, b"\x1bc");
    feed(&mut term, &mut parser, b"\x1b]5522;type=wdata\x1b\\");

    // After the reset the end marker is a stray chunk, refused with EINVAL
    // instead of storing the abandoned text.
    assert_eq!(recorder.pty_writes(), vec!["\x1b]5522;type=write:status=EINVAL\x1b\\"]);
    assert!(
        recorder
            .get(|event| match event {
                Event::KittyClipboardStore(..) => Some(()),
                _ => None,
            })
            .is_none()
    );
}

#[test]
fn other_unknown_osc_sequences_stay_silent() {
    let (mut term, recorder) = terminal(permissive());
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b]9999;whatever\x1b\\");

    assert!(recorder.pty_writes().is_empty());
}
