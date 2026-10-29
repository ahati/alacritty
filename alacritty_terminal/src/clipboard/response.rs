//! Building of kitty clipboard protocol replies.
//!
//! Every function in this module returns protocol bytes; nothing else in the
//! terminal may write an OSC 5522 sequence. All replies terminate with ST and
//! echo the multiplexer `id` of the request they answer, as the protocol
//! requires.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use super::ErrorCode;

/// The MIME types this terminal puts on, and serves from, the clipboard.
///
/// These are also the types the MIME listing reports; until the clipboard
/// learns richer types there is nothing else to advertise.
pub const SERVED_MIMES: [&str; 2] = ["text/plain", "text/plain;charset=utf-8"];

/// The largest decoded byte count carried by one `DATA` chunk.
const CHUNK_SIZE: usize = 4096;

/// The MIME type the listing reports its result under.
const LISTING_MIME: &str = ".";

/// Assemble one packet out of its metadata and optional payload.
fn packet(metadata: &str, payload: Option<&[u8]>) -> String {
    let mut out = format!("\x1b]5522;{metadata}");
    if let Some(payload) = payload {
        out.push(';');
        out.push_str(&BASE64.encode(payload));
    }
    out.push_str("\x1b\\");
    out
}

/// The `id=` metadata suffix echoing the multiplexer id of the request.
fn id_suffix(id: Option<&str>) -> String {
    match id {
        Some(id) => format!(":id={id}"),
        None => String::new(),
    }
}

/// The opening `status=OK` packet of a read reply.
fn read_ok(id: Option<&str>, primary: bool) -> String {
    let loc = if primary { ":loc=primary" } else { "" };
    packet(&format!("type=read:status=OK{loc}{}", id_suffix(id)), None)
}

/// One `status=DATA` packet of a read reply.
fn read_data(mime: &str, chunk: &[u8], id: Option<&str>) -> String {
    let mime = BASE64.encode(mime);
    packet(&format!("type=read:status=DATA:mime={mime}{}", id_suffix(id)), Some(chunk))
}

/// The closing `status=DONE` packet of a read reply.
fn read_done(id: Option<&str>) -> String {
    packet(&format!("type=read:status=DONE{}", id_suffix(id)), None)
}

/// The packet replacing `status=OK` when a read fails.
pub fn read_error(code: ErrorCode, id: Option<&str>) -> String {
    packet(&(format!("type=read:status={}", code.as_str()) + &id_suffix(id)), None)
}

/// The reply to a write the system clipboard accepted.
pub fn write_done(id: Option<&str>) -> String {
    packet(&format!("type=write:status=DONE{}", id_suffix(id)), None)
}

/// The reply refusing, or failing to complete, a write.
pub fn write_error(code: ErrorCode, id: Option<&str>) -> String {
    packet(&(format!("type=write:status={}", code.as_str()) + &id_suffix(id)), None)
}

/// The full reply to a MIME listing request.
///
/// The listing names what this build can put on the clipboard; it never
/// touches the system clipboard, which is what lets it be answered without a
/// permission prompt.
pub fn listing_response(id: Option<&str>) -> String {
    let list = SERVED_MIMES.join(" ");
    read_ok(id, false) + &read_data(LISTING_MIME, list.as_bytes(), id) + &read_done(id)
}

/// The full reply to a data read, chunked at the protocol's limit.
///
/// Every served MIME type delivers the same text, chunked sequentially per
/// type as the protocol requires.
pub fn read_response(served: &[&str], text: &[u8], id: Option<&str>) -> String {
    let mut out = read_ok(id, false);
    for mime in served {
        for chunk in text.chunks(CHUNK_SIZE) {
            out.push_str(&read_data(mime, chunk, id));
        }
    }
    out.push_str(&read_done(id));
    out
}

/// The unsolicited MIME listing sent when the user pastes with the
/// paste-events mode enabled.
///
/// The listing travels under the listing MIME, and `loc=primary` marks a
/// paste out of the primary selection. No `id` is attached: unsolicited
/// packets have none, and multiplexers forward them to the active window.
pub fn paste_notification(primary: bool) -> String {
    let list = SERVED_MIMES.join(" ");
    read_ok(None, primary) + &read_data(LISTING_MIME, list.as_bytes(), None) + &read_done(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base64(text: &str) -> String {
        BASE64.encode(text)
    }

    #[test]
    fn write_replies_carry_status_and_id() {
        assert_eq!(write_done(Some("tmux.0")), "\x1b]5522;type=write:status=DONE:id=tmux.0\x1b\\");
        assert_eq!(
            write_error(ErrorCode::EINVAL, None),
            "\x1b]5522;type=write:status=EINVAL\x1b\\"
        );
    }

    #[test]
    fn read_errors_name_the_code() {
        assert_eq!(read_error(ErrorCode::EPERM, None), "\x1b]5522;type=read:status=EPERM\x1b\\");
        assert_eq!(
            read_error(ErrorCode::ENOSYS, Some("i1")),
            "\x1b]5522;type=read:status=ENOSYS:id=i1\x1b\\"
        );
    }

    #[test]
    fn listing_reports_the_served_mimes_under_a_period() {
        let reply = listing_response(None);
        assert_eq!(
            reply,
            format!(
                "\x1b]5522;type=read:status=OK\x1b\\\x1b]5522;type=read:status=DATA:mime={};{}\x1b\\\x1b]5522;type=read:status=DONE\x1b\\",
                base64("."),
                base64("text/plain text/plain;charset=utf-8"),
            )
        );
    }

    #[test]
    fn data_reads_chunk_at_4096_bytes() {
        let text = vec![b'x'; 5000];
        let reply = read_response(&["text/plain"], &text, Some("w"));
        // 5000 bytes split into a full chunk and a partial one.
        assert_eq!(reply.matches("\x1b]5522;type=read:status=DATA").count(), 2);
        // Every packet carries the id: OK, two DATA, DONE.
        assert_eq!(reply.matches(":id=w").count(), 4);
        assert!(reply.contains(&format!(";{}\x1b\\", base64(&"x".repeat(4096)))));
        assert!(reply.ends_with("\x1b]5522;type=read:status=DONE:id=w\x1b\\"));
    }

    #[test]
    fn empty_data_reads_answer_ok_done() {
        let reply = read_response(&["text/plain"], b"", None);
        assert_eq!(
            reply,
            "\x1b]5522;type=read:status=OK\x1b\\\x1b]5522;type=read:status=DONE\x1b\\"
        );
    }

    #[test]
    fn paste_notifications_flag_the_primary_selection() {
        let clipboard = paste_notification(false);
        assert!(clipboard.starts_with("\x1b]5522;type=read:status=OK\x1b\\"));
        assert!(!clipboard.contains("loc=primary"));

        let primary = paste_notification(true);
        assert!(primary.contains("status=OK:loc=primary"));
        assert!(primary.contains(&format!("mime={}", base64("."))));
    }

    #[test]
    fn chunks_reach_the_4096_limit_only_for_full_chunks() {
        // A final partial chunk is padded on its own, as the protocol
        // requires, which the base64 of a short text shows directly.
        let reply = read_response(&["text/plain"], b"hi", None);
        assert!(reply.contains(&format!(";{}\x1b\\", base64("hi"))));
    }
}
