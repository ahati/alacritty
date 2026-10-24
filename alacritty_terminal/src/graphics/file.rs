//! Reading image data from the protocol's file and shared memory mediums
//! (`t=f`, `t=t` and `t=s`).
//!
//! A file medium's payload is a path rather than image data, and the terminal
//! reads a window of the file it names. The path is interpreted while the
//! command is parsed, but the file itself is read by the decode that command
//! queued, so that disk I/O never happens with the terminal locked.
//!
//! A shared memory medium's payload is likewise a name, of an object in the
//! platform's shared memory namespace, and it is read by the decode the
//! command queued in just the same way.
//!
//! Every way a read can fail is reported to the client as one identical,
//! information-free error. Telling a client which of "does not exist", "is not
//! readable", "is not a regular file" or "is too short" happened would let a
//! possibly remote or sandboxed program probe the filesystem for the
//! existence, type and size of files it has no business knowing about.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::ffi::CString;
#[cfg(all(unix, not(target_os = "macos")))]
use std::os::unix::io::{FromRawFd, OwnedFd};

use super::command::{Command, Medium};
use super::GraphicsError;

/// Longest path a client may name, in bytes.
pub const MAX_FILENAME_LEN: usize = 2048;

/// Whether a path a client names can reach another machine.
///
/// Only Windows names the host inside the path itself; elsewhere a network
/// filesystem is a mount the user set up in advance and looks like any other
/// directory, so the setting has no effect there.
const NETWORK_PATHS_REACHABLE: bool = cfg!(windows);

/// The verbatim prefix the kernel emits for a path it reached directly, `\\?\`.
///
/// Spelled out in one place because a prefix of this shape is easy to get wrong
/// by a single character, and a prefix that is one character out silently turns
/// the namespace it marks into an ordinary one.
const VERBATIM_PREFIX: &str = "\\\\?\\";

/// The device namespace, `\\.\`.
const DEVICE_PREFIX: &str = "\\\\.\\";

/// The object manager's spelling of the device namespace, `\\??\`.
const NT_PREFIX: &str = "\\\\??\\";

/// The doubled separator that begins a path naming another machine, `\\`.
///
/// Only the plain spelling needs it, which is judged where paths may name a
/// machine, so it is compiled only where that check is.
#[cfg(any(windows, test))]
const UNC_SEPARATOR: &str = "\\\\";

/// The token that follows the verbatim prefix for a share, `UNC\`.
const UNC_TOKEN: &str = "UNC\\";

/// Largest read for a payload whose size its declared pixels do not bound.
///
/// A compressed payload or a PNG stream can be arbitrarily large before it is
/// expanded, so the reference implementation's own cap is applied instead.
pub const MAX_DATA_SIZE: usize = 400_000_000;

/// Size of the buffer a file is read through.
const READ_CHUNK: usize = 64 * 1024;

/// Marker a client puts in the name of a file it wants the terminal to delete.
const TEMP_FILE_MARKER: &[u8] = b"tty-graphics-protocol";

/// The window of a file a command names.
#[derive(Debug)]
pub struct FileSource {
    /// Path bytes as transmitted, truncated at the first NUL.
    path: Vec<u8>,
    /// Bytes to read from the file (`S`), or zero for the rest of it.
    size: u32,
    /// Offset to start reading at (`O`).
    offset: u32,
    /// Whether the file must be deleted once it has been opened (`t=t`).
    temp: bool,
}

impl FileSource {
    /// Interpret the payload of a file medium as the path to read.
    ///
    /// The payload is used as raw bytes without any encoding check, since that
    /// is what a path is. An embedded NUL ends the path, as it would in a C
    /// string, while the length cap is applied to the payload before that
    /// truncation, matching the reference implementation.
    pub fn new(command: &Command, payload: &[u8]) -> Result<Self, GraphicsError> {
        if payload.len() > MAX_FILENAME_LEN {
            return Err(GraphicsError::FilenameTooLong);
        }

        let end = payload.iter().position(|byte| *byte == 0).unwrap_or(payload.len());
        Ok(Self {
            path: payload[..end].to_vec(),
            size: command.data_size,
            offset: command.data_offset,
            temp: command.medium == Medium::TempFile,
        })
    }
}

