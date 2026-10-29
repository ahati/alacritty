//! State machine for a clipboard write received over the protocol.

use crate::clipboard::ErrorCode;
use crate::term::ClipboardType;

/// A clipboard write in progress.
///
/// The protocol serializes one write as a `type=write` start packet, any
/// number of `type=wdata` chunks grouped by MIME type, and a bare `wdata`
/// end marker. A fresh start packet restarts a write in progress, and after
/// any error every further write packet is ignored until the next start.
///
/// The terminal has a single system clipboard, so unlike the graphics state
/// there is one session, not one per screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteSession {
    state: State,
    loc: ClipboardType,
    id: Option<String>,
    /// The accumulated data of each MIME type, in arrival order.
    parts: Vec<(String, Vec<u8>)>,
    total: usize,
    max_write_size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Idle,
    Writing,
    Errored,
}

/// What the terminal should do once a write ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Text arrived and belongs on the system clipboard; the front end
    /// stores it and turns the store result into the final reply.
    Store { loc: ClipboardType, text: String, id: Option<String> },
    /// The write carried no MIME type this terminal can store; reply
    /// `ENOSYS`.
    NoText { id: Option<String> },
}

/// A protocol violation while accumulating a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteError {
    /// The status code the reply must carry.
    pub code: ErrorCode,
    /// The multiplexer id of the aborted write.
    pub id: Option<String>,
}

impl WriteSession {
    pub fn new(max_write_size: usize) -> Self {
        Self {
            state: State::Idle,
            loc: ClipboardType::Clipboard,
            id: None,
            parts: Vec::new(),
            total: 0,
            max_write_size,
        }
    }

    /// Drop a write in progress, as the terminal reset does.
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.id = None;
        self.parts.clear();
        self.total = 0;
    }

    /// Begin (or restart) a write.
    pub fn start(&mut self, loc: ClipboardType, id: Option<String>) {
        self.state = State::Writing;
        self.loc = loc;
        self.id = id;
        self.parts.clear();
        self.total = 0;
    }

    /// Whether write packets must be ignored until the next start.
    pub fn errored(&self) -> bool {
        self.state == State::Errored
    }

    /// Abort the session for a packet that was invalid before it could be
    /// accumulated, such as one with corrupt base64.
    pub fn invalidate(&mut self, id: Option<String>) -> WriteError {
        self.state = State::Errored;
        WriteError { code: ErrorCode::EINVAL, id }
    }

    /// Accumulate one chunk, or close the write at the end marker.
    pub fn wdata(
        &mut self,
        mime: Option<String>,
        data: Vec<u8>,
    ) -> Result<Option<Outcome>, WriteError> {
        match self.state {
            State::Errored => return Ok(None),
            State::Idle => return Err(self.invalidate(None)),
            State::Writing => {},
        }

        let Some(mime) = mime else {
            if !data.is_empty() {
                // The end marker carries nothing but itself.
                return Err(self.invalidate(self.id.clone()));
            }
            return Ok(Some(self.finish()));
        };

        let chunk_len = data.len();
        match self.parts.iter_mut().find(|(open, _)| *open == mime) {
            // The protocol sends a MIME type's chunks sequentially, and the
            // terminal decodes their concatenation.
            Some((_, part)) => part.extend_from_slice(&data),
            None => self.parts.push((mime, data)),
        }
        self.total += chunk_len;

        if self.total > self.max_write_size {
            self.state = State::Errored;
            return Err(WriteError { code: ErrorCode::EFBIG, id: self.id.clone() });
        }

        Ok(None)
    }

    /// Close the write at the end marker and decide what to do with it.
    fn finish(&mut self) -> Outcome {
        self.state = State::Idle;
        let id = self.id.take();

        // The first text part that is valid UTF-8 becomes the clipboard
        // content; this build has no destination for the other types.
        match self
            .parts
            .iter()
            .filter(|(mime, _)| is_text_mime(mime))
            .find_map(|(_, data)| String::from_utf8(data.clone()).ok())
        {
            Some(text) => Outcome::Store { loc: self.loc, text, id },
            None => Outcome::NoText { id },
        }
    }
}

