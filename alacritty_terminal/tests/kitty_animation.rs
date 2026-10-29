//! End-to-end tests for kitty animation protocol support.
//!
//! These drive the real VT parser into a real [`Term`], covering the `a=f`
//! frame transmissions, `a=a` playback control, the terminal-driven clock,
//! and the replies the terminal writes back.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
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
}

impl EventListener for Recorder {
    fn send_event(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

/// Build a terminal together with its event recorder.
fn terminal() -> (Term<Recorder>, Recorder) {
    let recorder = Recorder::default();
    let size = TermSize::new(80, 24);
    let term = Term::new(Config::default(), &size, recorder.clone());
    (term, recorder)
}

/// Feed raw bytes through the real parser, settling any decode it queues the
/// way the event loop does.
fn feed(term: &mut Term<Recorder>, parser: &mut Processor, bytes: &[u8]) {
    parser.advance(term, bytes);

    for result in term.take_graphics_jobs() {
        term.install_graphics(result.decode());
    }
}

/// Encode a payload the way clients do.
fn payload(text: &[u8]) -> String {
    BASE64.encode(text)
}

/// The root frame of a 1x1 animation: one opaque red pixel.
fn base_image() -> Vec<u8> {
    format!("\x1b_Ga=t,i=7,f=32,s=1,v=1;{}\x1b\\", payload(&[255, 0, 0, 255])).into_bytes()
}

/// Append an animation frame carrying one pixel, or a wider patch.
fn frame<P: AsRef<[u8]>>(pixel: P, keys: &str) -> Vec<u8> {
    format!("\x1b_Ga=f,i=7,{keys};{}\x1b\\", payload(pixel.as_ref())).into_bytes()
}

/// An image with a red root and a green second frame, running forever.
///
/// Returns the moment playback was armed, which is the earliest deadline the
/// test can observe.
fn running_terminal() -> (Term<Recorder>, Recorder, Instant) {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, &base_image());
    feed(&mut term, &mut parser, &frame([0, 255, 0, 255], "f=32,s=1,v=1"));
    // The root frame is gapless by construction, so a client that wants it
    // shown gives it a gap before running the animation.
    feed(&mut term, &mut parser, b"\x1b_Ga=a,i=7,r=1,z=40\x1b\\");
    feed(&mut term, &mut parser, b"\x1b_Ga=a,i=7,s=3,v=1\x1b\\");
    let now = Instant::now();

    (term, recorder, now)
}

#[test]
fn frame_transmissions_reply_with_their_frame_number() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, &base_image());
    feed(&mut term, &mut parser, &frame([0, 255, 0, 255], "f=32,s=1,v=1"));
    feed(&mut term, &mut parser, &frame([0, 0, 255, 255], "f=32,s=1,v=1"));

    // The root transmission is acknowledged like any image, and each frame
    // reply names the frame the data landed on.
    assert_eq!(
        recorder.pty_writes(),
        ["\x1b_Gi=7;OK\x1b\\", "\x1b_Gi=7,r=2;OK\x1b\\", "\x1b_Gi=7,r=3;OK\x1b\\"]
    );
}

#[test]
fn running_animations_arm_the_clock_and_advance() {
    let (mut term, _recorder, now) = running_terminal();

    // The running animation is due once the current frame's gap elapses.
    assert!(term.next_animation_deadline().is_some());

    // Nothing advances before the deadline.
    assert!(!term.advance_animations(now));

    // Past it, the green frame is shown and the clock re-arms.
    assert!(term.advance_animations(now + Duration::from_secs(1)));
    assert!(term.next_animation_deadline().is_some());
}

#[test]
fn the_displayed_pixels_follow_the_current_frame() {
    let (mut term, _recorder, now) = running_terminal();

    let red = term.graphics().image(1).unwrap().pixels.clone();
    assert!(!term.advance_animations(now));
    assert_eq!(&*term.graphics().image(1).unwrap().pixels, &*red);

    term.advance_animations(now + Duration::from_secs(1));
    let green = &term.graphics().image(1).unwrap().frames()[1].pixels;
    assert_eq!(&*term.graphics().image(1).unwrap().pixels, &**green);
}