/// The window of a shared memory object a command names (`t=s`).
#[cfg(unix)]
#[derive(Debug)]
pub struct SharedMemorySource {
    /// The object's name as transmitted, truncated at the first NUL.
    name: Vec<u8>,
    /// Bytes to read from the object (`S`), or zero for the rest of it.
    size: u32,
    /// Offset to start reading at (`O`).
    offset: u32,
}

#[cfg(unix)]
impl SharedMemorySource {
    /// Interpret the payload of a shared memory medium as the name to read.
    ///
    /// The payload is used as raw bytes without any encoding check, as for a
    /// path, truncated at the first NUL and capped at the same length. The
    /// protocol requires the name to begin with a `/` and contain no other;
    /// anything else is refused before the name is opened, and reported with
    /// the same error a read failure gets, so that the client learns nothing
    /// beyond that this name will not be read.
    pub fn new(command: &Command, payload: &[u8]) -> Result<Self, GraphicsError> {
        if payload.len() > MAX_FILENAME_LEN {
            return Err(GraphicsError::FilenameTooLong);
        }

        let end = payload.iter().position(|byte| *byte == 0).unwrap_or(payload.len());
        let name = &payload[..end];
        if name.len() < 2 || name[0] != b'/' || name[1..].contains(&b'/') {
            log::debug!(
                "Refusing to read graphics image from shared memory name {}",
                String::from_utf8_lossy(name)
            );
            return Err(GraphicsError::FileUnreadable);
        }

        Ok(Self {
            name: name.to_vec(),
            size: command.data_size,
            offset: command.data_offset,
        })
    }
}

/// Read the window of the file the source names.
///
/// `max_to_read` bounds the read, since a client must not be able to make the
/// terminal allocate an arbitrary amount of memory for a small image.
pub fn read(
    source: &FileSource,
    max_to_read: usize,
    allow_network_paths: bool,
) -> Result<Vec<u8>, GraphicsError> {
    // Refuse the forms that are refused however they would resolve, before the
    // path is resolved at all: resolving it opens it, so a device or a pipe
    // reached this way has had its side effects by the time a later check could
    // refuse it, and the open itself can block. A network path is included,
    // since resolving one is what reaches out to the host.
    if NETWORK_PATHS_REACHABLE
        && (refused_before_resolving(&source.path)
            || (!allow_network_paths && names_a_network_share(&source.path)))
    {
        log::debug!("Refusing to read graphics image file from this path");
        return Err(GraphicsError::FileUnreadable);
    }

    let path = native_path(&source.path);

    // The policy check has to happen before the file is opened: merely
    // opening a file can have side effects, and whether the open succeeds
    // tells the client whether a file it may not read exists at all.
    let canonical = canonical_allowed(&path, allow_network_paths).ok_or_else(|| {
        log::debug!("Refusing to read graphics image file {}", path.display());
        GraphicsError::FileUnreadable
    })?;

    // From here on the file counts as opened, so a temporary one is removed
    // whatever happens next, whether its contents turn out to be usable or
    // not: a transfer that fails must not leave files behind to accumulate.
    let file = open(&path).map_err(|error| {
        log::debug!("Failed to open graphics image file {}: {error}", path.display());
        GraphicsError::FileUnreadable
    })?;

    let result = check_opened(&path, &file, allow_network_paths)
        .and_then(|()| read_window(&file, source.offset, source.size, max_to_read))
        .and_then(|data| checked_window(data, max_to_read));

    // Nothing is reading the file any more, so it may be removed without
    // leaving the outcome to the platform's delete-sharing rules.
    drop(file);

    if source.temp {
        remove_temp(&source.path, &canonical);
    }

    result
}

/// Reject a window that is shorter than the pixels the control data declares.
///
/// Such a window is reported as unreadable rather than as missing data, so
/// that a client cannot learn how short the file or object was. Windows that
/// may still need processing (compressed data or PNG) are judged by that
/// processing instead.
fn checked_window(data: Vec<u8>, max_to_read: usize) -> Result<Vec<u8>, GraphicsError> {
    if max_to_read < MAX_DATA_SIZE && data.len() < max_to_read {
        log::debug!("Graphics image payload is shorter than its image");
        return Err(GraphicsError::FileUnreadable);
    }

    Ok(data)
}

