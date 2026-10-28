//! Parsing of a single kitty clipboard protocol packet.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::term::ClipboardType;

/// The largest multiplexer `id` echoed back, after filtering.
const MAX_ID_LEN: usize = 128;

/// A parsed OSC 5522 packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// `type=read`: read the requested MIME types, or list them for `.`.
    Read { loc: ClipboardType, request: ReadRequest, id: Option<String> },
    /// `type=write`: begin (or restart) a clipboard write.
    Write { loc: ClipboardType, id: Option<String> },
    /// `type=wdata`: one chunk of data; a missing `mime` marks the end of the
    /// write.
    WData { mime: Option<String>, data: Vec<u8> },
    /// `type=walias`: alias MIME types to a target's data.
    WAlias,
}

/// What a `type=read` packet asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadRequest {
    /// The payload was a bare `.`, asking for the available MIME types.
    List,
    /// A space-separated list of MIME types, matched case-insensitively.
    Mime(Vec<String>),
}

/// A packet that violates the protocol.
///
/// The read half has no error status and ignores its malformed packets; the
/// write half must answer them with `EINVAL`, so the caller needs to know
/// which half failed and which `id` to echo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    /// Whether the packet belonged to the write half.
    pub write: bool,
    /// The multiplexer `id`, filtered, for the error reply.
    pub id: Option<String>,
}

/// Parse the segments following the leading `5522` of an OSC body.
pub fn parse(params: &[&[u8]]) -> Result<Packet, Malformed> {
    let malformed = |write| Malformed { write, id: None };
    let metadata = params.first().ok_or_else(|| malformed(false))?;
    let fields = Metadata::parse(metadata).map_err(|write| Malformed { write, id: None })?;

    // The payload is the second segment; anything behind it is not base64,
    // and thus not a packet this protocol defines.
    let payload = params.get(1).copied().unwrap_or_default();
    if params.iter().skip(2).any(|segment| !segment.is_empty()) {
        return Err(malformed(fields.kind.write()));
    }

    let malformed = |write| Malformed { write, id: fields.id.clone() };

    match fields.kind {
        Kind::Read => {
            let payload = decode_str(payload).ok_or_else(|| malformed(false))?;
            let request = if payload == "." {
                ReadRequest::List
            } else {
                let mimes: Vec<String> = payload
                    .split_ascii_whitespace()
                    .map(|mime| mime.to_ascii_lowercase())
                    .collect();
                if mimes.is_empty() {
                    return Err(malformed(false));
                }
                ReadRequest::Mime(mimes)
            };
            Ok(Packet::Read { loc: fields.loc, request, id: fields.id })
        },
        Kind::Write => {
            if !payload.is_empty() {
                return Err(malformed(true));
            }
            Ok(Packet::Write { loc: fields.loc, id: fields.id })
        },
        Kind::WData => {
            let mime = match fields.mime {
                Some(mime) => Some(mime),
                // The end-of-write marker carries neither MIME nor data.
                None if payload.is_empty() => {
                    return Ok(Packet::WData { mime: None, data: Vec::new() });
                },
                // Data without a MIME type is invalid.
                None => return Err(malformed(true)),
            };
            let data = BASE64.decode(payload).map_err(|_| malformed(true))?;
            Ok(Packet::WData { mime, data })
        },
        Kind::WAlias => {
            // Aliases are recorded but unused; their shape is still validated
            // so corrupt writes fail the same way they would with aliases
            // honored.
            if fields.mime.is_none() || payload.is_empty() {
                return Err(malformed(true));
            }
            Ok(Packet::WAlias)
        },
    }
}

/// The metadata keys of one packet.
struct Metadata {
    kind: Kind,
    loc: ClipboardType,
    id: Option<String>,
    /// The `mime` key, base64-decoded, for `wdata` and `walias` packets.
    mime: Option<String>,
}

