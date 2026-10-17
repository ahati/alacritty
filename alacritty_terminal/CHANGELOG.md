# Changelog

All notable changes to alacritty_terminal are documented in this file. The
sections should follow the order `Added`, `Changed`, `Deprecated`, `Fixed` and
`Removed`.

**Breaking changes are written in bold style.**

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## 0.26.1-dev

### Added

- Kitty graphics protocol support. Decoded images are kept in the layout they were
  transmitted in, so an uncompressed transmission needs no per-pixel conversion, and
  decoding runs away from the terminal lock via `Term::take_graphics_jobs` and
  `Term::install_graphics`

### Changed

- **Transmitting an image no longer decodes it before returning**, so the reply that
  reports whether a payload was accepted now follows its decode
- **`Decoded` and `ImageData` carry a `PixelFormat`**, since pixels are no longer
  always RGBA

### Fixed

- Panic when the PTY could not be set to non-blocking
- Off-by-one in ViMotion::ParagraphUp
- Unbounded per-cell memory usage for zero-width cells
- Unsoundness in `Row::new` when `columns == 0`
- Placements being dropped on a column resize, instead of surviving it as they do
  in the reference implementation
- Placements being destroyed once they scrolled above the viewport, rather than once
  the line they are anchored to left the buffer. A placement is now measured by its
  whole extent, so one taller than the screen survives with it
- Images being rejected once the graphics memory limit was reached, instead of the
  oldest images being evicted. Transient images (`N=1`) are evicted before older
  ones that are not transient

## 0.26.0

### Added

- New `escape_args` field on `tty::Options` for Windows shell argument escaping control

### Changed

- Pass `-q` to `login` on macOS if `~/.hushlogin` is present
- **`ChildEvent::Exited` and `Event::ChildExit` now contain `ExitStatus` instead of `i32`**

## 0.25.0

### Changed

- Replaced `Options::hold` with `Options::drain_on_exit`

## 0.24.2

### Added

- Escape sequence to move cursor forward tabs ( CSI Ps I )

## 0.24.1

### Changed

- Shell RCs are no longer sourced on macOs

### Fixed

- Semantic search handling of fullwidth characters
- Inline search ignoring line wrapping flag
- Clearing of `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID` in the main process
- FD leaks when closing PTYs on Unix
- Crash when ConPTY creation failed

## 0.24.0

### Added

- `tty::unix::from_fd()` to create a TTY from a pre-opened PTY's file-descriptors

### Changed

- **`Term` is not focused by default anymore**