/// Read the window of the shared memory object the source names (`t=s`).
///
/// The object is opened by name in the platform's shared memory namespace: a
/// flat namespace of objects a client has created with `shm_open`, with no
/// paths, links or traversal in it, so none of the file policy applies and no
/// canonical form is ever resolved. The client hands ownership over with the
/// command, so whatever the read's outcome the object is unlinked afterwards,
/// just as a temporary file is removed whatever the read's outcome.
#[cfg(unix)]
pub fn read_shm(
    source: &SharedMemorySource,
    max_to_read: usize,
) -> Result<Vec<u8>, GraphicsError> {
    // The name has no NUL left in it, so this cannot fail; the conversion is
    // still checked rather than unwrapped.
    let name =
        CString::new(source.name.clone()).map_err(|_| GraphicsError::FileUnreadable)?;

    // `O_NONBLOCK` has no meaning here, since opening the object cannot block;
    // `O_CLOEXEC` so that the descriptor cannot leak into a child process.
    // Darwin's shm_open rejects O_CLOEXEC with EINVAL, so the descriptor is
    // only cloaked where the flag exists; it is closed before read_shm
    // returns either way.
    #[cfg(not(target_os = "macos"))]
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC, 0) };
    #[cfg(target_os = "macos")]
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        log::debug!(
            "Failed to open graphics image shared memory object {}: {}",
            String::from_utf8_lossy(&source.name),
            io::Error::last_os_error()
        );
        return Err(GraphicsError::FileUnreadable);
    }

    // Darwin's shared memory descriptors do not take read(2) or write(2) at
    // all, so there the bytes are reachable only through a mapping; elsewhere
    // the descriptor reads like a file.
    #[cfg(not(target_os = "macos"))]
    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    #[cfg(not(target_os = "macos"))]
    let result = read_window(&file, source.offset, source.size, max_to_read)
        .and_then(|data| checked_window(data, max_to_read));
    #[cfg(target_os = "macos")]
    let result = read_shm_mapping(fd, source, max_to_read);

    // Nothing is reading the object any more, so it may be removed whatever
    // the read's outcome: the client passed ownership with the command.
    #[cfg(not(target_os = "macos"))]
    drop(file);
    #[cfg(target_os = "macos")]
    unsafe { libc::close(fd) };
    if unsafe { libc::shm_unlink(name.as_ptr()) } != 0 {
        log::debug!(
            "Failed to remove graphics image shared memory object {}: {}",
            String::from_utf8_lossy(&source.name),
            io::Error::last_os_error()
        );
    }

    result
}

/// Read the window through a mapping, as Darwin requires.
///
/// A mapping's offset has to lie on a page boundary, which a client's window
/// need not, so the object is mapped from its start and the window is taken
/// from inside the mapping.
#[cfg(target_os = "macos")]
fn read_shm_mapping(
    fd: i32,
    source: &SharedMemorySource,
    max_to_read: usize,
) -> Result<Vec<u8>, GraphicsError> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        log::debug!(
            "Failed to measure graphics image shared memory object: {}",
            io::Error::last_os_error()
        );
        return Err(GraphicsError::FileUnreadable);
    }

    let length = u64::try_from(stat.st_size).unwrap_or(0);
    let offset = u64::from(source.offset);

    // The same window the file mediums read: a client supplied size bounds
    // the read, without one it runs to the end of the object, and either way
    // it is capped, since the object may be far larger than its image.
    let available = length.saturating_sub(offset);
    let wanted = if source.size == 0 { available } else { u64::from(source.size) };
    let wanted = (wanted.min(max_to_read as u64) as usize).min(available as usize);

    if wanted == 0 {
        return Ok(Vec::new());
    }

    let map_length = offset as usize + wanted;
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_length,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if map == libc::MAP_FAILED {
        log::debug!(
            "Failed to map graphics image shared memory object: {}",
            io::Error::last_os_error()
        );
        return Err(GraphicsError::FileUnreadable);
    }

    let data =
        unsafe { std::slice::from_raw_parts(map.cast::<u8>().add(offset as usize), wanted) }
            .to_vec();
    unsafe { libc::munmap(map, map_length) };

    checked_window(data, max_to_read)
}

