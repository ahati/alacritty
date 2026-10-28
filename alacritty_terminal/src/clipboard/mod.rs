//! Kitty clipboard protocol support.
//!
//! This module implements the terminal side of the [Kitty clipboard
//! protocol](https://sw.kovidgoyal.net/kitty/clipboard/), which exchanges
//! clipboard data over OSC 5522 packets of the form
//! `ESC ] 5522 ; metadata ; payload ESC \`, an extension of OSC 52.
//!
//! The implementation is split by responsibility:
//!
//! * [`packet`] parses one packet out of the raw OSC parameters.
//! * [`session`] tracks a write in progress, accumulating and validating its chunks.
//! * [`response`] builds reply packets, including chunked reads and the `id` echo.
//! * This module holds the configuration, the permission model, and the packet router.
//!
//! Only plain text is served and stored for now (`text/plain` and
//! `text/plain;charset=utf-8`). Other MIME types are refused with `ENOSYS`
//! when a write consists solely of them, and are silently skipped when a
//! read requests them alongside text. Permission is config-driven; until the
//! front end grows permission prompts, a denial is a protocol-visible
//! `EPERM`, and password-based authorization (`pw`/`name`) is parsed and
//! ignored.

mod packet;
mod response;
mod session;

pub use self::packet::{Malformed, Packet, ReadRequest};
pub use self::response::{SERVED_MIMES, paste_notification};
pub use self::session::{Outcome, WriteSession};

use std::sync::Arc;

use crate::event::{Event, EventListener};

/// The OSC number of the protocol and of the paste-events private mode.
///
/// The private mode reusing the protocol's number is per spec: it makes
/// paste events announce the clipboard's MIME types over this protocol.
pub const OSC_NUMBER: u16 = 5522;

/// [`OSC_NUMBER`] as the raw OSC body segment it is dispatched under.
pub const OSC_BYTES: &[u8] = b"5522";

/// Default ceiling for a client write, the protocol's required minimum.
pub const DEFAULT_MAX_WRITE_SIZE: usize = 64 * 1024 * 1024;

/// Whether terminal programs may use a clipboard operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Permission {
    /// The operation is refused with `EPERM`.
    #[default]
    Deny,
    /// The operation is carried out.
    Allow,
}

/// Terminal-side configuration for the kitty clipboard protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyClipboardConfig {
    /// Whether clients may read clipboard content.
    pub read: Permission,
    /// Whether clients may write clipboard content.
    pub write: Permission,
    /// The largest write accepted before it is refused with `EFBIG`.
    pub max_write_size: usize,
}

impl Default for KittyClipboardConfig {
    fn default() -> Self {
        // The same compromise as OSC 52's `OnlyCopy` default: writing is
        // harmless modulo what the program already produced, reading hands
        // clipboard content to a program that did not have it.
        Self {
            read: Permission::Deny,
            write: Permission::Allow,
            max_write_size: DEFAULT_MAX_WRITE_SIZE,
        }
    }
}

impl KittyClipboardConfig {
    /// Whether the protocol is handled at all.
    ///
    /// When nothing is permitted the packets are ignored and the supporting
    /// private mode reports itself as unsupported, so probing programs fall
    /// back to plain OSC 52.
    pub fn enabled(&self) -> bool {
        self.read == Permission::Allow || self.write == Permission::Allow
    }
}

/// The result of a clipboard store on the front end thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOutcome {
    /// The text reached the system clipboard; reply `DONE`.
    Stored,
    /// The clipboard provider failed; reply `EIO`.
    Failed,
    /// The location does not exist on this platform, such as a primary
    /// selection on Windows or macOS; reply `ENOSYS`.
    Unavailable,
}