impl Metadata {
    /// Parse the colon-separated `key=value` pairs.
    ///
    /// Unknown keys are ignored. The password keys (`pw`, `name`) are
    /// validated when present, since corrupt authorization must fail the
    /// same way as corrupt data, but this build has no prompts to spend
    /// them on, so they are dropped.
    ///
    /// The `Err` flag reports whether the failed packet took part in a
    /// write, which decides between ignoring it and answering `EINVAL`.
    fn parse(metadata: &[u8]) -> Result<Self, bool> {
        // The kind decides how failures are reported, so it is extracted
        // before anything that can fail.
        let kind = metadata
            .split(|byte| *byte == b':')
            .filter_map(|field| {
                let eq = field.iter().position(|byte| *byte == b'=')?;
                match &field[..eq] {
                    b"type" => Kind::parse(&field[eq + 1..]),
                    _ => None,
                }
            })
            .next()
            .ok_or(false)?;

        let mut loc = ClipboardType::Clipboard;
        let mut id = None;
        let mut mime = None;

        for field in metadata.split(|byte| *byte == b':') {
            let Some(eq) = field.iter().position(|byte| *byte == b'=') else {
                return Err(kind.write());
            };
            let (key, value) = (&field[..eq], &field[eq + 1..]);
            match key {
                b"loc" => {
                    loc = match value {
                        b"clipboard" => ClipboardType::Clipboard,
                        b"primary" => ClipboardType::Selection,
                        _ => return Err(kind.write()),
                    };
                },
                b"id" => id = filter_id(value),
                b"mime" => {
                    mime = match decode_str(value) {
                        // An empty MIME type is a missing one.
                        Some(mime) if !mime.is_empty() => Some(mime),
                        _ => return Err(kind.write()),
                    };
                },
                // Authorization is validated, then dropped.
                b"pw" | b"name" => {
                    decode_str(value).ok_or(kind.write())?;
                },
                _ => {},
            }
        }

        Ok(Self { kind, loc, id, mime })
    }
}

/// The packet types a terminal receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    WData,
    WAlias,
}

impl Kind {
    fn parse(value: &[u8]) -> Option<Self> {
        match value {
            b"read" => Some(Self::Read),
            b"write" => Some(Self::Write),
            b"wdata" => Some(Self::WData),
            b"walias" => Some(Self::WAlias),
            _ => None,
        }
    }

    /// Whether packets of this kind take part in a clipboard write.
    fn write(self) -> bool {
        matches!(self, Kind::Write | Kind::WData | Kind::WAlias)
    }
}

/// Decode padded, canonical base64 into UTF-8 text.
fn decode_str(input: &[u8]) -> Option<String> {
    let bytes = BASE64.decode(input).ok()?;
    String::from_utf8(bytes).ok()
}