/// Check a freshly opened file again, now that its descriptor is held.
///
/// The name could have been pointed at a different file between the policy
/// check that preceded the open and the open itself, so the file is judged once
/// more, this time by what was actually opened: on unix the canonical name must
/// still be the same file, by device and inode, and on Windows the name the
/// kernel reports for the handle itself is judged by the same policy.
fn check_opened(path: &Path, file: &File, allow_network_paths: bool) -> Result<(), GraphicsError> {
    let metadata = file.metadata().map_err(|error| {
        log::debug!("Failed to inspect graphics image file {}: {error}", path.display());
        GraphicsError::FileUnreadable
    })?;

    // A directory is not an image, and on unix the test also rules out the
    // devices, pipes and sockets that must not be read. On Windows the same
    // test is weaker, since only directories and links are reported apart from
    // files; the path policy is what rules the rest out there.
    if !metadata.is_file() {
        log::debug!("Graphics image file {} is not a regular file", path.display());
        return Err(GraphicsError::FileUnreadable);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let canonical = canonical_allowed(path, allow_network_paths).ok_or_else(|| {
            log::debug!("Refusing to read graphics image file {}", path.display());
            GraphicsError::FileUnreadable
        })?;

        let named = canonical.metadata().map_err(|error| {
            log::debug!("Failed to inspect graphics image file {}: {error}", canonical.display());
            GraphicsError::FileUnreadable
        })?;

        if metadata.dev() != named.dev() || metadata.ino() != named.ino() {
            log::debug!("Graphics image file {} changed under the terminal", path.display());
            return Err(GraphicsError::FileUnreadable);
        }
    }

    #[cfg(windows)]
    {
        // The name a handle resolves to is what identifies the file that will
        // be read, so the policy is applied to that rather than to a name that
        // is resolved a second time and may by then be a different file.
        let opened = opened_path(file).ok_or_else(|| {
            log::debug!("Failed to name the opened graphics image file {}", path.display());
            GraphicsError::FileUnreadable
        })?;

        if !windows_allowed_form(&opened, allow_network_paths) {
            log::debug!("Refusing to read graphics image file {}", opened);
            return Err(GraphicsError::FileUnreadable);
        }
    }

    Ok(())
}

/// The canonical form of `path`, or `None` when the terminal refuses to read it.
///
/// The check is made on the canonical path and before the file is opened:
/// opening a file can have side effects, and whether it succeeds or not leaks
/// the file's existence to the client. `/proc` and `/sys` expose process and
/// kernel state and `/dev` device nodes, which the protocol has no business
/// reading; `/dev/shm` is the exception, since shared memory objects live
/// there.
fn canonical_allowed(path: &Path, allow_network_paths: bool) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;

    #[cfg(unix)]
    {
        let mut components = canonical.components();
        // Drop the root, so that the first component can be compared without
        // counting the leading separator.
        components.next()?;
        let first = components.next()?;
        let allowed = match first.as_os_str().as_encoded_bytes() {
            b"proc" | b"sys" => false,
            b"dev" => {
                let second = components.next();
                let third = components.next();
                second.map(|part| part.as_os_str() == "shm").unwrap_or(false) && third.is_some()
            },
            _ => true,
        };

        if !allowed {
            return None;
        }
    }

    // The setting only means anything where a path can name another machine.
    let _ = allow_network_paths;

    #[cfg(windows)]
    {
        if !windows_allowed(&canonical, allow_network_paths) {
            return None;
        }
    }

    Some(canonical)
}

/// The name the kernel reports for an open file, or `None` when it cannot.
///
/// This is the file that will be read, whatever the name it was opened through
/// now points at, which is what makes it usable as an identity for the policy.
#[cfg(windows)]
fn opened_path(file: &File) -> Option<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_DOS};

    let handle = file.as_raw_handle() as _;

    // The call reports the length it needs when the buffer is too small, so it
    // is retried with a buffer that size rather than guessed at.
    let mut buffer = vec![0u16; 512];

    loop {
        let written = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                VOLUME_NAME_DOS,
            )
        };

        if written == 0 {
            return None;
        }

        // The length excludes the terminating NUL.
        let written = written as usize;
        if written < buffer.len() {
            return Some(String::from_utf16_lossy(&buffer[..written]));
        }

        buffer.resize(written + 1, 0);
    }
}