/// Route one OSC 5522 packet through the config, the write session, and the
/// front end.
///
/// `params` are the raw OSC body segments following the leading `5522`.
pub fn handle_packet<T: EventListener>(
    params: &[&[u8]],
    config: &KittyClipboardConfig,
    session: &mut WriteSession,
    event_proxy: &T,
) {
    if !config.enabled() {
        return;
    }

    let packet = match packet::parse(params) {
        Ok(packet) => packet,
        Err(malformed) => {
            // The write half must answer invalid packets with `EINVAL`; the
            // read half has no error status, so it silently ignores them.
            if malformed.write {
                let error = session.invalidate(malformed.id);
                event_proxy.send_event(Event::PtyWrite(response::write_error(
                    error.code,
                    error.id.as_deref(),
                )));
            }
            return;
        },
    };

    match packet {
        Packet::Read { loc, request, id } => match request {
            // Listings never touch the system clipboard, so they can be
            // answered from what this build serves, without a prompt.
            ReadRequest::List => {
                let reply = response::listing_response(id.as_deref());
                event_proxy.send_event(Event::PtyWrite(reply));
            },
            ReadRequest::Mime(requested) => {
                if config.read == Permission::Deny {
                    let reply = response::read_error(ErrorCode::EPERM, id.as_deref());
                    event_proxy.send_event(Event::PtyWrite(reply));
                    return;
                }

                let served: Vec<&str> = SERVED_MIMES
                    .iter()
                    .copied()
                    .filter(|mime| requested.iter().any(|request| request == mime))
                    .collect();

                // Every requested type missing is `ENOSYS`, per the read
                // error definitions; a partial overlap just skips the
                // missing types.
                if served.is_empty() {
                    let reply = response::read_error(ErrorCode::ENOSYS, id.as_deref());
                    event_proxy.send_event(Event::PtyWrite(reply));
                    return;
                }

                let id = id.clone();
                event_proxy.send_event(Event::KittyClipboardRead(
                    loc,
                    Arc::new(move |text: Option<&str>| match text {
                        // The front end reports a missing location, such as a
                        // primary selection on Windows or macOS.
                        None => response::read_error(ErrorCode::ENOSYS, id.as_deref()),
                        Some(text) => {
                            response::read_response(&served, text.as_bytes(), id.as_deref())
                        },
                    }),
                ));
            },
        },
        Packet::Write { loc, id } => {
            if config.write == Permission::Deny {
                let reply = response::write_error(ErrorCode::EPERM, id.as_deref());
                event_proxy.send_event(Event::PtyWrite(reply));
                return;
            }
            session.start(loc, id);
        },
        Packet::WData { mime, data } => {
            if session.errored() {
                return;
            }
            match session.wdata(mime, data) {
                Err(error) => {
                    event_proxy.send_event(Event::PtyWrite(response::write_error(
                        error.code,
                        error.id.as_deref(),
                    )));
                },
                Ok(Some(Outcome::Store { loc, text, id })) => {
                    event_proxy.send_event(Event::KittyClipboardStore(
                        loc,
                        text,
                        Arc::new(move |outcome: StoreOutcome| {
                            let code = match outcome {
                                StoreOutcome::Stored => return response::write_done(id.as_deref()),
                                StoreOutcome::Failed => ErrorCode::EIO,
                                StoreOutcome::Unavailable => ErrorCode::ENOSYS,
                            };
                            response::write_error(code, id.as_deref())
                        }),
                    ));
                },
                Ok(Some(Outcome::NoText { id })) => {
                    // The write carried nothing this terminal can put on the
                    // system clipboard.
                    let reply = response::write_error(ErrorCode::ENOSYS, id.as_deref());
                    event_proxy.send_event(Event::PtyWrite(reply));
                },
                Ok(None) => {},
            }
        },
        // MIME aliasing rides on stored data this terminal does not keep
        // beyond text; the packet is valid, so the write continues.
        Packet::WAlias => {},
    }
}

/// The status and error codes the protocol defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    EIO,
    EINVAL,
    ENOSYS,
    EPERM,
    EBUSY,
    EFBIG,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::EIO => "EIO",
            ErrorCode::EINVAL => "EINVAL",
            ErrorCode::ENOSYS => "ENOSYS",
            ErrorCode::EPERM => "EPERM",
            ErrorCode::EBUSY => "EBUSY",
            ErrorCode::EFBIG => "EFBIG",
        }
    }
}
