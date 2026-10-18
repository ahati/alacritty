//! Reading image data from the protocol's file mediums (`t=f` and `t=t`).
//!
//! A file medium's payload is a path rather than image data, and the terminal
//! reads a window of the file it names. The path is interpreted while the
//! command is parsed, but the file itself is read by the decode that command
//! queued, so that disk I/O never happens with the terminal locked.
//!
//! Every way a read can fail is reported to the client as one identical,
//! information-free error. Telling a client which of "does not exist", "is not
//! readable", "is not a regular file" or "is too short" happened would let a
//! possibly remote or sandboxed program probe the filesystem for the
//! existence, type and size of files it has no business knowing about.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

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

/// Read the window of the file the source names.
///
/// `max_to_read` bounds the read, since a client must not be able to make the
/// terminal allocate an arbitrary amount of memory for a small image.
pub fn read(
    source: &FileSource,
    max_to_read: usize,
    allow_network_paths: bool,
) -> Result<Vec<u8>, GraphicsError> {
    // A network path is refused from the raw bytes, before the path is so much
    // as resolved: resolving it is what reaches out to the host, so a check
    // that ran later could not prevent the connection.
    if !allow_network_paths && NETWORK_PATHS_REACHABLE && names_a_network_share(&source.path) {
        log::debug!("Refusing to read graphics image file from a network path");
        return Err(GraphicsError::FileUnreadable);
    }

    let path = native_path(&source.path);

    // The policy check has to happen before the file is opened: merely
    // opening a file can have side effects, and whether the open succeeds
    // tells the client whether a file it may not read exists at all.
    let canonical = canonical_allowed(&path).ok_or_else(|| {
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

    let result = check_opened(&path, &file).and_then(|()| {
        let data = read_window(&file, source, max_to_read)?;
        // A raw window holding less than the pixels the control data declares
        // is reported as unreadable rather than as missing data, so that a
        // client cannot learn how short the file was. Windows that may still
        // need processing (compressed data or PNG) are judged by that
        // processing instead.
        if max_to_read < MAX_DATA_SIZE && data.len() < max_to_read {
            log::debug!("Graphics image file {} is shorter than its image", path.display());
            return Err(GraphicsError::FileUnreadable);
        }
        Ok(data)
    });

    // Close the descriptor before deleting anything: Windows refuses to
    // remove a file that is still open, so a temporary file would be left
    // behind if the removal happened while the handle were alive.
    drop(file);

    if source.temp {
        remove_temp(&source.path, &canonical);
    }

    result
}

/// Check a freshly opened file again, now that its descriptor is held.
///
/// The name could have been pointed at a different file between the policy
/// check that preceded the open and the open itself, so the checks run once
/// more against what was actually opened: the canonical path is resolved and
/// judged by the same policy, it must still name the file that was opened
/// (by device and inode on unix), and that file must be a regular one.
fn check_opened(path: &Path, file: &File) -> Result<(), GraphicsError> {
    let metadata = file.metadata().map_err(|error| {
        log::debug!("Failed to inspect graphics image file {}: {error}", path.display());
        GraphicsError::FileUnreadable
    })?;

    // The policy is checked once more against the canonical name, even where
    // the identity check below cannot run.
    let _canonical = canonical_allowed(path).ok_or_else(|| {
        log::debug!("Refusing to read graphics image file {}", path.display());
        GraphicsError::FileUnreadable
    })?;

    if !metadata.is_file() {
        log::debug!("Graphics image file {} is not a regular file", path.display());
        return Err(GraphicsError::FileUnreadable);
    }

    // The identity check needs the file's device and inode, which stay out of
    // reach on the platforms that do not expose them; the policy check above
    // has already been repeated against the canonical path there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let named = _canonical.metadata().map_err(|error| {
            log::debug!("Failed to inspect graphics image file {}: {error}", _canonical.display());
            GraphicsError::FileUnreadable
        })?;

        if metadata.dev() != named.dev() || metadata.ino() != named.ino() {
            log::debug!("Graphics image file {} changed under the terminal", path.display());
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
fn canonical_allowed(path: &Path) -> Option<PathBuf> {
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

    #[cfg(windows)]
    {
        if !windows_allowed(&canonical) {
            return None;
        }
    }

    Some(canonical)
}

/// Whether a canonical Windows path may be read.
#[cfg(windows)]
fn windows_allowed(canonical: &Path) -> bool {
    windows_allowed_form(&canonical.as_os_str().to_string_lossy())
}

/// Whether a canonical Windows path, as text, may be read.
///
/// Both a drive path and a share on a host name an ordinary file the user can
/// reach, and both are accepted. The device namespace reaches raw disks, whole
/// volumes and the kernel's own object tree, which the protocol has no business
/// reading, and the reserved DOS names are devices however ordinary they look,
/// so those are refused.
///
/// Compiled everywhere, even where the path form it judges cannot occur, so that
/// the judgement itself can be tested.
#[cfg(any(windows, test))]
fn windows_allowed_form(canonical: &str) -> bool {
    // A device path names a device in every spelling.
    if canonical.starts_with(r"\\.\") {
        return false;
    }

    // `canonicalize` yields the verbatim form for the paths it reaches
    // directly: `\\?\C:\...` for a drive and `\\?\UNC\host\share\...` for a
    // share. Judging the familiar spelling underneath is what makes the two
    // comparable, and a verbatim path that is neither is a volume, a device or
    // a pipe rather than a file, so it stays refused.
    let body = canonical.strip_prefix(r"\\?\").unwrap_or(canonical);

    let drive_path = {
        let bytes = body.as_bytes();
        bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
    };

    let share_path =
        if let Some(share) = body.strip_prefix(r"UNC\").or_else(|| body.strip_prefix("UNC/")) {
            names_a_host_and_share(share)
        } else if canonical == body {
            // The path needed no canonicalizing, so it may carry the plain spelling.
            body.strip_prefix(r"\\")
                .or_else(|| body.strip_prefix("//"))
                .is_some_and(names_a_host_and_share)
        } else {
            false
        };

    if !drive_path && !share_path {
        return false;
    }

    // Reserved device names are devices wherever they appear.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    let name = body.rsplit(['\\', '/']).find(|part| !part.is_empty()).unwrap_or(body);
    let stem = name.split('.').next().unwrap_or(name);

    !RESERVED.contains(&stem.to_ascii_uppercase().as_str())
}

/// Whether a share path names both a host and a share on it.
///
/// Anything shorter names no file at all.
#[cfg(any(windows, test))]
fn names_a_host_and_share(share: &str) -> bool {
    let mut parts = share.split(['\\', '/']).filter(|part| !part.is_empty());

    parts.next().is_some() && parts.next().is_some()
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
    source: &FileSource,
    max_to_read: usize,
) -> Result<Vec<u8>, GraphicsError> {
    let length = file.metadata().map_err(|_| GraphicsError::FileUnreadable)?.len();
    let offset = u64::from(source.offset);

    // A client supplied size bounds the read; without one the read runs to the
    // end of the file. Either way it is capped, since the file may be far
    // larger than the image it holds.
    let available = length.saturating_sub(offset);
    let wanted = if source.size == 0 { available } else { u64::from(source.size) };
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

    #[test]
    fn a_network_share_is_named_by_a_doubled_separator() {
        // Both spellings reach out to a host, so both are refused when the
        // configuration says paths may not leave this machine.
        assert!(names_a_network_share(br"\\host\share\pic.png"));
        assert!(names_a_network_share(b"//host/share/pic.png"));

        // Ordinary paths are not network paths, in either direction.
        assert!(!names_a_network_share(br"C:\pic.png"));
        assert!(!names_a_network_share(b"/tmp/pic.png"));
        assert!(!names_a_network_share(br"\pic.png"));
    }

    #[test]
    fn an_ordinary_windows_file_is_allowed() {
        assert!(windows_allowed_form(r"\\?\C:\Users\me\pic.png"));
        assert!(windows_allowed_form(r"C:\Users\me\pic.png"));
        assert!(windows_allowed_form("C:/Users/me/pic.png"));

        // A mapped network drive is an ordinary drive path.
        assert!(windows_allowed_form(r"\\?\Z:\photos\pic.png"));
    }

    #[test]
    fn a_windows_network_share_is_allowed() {
        // `canonicalize` yields the verbatim spelling, which must be accepted
        // just as the familiar one is.
        assert!(windows_allowed_form(r"\\?\UNC\host\share\pic.png"));
        assert!(windows_allowed_form(r"\\host\share\pic.png"));
        assert!(windows_allowed_form("//host/share/pic.png"));
        assert!(windows_allowed_form(r"\\?\UNC\host\share"));
    }

    #[test]
    fn the_windows_device_namespace_is_refused() {
        // These are not files, however much they look like paths: raw disks,
        // whole volumes and the kernel's own object tree.
        assert!(!windows_allowed_form(r"\\.\PhysicalDrive0"));
        assert!(!windows_allowed_form(r"\\?\GLOBALROOT\Device\HarddiskVolume1\pic.png"));
        assert!(!windows_allowed_form(r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\pic.png"));
        // A share path has to name a share on a host.
        assert!(!windows_allowed_form(r"\\host"));
        assert!(!windows_allowed_form("//host"));
    }

    #[test]
    fn the_reserved_device_names_are_refused() {
        assert!(!windows_allowed_form(r"C:\Users\me\NUL"));
        assert!(!windows_allowed_form(r"\\.\CON"));
        assert!(!windows_allowed_form(r"\\?\C:\COM1"));
        assert!(!windows_allowed_form(r"C:\Users\me\nul.png"));

        // A name that merely begins like a device is an ordinary file.
        assert!(windows_allowed_form(r"C:\Users\me\console.png"));
        assert!(windows_allowed_form(r"C:\Users\me\nulled.png"));
    }
}