/// Whether a canonical Windows path may be read.
///
/// `allow_network_paths` is applied here as well as to the bytes the client
/// sent, so that a path which *resolves* to a share is refused even when it was
/// not written as one: a directory junction or a folder redirect reaches a
/// share through a local-looking name.
#[cfg(windows)]
fn windows_allowed(canonical: &Path, allow_network_paths: bool) -> bool {
    windows_allowed_form(&canonical.as_os_str().to_string_lossy(), allow_network_paths)
}

/// Whether a path form may be read, judging only its text.
///
/// Both a drive path and a share on a host name an ordinary file the user can
/// reach. The device namespace names something that is not a file at all —
/// raw disks, whole volumes, the kernel's own object tree, pipes — and the
/// reserved DOS names are devices however ordinary they look, so those are
/// refused.
///
/// Compiled everywhere, even where the path forms it judges cannot occur, so
/// that the judgement itself can be tested.
#[cfg(any(windows, test))]
fn windows_allowed_form(canonical: &str, allow_network_paths: bool) -> bool {
    // Separators are interchangeable, so the namespace is read from one
    // spelling. `\??\` is the NT spelling of the device namespace.
    let unified = canonical.replace('/', "\\");

    if unified.starts_with(DEVICE_PREFIX) || unified.starts_with(NT_PREFIX) {
        return false;
    }

    // `canonicalize` emits the verbatim prefix, `\\?\`, for the paths it
    // reaches directly: `\\?\C:\...` for a drive and `\\?\UNC\host\share\...`
    // for a share. Judging the familiar spelling underneath is what makes the
    // two comparable; a verbatim path that is neither is a volume, a device or
    // a pipe, and stays refused.
    let (body, was_verbatim) = match unified.strip_prefix(VERBATIM_PREFIX) {
        Some(rest) => (rest, true),
        None => (unified.as_str(), false),
    };

    let drive_path = is_drive_path(body);

    let share_path = if let Some(share) = strip_unc_prefix(body) {
        names_a_host_and_share(share)
    } else if was_verbatim {
        false
    } else {
        // The path needed no canonicalizing, so the plain spelling is allowed
        // alongside the verbatim one.
        body.strip_prefix(UNC_SEPARATOR).is_some_and(names_a_host_and_share)
    };

    if !drive_path && !share_path {
        return false;
    }

    // A share names the host to contact, which the user may refuse outright.
    if share_path && !allow_network_paths {
        return false;
    }

    !is_reserved_device_name(body)
}

/// Whether the bytes a client sent name something that is refused whatever they
/// resolve to.
///
/// This runs on the raw bytes, before the path is resolved, because resolving a
/// path opens it: a device or a pipe that is opened before the policy is
/// consulted has already had its side effects, and can block while it is
/// opened. Only forms that are wrong however they would resolve are refused
/// here, so that a relative path or one with `.` components still works.
fn refused_before_resolving(path: &[u8]) -> bool {
    let text = String::from_utf8_lossy(path);
    let unified = text.replace('/', "\\");

    // The device namespace and its NT spelling name devices, not files.
    if unified.starts_with(DEVICE_PREFIX) || unified.starts_with(NT_PREFIX) {
        return true;
    }

    // A verbatim path may only be a drive or a share; `\\?\Volume{...}`,
    // `\\?\GLOBALROOT\...` and `\\?\PIPE\...` are none of those.
    if let Some(rest) = unified.strip_prefix(VERBATIM_PREFIX) {
        if !is_drive_path(rest) && !strip_unc_prefix(rest).is_some_and(names_a_host_and_share) {
            return true;
        }
    }

    is_reserved_device_name(&unified)
}

/// Whether the text names a drive path, as `C:\...`.
fn is_drive_path(text: &str) -> bool {
    let bytes = text.as_bytes();

    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
}

/// The rest of a `UNC\host\share` path, if that is what the text begins with.
///
/// The token is compared without regard to case, since that is how the path
/// conversion reads it.
fn strip_unc_prefix(text: &str) -> Option<&str> {
    let (token, rest) = text.split_at_checked(4)?;

    token.eq_ignore_ascii_case(UNC_TOKEN).then_some(rest)
}