/// Strip every character the protocol keeps out of multiplexer ids, then cap
/// the remainder.
fn filter_id(id: &[u8]) -> Option<String> {
    let filtered: String = id
        .iter()
        .filter_map(|byte| {
            let byte = *byte as char;
            (byte.is_ascii_alphanumeric() || matches!(byte, '-' | '_' | '+' | '.')).then_some(byte)
        })
        .take(MAX_ID_LEN)
        .collect();
    (!filtered.is_empty()).then_some(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_packet(osc: &str) -> Result<Packet, Malformed> {
        let segments: Vec<&[u8]> = osc.split(';').skip(1).map(str::as_bytes).collect();
        parse(&segments)
    }

    #[test]
    fn parse_read_of_single_mime() {
        // `text/plain`.
        let packet = parse_packet("5522;type=read;dGV4dC9wbGFpbg==").unwrap();
        assert_eq!(
            packet,
            Packet::Read {
                loc: ClipboardType::Clipboard,
                request: ReadRequest::Mime(vec!["text/plain".into()]),
                id: None,
            }
        );
    }

    #[test]
    fn parse_read_listing() {
        // `.`.
        let packet = parse_packet("5522;type=read;Lg==").unwrap();
        assert_eq!(
            packet,
            Packet::Read { loc: ClipboardType::Clipboard, request: ReadRequest::List, id: None }
        );
    }

    #[test]
    fn parse_read_with_location_and_id() {
        let packet = parse_packet("5522;type=read:loc=primary:id=tmux.0;Lg==").unwrap();
        assert_eq!(
            packet,
            Packet::Read {
                loc: ClipboardType::Selection,
                request: ReadRequest::List,
                id: Some("tmux.0".into()),
            }
        );
    }

    #[test]
    fn parse_mimes_case_insensitively() {
        // `TEXT/PLAIN`.
        let packet = parse_packet("5522;type=read;VEVYVC9QTEFJTg==").unwrap();
        assert_eq!(
            packet,
            Packet::Read {
                loc: ClipboardType::Clipboard,
                request: ReadRequest::Mime(vec!["text/plain".into()]),
                id: None,
            }
        );
    }

    #[test]
    fn parse_write_start() {
        let packet = parse_packet("5522;type=write:id=a+b-c._1").unwrap();
        assert_eq!(
            packet,
            Packet::Write { loc: ClipboardType::Clipboard, id: Some("a+b-c._1".into()) }
        );
    }

    #[test]
    fn write_start_rejects_payload() {
        let malformed = parse_packet("5522;type=write;dGV4dA==").unwrap_err();
        assert!(malformed.write);
    }

    #[test]
    fn parse_wdata_chunk_and_end_marker() {
        // `text/plain`.
        let chunk = parse_packet("5522;type=wdata:mime=dGV4dC9wbGFpbg==;aGVsbG8=").unwrap();
        assert_eq!(
            chunk,
            Packet::WData { mime: Some("text/plain".into()), data: b"hello".to_vec() }
        );

        let end = parse_packet("5522;type=wdata").unwrap();
        assert_eq!(end, Packet::WData { mime: None, data: Vec::new() });
    }

    #[test]
    fn wdata_without_mime_is_invalid() {
        let malformed = parse_packet("5522;type=wdata;aGVsbG8=").unwrap_err();
        assert!(malformed.write);
    }

    #[test]
    fn parse_walias_shape() {
        let packet = parse_packet("5522;type=walias:mime=dGV4dC9wbGFpbg==;dGV4dC9odG1s").unwrap();
        assert_eq!(packet, Packet::WAlias);
    }

    #[test]
    fn unknown_type_is_ignored_on_the_read_side() {
        let malformed = parse_packet("5522;type=nope;Lg==").unwrap_err();
        assert!(!malformed.write);
    }

    #[test]
    fn unfilterable_ids_disappear() {
        let packet = parse_packet("5522;type=write:id=!!!;").unwrap();
        assert_eq!(packet, Packet::Write { loc: ClipboardType::Clipboard, id: None });
    }

    #[test]
    fn id_keeps_only_the_valid_charset() {
        let packet = parse_packet("5522;type=write:id=tmüx/0;").unwrap();
        assert_eq!(
            packet,
            Packet::Write { loc: ClipboardType::Clipboard, id: Some("tmx0".into()) }
        );
    }

    #[test]
    fn password_without_name_is_dropped() {
        // Metadata with a lone `pw` still parses; the authorization is simply
        // not honored by this build.
        let packet = parse_packet("5522;type=read:pw=c2VjcmV0;Lg==").unwrap();
        assert_eq!(
            packet,
            Packet::Read { loc: ClipboardType::Clipboard, request: ReadRequest::List, id: None }
        );
    }

    #[test]
    fn non_canonical_base64_is_rejected() {
        // `hello` without padding is not canonical base64.
        assert!(parse_packet("5522;type=wdata:mime=dGV4dC9wbGFpbg==;aGVsbG8").is_err());
        // Whitespace never belongs in a payload.
        assert!(parse_packet("5522;type=read;dGV4dCBwbGFpbg == ").is_err());
    }

    #[test]
    fn extra_payload_segments_are_rejected() {
        let malformed = parse_packet("5522;type=wdata:mime=dGV4dA==;aGk=;aGk=").unwrap_err();
        assert!(malformed.write);
    }

    #[test]
    fn empty_mime_type_is_missing() {
        let malformed = parse_packet("5522;type=wdata:mime=;aGk=").unwrap_err();
        assert!(malformed.write);
    }
}