#[test]
fn gapless_frames_are_skipped() {
    let (mut term, _recorder) = terminal();
    let mut parser = Processor::new();
    feed(&mut term, &mut parser, &base_image());

    // Frame 2 is gapless: playing from the root must skip it and land on
    // frame 3, which has the sequence's only visible gap.
    feed(&mut term, &mut parser, &frame([0, 255, 0, 255], "z=-1,f=32,s=1,v=1"));
    feed(&mut term, &mut parser, &frame([0, 0, 255, 255], "f=32,s=1,v=1"));
    feed(&mut term, &mut parser, b"\x1b_Ga=a,i=7,r=1,z=40\x1b\\");
    feed(&mut term, &mut parser, b"\x1b_Ga=a,i=7,s=3\x1b\\");
    let now = Instant::now();

    assert!(term.advance_animations(now + Duration::from_secs(1)));
    let image = term.graphics().image(1).unwrap();
    assert_eq!(image.animation().unwrap().current, 2);
}

#[test]
fn loading_mode_holds_on_the_last_frame() {
    let (mut term, _recorder, now) = running_terminal();
    let mut parser = Processor::new();

    // Loading mode plays like running until the last frame's gap elapses,
    // where it holds instead of wrapping.
    feed(&mut term, &mut parser, b"\x1b_Ga=a,i=7,s=2\x1b\\");
    feed(&mut term, &mut parser, b"\x1b_Ga=a,i=7,c=2\x1b\\");

    assert!(!term.advance_animations(now));
    assert!(!term.advance_animations(now + Duration::from_secs(1)));
    // The hold cleared the deadline until a new frame arrives.
    assert!(term.next_animation_deadline().is_none());

    // A new frame restarts playback from the held frame.
    feed(&mut term, &mut parser, &frame([0, 0, 255, 255], "f=32,s=1,v=1"));
    assert!(term.next_animation_deadline().is_some());
}

#[test]
fn stopping_clears_the_deadline() {
    let (mut term, _recorder, now) = running_terminal();
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, b"\x1b_Ga=a,i=7,s=1\x1b\\");

    assert!(term.next_animation_deadline().is_none());
    assert!(!term.advance_animations(now + Duration::from_secs(1)));
}

#[test]
fn frame_deletion_follows_the_display_and_renumbers() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();
    feed(&mut term, &mut parser, &base_image());
    feed(&mut term, &mut parser, &frame([0, 255, 0, 255], "f=32,s=1,v=1"));
    feed(&mut term, &mut parser, &frame([0, 0, 255, 255], "f=32,s=1,v=1"));
    feed(&mut term, &mut parser, b"\x1b_Ga=a,i=7,c=2\x1b\\");

    let writes = recorder.pty_writes().len();
    feed(&mut term, &mut parser, b"\x1b_Ga=d,d=f,i=7,r=2\x1b\\");

    // The deletion acknowledges like every other addressed command, and the
    // successor took the deleted frame's place.
    assert_eq!(recorder.pty_writes().len(), writes + 1);
    let image = term.graphics().image(1).unwrap();
    assert_eq!(image.frames().len(), 2);
    assert_eq!(image.animation().unwrap().current, 1);
}

#[test]
fn rejected_frames_are_answered() {
    let (mut term, recorder) = terminal();
    let mut parser = Processor::new();

    feed(&mut term, &mut parser, &base_image());

    // A patch wider than the image.
    let wide_patch = [0u8, 0, 255, 255, 0, 0, 255, 255];
    feed(&mut term, &mut parser, &frame(wide_patch, "f=32,s=2,v=1"));
    // A frame on an image that does not exist.
    feed(&mut term, &mut parser, &frame([0, 0, 255, 255], "i=9,f=32,s=1,v=1"));

    assert_eq!(
        recorder.pty_writes(),
        [
            "\x1b_Gi=7;OK\x1b\\",
            "\x1b_Gi=7;EINVAL:frame larger than image\x1b\\",
            "\x1b_Gi=9;ENOENT:image not found\x1b\\",
        ]
    );
}