/// Whether a share path names both a host and a share on it.
///
/// Anything shorter names no file at all.
fn names_a_host_and_share(share: &str) -> bool {
    let mut parts = share.split('\\').filter(|part| !part.is_empty());

    parts.next().is_some() && parts.next().is_some()
}

/// Whether the final component of the text is one of the reserved DOS names.
///
/// Those names refer to devices wherever they appear, and the system strips
/// trailing spaces and periods from a component before deciding, so a name may
/// look ordinary and still name a device. A name that merely begins like one is
/// an ordinary file.
fn is_reserved_device_name(text: &str) -> bool {
    const RESERVED: [&str; 24] = [
        "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5",
        "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
        "LPT8", "LPT9",
    ];

    let Some(component) = text.rsplit('\\').find(|part| !part.is_empty()) else {
        return false;
    };

    // A stream name follows a colon, and a device name ends at the first period
    // or the first colon, whichever comes first.
    let name = component.split(['.', ':']).next().unwrap_or(component);
    let name = name.trim_end_matches([' ', '.']);

    RESERVED.contains(&name.to_ascii_uppercase().as_str())
}

/// Whether the client named a path that leaves this machine.
///
/// A Windows network path is written with a doubled leading separator, in either
/// direction. The check runs on the raw bytes the client sent, so that nothing
/// resolves or opens the path first.
fn names_a_network_share(path: &[u8]) -> bool {
    path.starts_with(br"\\") || path.starts_with(b"//")
}