/// Whether a MIME type carries text this terminal can put on the clipboard.
pub fn is_text_mime(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    mime == "text/plain" || mime.starts_with("text/plain;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> WriteSession {
        WriteSession::new(16)
    }

    fn start(session: &mut WriteSession) {
        session.start(ClipboardType::Clipboard, Some("id".into()));
    }

    #[test]
    fn chunked_text_write_stores() {
        let mut session = session();
        start(&mut session);
        assert_eq!(session.wdata(Some("text/plain".into()), b"hel".to_vec()), Ok(None));
        assert_eq!(session.wdata(Some("text/plain".into()), b"lo".to_vec()), Ok(None));
        assert_eq!(
            session.wdata(None, Vec::new()),
            Ok(Some(Outcome::Store {
                loc: ClipboardType::Clipboard,
                text: "hello".into(),
                id: Some("id".into()),
            }))
        );
        // The write is over; a stray chunk without a start is invalid.
        assert_eq!(
            session.wdata(Some("text/plain".into()), b"x".to_vec()),
            Err(WriteError { code: ErrorCode::EINVAL, id: None })
        );
    }

    #[test]
    fn non_text_writes_are_refused_with_enosys() {
        let mut session = session();
        start(&mut session);
        session.wdata(Some("image/png".into()), b"png".to_vec()).unwrap();
        assert_eq!(
            session.wdata(None, Vec::new()),
            Ok(Some(Outcome::NoText { id: Some("id".into()) }))
        );
    }

    #[test]
    fn invalid_utf8_text_parts_are_skipped() {
        let mut session = session();
        start(&mut session);
        session.wdata(Some("text/plain".into()), vec![0xff]).unwrap();
        session.wdata(Some("text/plain;charset=utf-8".into()), b"ok".to_vec()).unwrap();
        assert_eq!(
            session.wdata(None, Vec::new()),
            Ok(Some(Outcome::Store {
                loc: ClipboardType::Clipboard,
                text: "ok".into(),
                id: Some("id".into()),
            }))
        );
    }

    #[test]
    fn mime_switches_open_new_parts() {
        let mut session = session();
        start(&mut session);
        session.wdata(Some("text/plain".into()), b"a".to_vec()).unwrap();
        session.wdata(Some("text/html".into()), b"b".to_vec()).unwrap();
        // Reopening a MIME type does not lose the earlier data.
        session.wdata(Some("text/plain".into()), b"c".to_vec()).unwrap();

        // The first text part wins the store, with everything it collected.
        assert_eq!(
            session.wdata(None, Vec::new()),
            Ok(Some(Outcome::Store {
                loc: ClipboardType::Clipboard,
                text: "ac".into(),
                id: Some("id".into()),
            }))
        );
    }

    #[test]
    fn oversized_writes_fail_with_efbig() {
        let mut session = session();
        start(&mut session);
        assert_eq!(
            session.wdata(Some("text/plain".into()), vec![b'x'; 17]),
            Err(WriteError { code: ErrorCode::EFBIG, id: Some("id".into()) })
        );
        // After an error, chunks are ignored until the next start.
        assert_eq!(session.wdata(Some("text/plain".into()), b"y".to_vec()), Ok(None));
        assert!(session.errored());
        start(&mut session);
        assert!(!session.errored());
    }

    #[test]
    fn restart_drops_the_abandoned_write() {
        let mut session = session();
        start(&mut session);
        session.wdata(Some("text/plain".into()), b"old".to_vec()).unwrap();
        session.start(ClipboardType::Selection, Some("second".into()));
        session.wdata(Some("text/plain".into()), b"new".to_vec()).unwrap();
        assert_eq!(
            session.wdata(None, Vec::new()),
            Ok(Some(Outcome::Store {
                loc: ClipboardType::Selection,
                text: "new".into(),
                id: Some("second".into()),
            }))
        );
    }

    #[test]
    fn end_marker_with_data_is_invalid() {
        let mut session = session();
        start(&mut session);
        assert!(session.wdata(None, b"stray".to_vec()).is_err());
        assert!(session.errored());
    }

    #[test]
    fn reset_drops_everything() {
        let mut session = session();
        start(&mut session);
        session.wdata(Some("text/plain".into()), b"half".to_vec()).unwrap();
        session.reset();
        assert!(!session.errored());
        // After a reset the next end marker is a stray chunk from the
        // session's point of view, so it is invalid.
        assert_eq!(
            session.wdata(None, Vec::new()),
            Err(WriteError { code: ErrorCode::EINVAL, id: None })
        );
    }

    #[test]
    fn text_mime_matching_covers_parameters() {
        assert!(is_text_mime("text/plain"));
        assert!(is_text_mime("TEXT/PLAIN"));
        assert!(is_text_mime("text/plain;charset=utf-8"));
        assert!(!is_text_mime("text/html"));
        assert!(!is_text_mime("text/plainx"));
    }
}