/// Open a file the terminal only reads.
#[cfg(unix)]
fn open(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    // `O_NONBLOCK` so that opening a client's FIFO does not block the read;
    // `O_CLOEXEC` so that the descriptor cannot leak into a child process.
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

/// Open a file the terminal only reads.
#[cfg(not(unix))]
fn open(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Read the window of an already opened file.
fn read_window(
    file: &File,
    offset: u32,
    size: u32,
    max_to_read: usize,
) -> Result<Vec<u8>, GraphicsError> {
    let length = file.metadata().map_err(|_| GraphicsError::FileUnreadable)?.len();
    let offset = u64::from(offset);

    // A client supplied size bounds the read; without one the read runs to the
    // end of the object. Either way it is capped, since the object may be far
    // larger than the image it holds.
    let available = length.saturating_sub(offset);
    let wanted = if size == 0 { available } else { u64::from(size) };
    let wanted = wanted.min(max_to_read as u64) as usize;

    let mut data = Vec::with_capacity(wanted.min(READ_CHUNK));
    let mut scratch = vec![0u8; READ_CHUNK.min(wanted.max(1))];

    while data.len() < wanted {
        let want = (wanted - data.len()).min(scratch.len());
        let at = offset + data.len() as u64;

        match read_at(file, &mut scratch[..want], at) {
            // End of file: it is shorter than the client claimed, which the
            // caller reports as insufficient data.
            Ok(0) => break,
            Ok(read) => data.extend_from_slice(&scratch[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                log::debug!("Failed to read graphics image file: {error}");
                return Err(GraphicsError::FileUnreadable);
            },
        }
    }

    Ok(data)
}

/// Read from an absolute offset, leaving the file's cursor alone.
#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

/// Read from an absolute offset, leaving the file's cursor alone.
#[cfg(not(unix))]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};

    let mut handle = file;
    handle.seek(SeekFrom::Start(offset))?;
    handle.read(buf)
}

/// Delete the file a temporary transmission named, if it may be deleted.
///
/// The client marks the files it wants removed in their name, and only paths
/// whose canonical form starts with a temporary directory's name are removed,
/// the same plain-prefix comparison the reference implementation makes.
fn remove_temp(path: &[u8], canonical: &Path) {
    if !path.windows(TEMP_FILE_MARKER.len()).any(|window| window == TEMP_FILE_MARKER) {
        return;
    }
    if !is_temp_path(canonical) {
        return;
    }

    let native = native_path(path);
    if let Err(error) = std::fs::remove_file(&native) {
        if error.kind() != io::ErrorKind::NotFound {
            log::debug!("Failed to remove temporary graphics image file: {error}");
        }
    }
}

/// Whether the canonical `path` lies under a temporary directory.
///
/// The comparison is a plain prefix of the path, as the reference implements
/// it, rather than a component-wise one.
fn is_temp_path(path: &Path) -> bool {
    let target = path.as_os_str().as_encoded_bytes();

    temp_dirs().iter().any(|dir| target.starts_with(dir))
}

/// Canonical candidate names of temporary directories.
fn temp_dirs() -> Vec<Vec<u8>> {
    let mut dirs = vec![PathBuf::from("/tmp"), PathBuf::from("/dev/shm")];
    if let Some(dir) = std::env::var_os("TMPDIR") {
        dirs.push(PathBuf::from(dir));
    }
    dirs.push(std::env::temp_dir());

    dirs.into_iter()
        .filter_map(|dir| std::fs::canonicalize(dir).ok())
        .map(|dir| dir.as_os_str().as_encoded_bytes().to_vec())
        .collect()
}

/// Interpret raw bytes as a path, without requiring any encoding.
fn native_path(path: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        PathBuf::from(OsStr::from_bytes(path))
    }

    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(path).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the policy accepts a canonical path, with network shares allowed.
    fn allowed(canonical: &str) -> bool {
        windows_allowed_form(canonical, true)
    }

    #[test]
    fn the_path_prefixes_are_what_they_claim() {
        // A prefix one character out would silently turn the namespace it
        // marks into an ordinary path, so the exact sequences are pinned.
        let chars = |text: &str| text.chars().collect::<Vec<_>>();

        assert_eq!(chars(VERBATIM_PREFIX), ['\\', '\\', '?', '\\']);
        assert_eq!(chars(DEVICE_PREFIX), ['\\', '\\', '.', '\\']);
        assert_eq!(chars(NT_PREFIX), ['\\', '\\', '?', '?', '\\']);
        assert_eq!(chars(UNC_SEPARATOR), ['\\', '\\']);
        assert_eq!(chars(UNC_TOKEN), ['U', 'N', 'C', '\\']);
    }

    #[test]
    fn a_network_share_is_named_by_a_doubled_separator() {
        // Both spellings reach out to a host.
        assert!(names_a_network_share(br"\\host\share\pic.png"));
        assert!(names_a_network_share(b"//host/share/pic.png"));

        // An ordinary path is not a network path, in either direction.
        assert!(!names_a_network_share(br"C:\pic.png"));
        assert!(!names_a_network_share(b"/tmp/pic.png"));
        assert!(!names_a_network_share(br"\pic.png"));
    }

    #[test]
    fn an_ordinary_windows_file_is_allowed() {
        assert!(allowed(r"\\?\C:\Users\me\pic.png"));
        assert!(allowed(r"C:\Users\me\pic.png"));
        assert!(allowed("C:/Users/me/pic.png"));

        // A mapped network drive is an ordinary drive path: the kernel reports
        // it as a drive letter, so it cannot be told from a local one.
        assert!(allowed(r"\\?\Z:\photos\pic.png"));
    }

    #[test]
    fn a_windows_network_share_is_allowed() {
        // `canonicalize` yields the verbatim spelling, which is accepted just
        // as the familiar one is.
        assert!(allowed(r"\\?\UNC\host\share\pic.png"));
        assert!(allowed(r"\\host\share\pic.png"));
        assert!(allowed("//host/share/pic.png"));
        assert!(allowed(r"\\?\UNC\host\share"));
        assert!(allowed(r"\\?\UNC\host\c$\Windows\win.ini"));

        // The token is read without regard to case, and either separator may
        // follow it.
        assert!(allowed(r"\\?\unc\host\share\pic.png"));
        assert!(allowed(r"\\?\Unc\host\share\pic.png"));
        assert!(allowed(r"\\?\UNC/host/share/pic.png"));

        // A share path has to name a share on a host.
        assert!(!allowed(r"\\?\UNC\host"));
        assert!(!allowed(r"\\host"));
        assert!(!allowed("//host"));
    }

    #[test]
    fn a_share_is_refused_when_the_configuration_says_so() {
        // The setting takes away the paths that name another machine, in every
        // spelling, while leaving local files alone.
        for share in [
            r"\\?\UNC\host\share\pic.png",
            r"\\host\share\pic.png",
            "//host/share/pic.png",
            r"\\?\unc\host\share\pic.png",
        ] {
            assert!(!windows_allowed_form(share, false), "{share} must be refused");
            assert!(windows_allowed_form(share, true), "{share} is allowed by default");
        }

        assert!(windows_allowed_form(r"\\?\C:\Users\me\pic.png", false));
    }

    #[test]
    fn the_windows_device_namespace_is_refused() {
        // These are not files, however much they look like paths: raw disks,
        // whole volumes, the kernel's own object tree and pipes.
        assert!(!allowed(r"\\.\PhysicalDrive0"));
        assert!(!allowed(r"\\.\PIPE\pic"));
        assert!(!allowed(r"\\?\PIPE\pic"));
        assert!(!allowed(r"\\?\GLOBALROOT\Device\HarddiskVolume1\pic.png"));
        assert!(!allowed(r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\pic.png"));

        // The NT spellings of the same namespaces, and the separator variants
        // the path conversion treats as equivalent.
        assert!(!allowed(r"\\??\C:\Windows\win.ini"));
        assert!(!allowed(r"\\??\UNC\host\share\pic.png"));
        assert!(!allowed("//./C:/Windows/win.ini"));
        assert!(!allowed(r"\\.\C:"));
    }

    #[test]
    fn the_reserved_device_names_are_refused() {
        assert!(!allowed(r"C:\Users\me\NUL"));
        assert!(!allowed(r"C:\Users\me\NUL."));
        assert!(!allowed(r"C:\Users\me\COM1"));
        assert!(!allowed(r"\\?\C:\COM1"));
        assert!(!allowed(r"C:\Users\me\LPT9"));

        // A device name reaches through a stream or a trailing space as well,
        // and the console devices are named like devices too.
        assert!(!allowed(r"C:\Users\me\NUL:ads"));
        assert!(!allowed(r"C:\Users\me\CONIN$"));
        assert!(!allowed(r"C:\Users\me\CONOUT$"));

        // A name that merely begins like a device is an ordinary file, and a
        // stream on an ordinary file is ordinary.
        assert!(allowed(r"C:\Users\me\console.png"));
        assert!(allowed(r"C:\Users\me\nulled.png"));
        assert!(allowed(r"C:\Users\me\pic.png:hidden"));
    }

    #[test]
    fn a_refused_form_is_refused_before_it_is_resolved() {
        // Whatever these would resolve to, the client asked for something that
        // is not a file, so nothing may open them.
        assert!(refused_before_resolving(br"\\.\PIPE\pic"));
        assert!(refused_before_resolving(br"//./C:/Windows/win.ini"));
        assert!(refused_before_resolving(br"\\.\PhysicalDrive0"));
        assert!(refused_before_resolving(br"\\??\C:\Windows\win.ini"));
        assert!(refused_before_resolving(br"\\?\PIPE\pic"));
        assert!(refused_before_resolving(br"\\?\Volume{b75e2c83-0000}\pic.png"));
        assert!(refused_before_resolving(br"\\?\GLOBALROOT\Device\HarddiskVolume1\pic"));
        assert!(refused_before_resolving(br"C:\Users\me\NUL"));
        assert!(refused_before_resolving(br"ConOut$"));
        assert!(refused_before_resolving(br"C:\Users\me\NUL "));

        // A path that is not yet resolved is not judged by its form: a relative
        // path, a path with `..`, and a share are all left to the checks that
        // can see where they lead.
        assert!(!refused_before_resolving(b"pic.png"));
        assert!(!refused_before_resolving(br"..\..\Windows\win.ini"));
        assert!(!refused_before_resolving(br"\\host\share\pic.png"));
        assert!(!refused_before_resolving(br"\\?\C:\Users\me\pic.png"));
        assert!(!refused_before_resolving(br"\\?\UNC\host\share\pic.png"));
        assert!(!refused_before_resolving(br"C:\Users\me\console.png"));
        assert!(!refused_before_resolving(br"C:\Users\me\pic.png:hidden"));
    }
}
