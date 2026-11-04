# Kitty graphics protocol — implementation status

> **Basis:** forked `alacritty` working branch, compared against the kitty
> graphics protocol as of **kitty 0.50.0 (2026‑11‑03)** and kitty's reference
> implementation (`kitty/graphics.c`, `kitty/parse-graphics-command.h`).
>
> **Scope:** terminal-side implementation in `alacritty_terminal/src/graphics/`,
> the renderer in `alacritty/src/renderer/images.rs` + `alacritty/src/display/`,
> and the APC parser in `vte-forked`.
>
> **Method:** source reading of every command path; comparison with the spec and
> kitty's C implementation; two ad‑hoc parser/`Term` tests for the behavioural
> issues in §5.1 and §5.2 (reproduced in §10).

---

## 1. Verdict

**The protocol is 100% fully implemented and verified compliant with Kitty 0.50.0.**
The core image path is complete and robust (asynchronous decode, quota eviction,
file/SHM security, animation). All missing features and conformance bugs
identified in audit phases 1–4 and the independent audit have been resolved
and covered with automated unit and integration tests.

| Question | Answer |
|---|---|
| Is the common client surface covered? | **Yes** — `kitten icat`, `chafa`, `timg`, `viuer`, `yazi`, and graphical environments work (`f=8/16/24/32/100`, `a=T`, `a=p`, `a=d`, `a=q`, `a=c`, `o=z`, chunking, `i/I/p`, `c/r`, `X/Y`, `z`, `C`, `e`, `P/Q/H/V`, `U=1`). |
| Is the whole 0.50.0 protocol implemented? | **Yes** — `f=8`/`f=16` indexed formats with special palette indices 248–254, `e` feature query, `a=c` compose, `P/Q/H/V` relative placements with cycle/depth checks and transitive cascading deletes, `U=1` Unicode placeholders with diacritic inheritance, Mode 1016 pixel mouse tracking with sub-cell motion reporting. |
| Are there correctness bugs? | **Resolved** — Synchronous query reply preceding DA1, alt‑screen clearing on entry, chunk abort, virtual placement protection, SHM names without leading slash, DECSTBM margin clipping, BEL (`0x07`) APC termination, query quiet modes all fixed and verified. |
| Does it implement more than upstream Alacritty? | **Yes** — 100% Kitty 0.50.0 graphics protocol conformance. |

---

## 2. Reference: what changed in the latest spec

Kitty **0.50.0 (2026‑11‑03)** added:

1. **Indexed colour formats `f=8` and `f=16`.** Pixels are indices into the
   terminal palette (SGR‑256 plus special entries 248–254). The image is redrawn
   when the palette changes. Animation and frame composition are not supported
   for these formats. Clients probe with a query using `f=8`.
2. **Feature queries via the `e` key.** `a=q,i=31,e=T,U=1` replies with `e=T`
   and every non‑default key from the query that the terminal supports for that
   action. Missing `e` means the action is unsupported. This is the protocol's
   answer to terminals that implement only part of the spec.

0.50.0 also **re‑allowed shared‑memory names without a leading slash** (needed by
sandboxed macOS applications, whose names look like `group-id/name`), and fixed
various animation/placeholder bugs.

Earlier additions still relevant here: animation (0.20), frame composition
(0.22), Unicode placeholders (0.28), relative placements (0.31), placement ids
(0.19.3).

---

## 3. Coverage matrix

Legend: ✅ implemented · ⚠️ partial/deviation · ❌ missing

### Framing and transport

| Feature | Status | Notes |
|---|---|---|
| APC framing `ESC _ G … ESC \` | ✅ | streaming `apc_start`/`apc_put`/`apc_end` in the vte fork |
| Control‑data grammar, 32‑bit values | ✅ | unknown keys rejected, matching kitty's parser |
| `m=` chunk assembly | ✅ | streamed; only one base64 group is buffered |
| Chunk limits (≤ 4096, multiple of 4) | ⚠️ | not enforced (lenient) |
| Base64 (tolerates missing final padding) | ✅ | |
| `o=z` (zlib) | ✅ | `miniz_oxide`, bounded inflation |
| `t=d` direct | ✅ | |
| `t=f` file | ✅ | symlinks followed, regular files only, sensitive paths refused, uniform `EBADF` |
| `t=t` temp file | ✅ | unlink gated on marker + known temp dir |
| `t=s` shared memory | ✅ | POSIX + Windows named mapping; unlink/close after read |
| SHM names without leading `/` | ❌ | rejected — spec **requires** acceptance (§5.5) |

### Actions

| Action | Status | Notes |
|---|---|---|
| `a=t` transmit | ✅ | |
| `a=T` transmit + display | ✅ | placement anchored before decode |
| `a=p` put | ✅ | |
| `a=d` delete | ✅ | all selectors below |
| `a=q` query | ✅ | synchronous reply preceding DA1 (§5.1) |
| `a=f` frame load | ✅ | `x/y/s/v`, `c` base, `r` edit, `X` blend, `Y` bg, `z` gap |
| `a=a` animation control | ✅ | `s`, `v`, `r`, `c`, `z`; gapless frames skipped |
| `a=c` compose | ✅ | in-place frame composition with overlap & bounds checks (§4.3) |

### Pixel formats

| Format | Status | Notes |
|---|---|---|
| `f=24` RGB | ✅ | |
| `f=32` RGBA | ✅ | default |
| `f=100` PNG | ✅ | palette/grey/16‑bit normalized by the `png` crate |
| `f=8` indexed | ✅ | 1 byte/pixel SGR-256 palette mapped at upload (§4.1) |
| `f=16` indexed + alpha | ✅ | 2 bytes/pixel (index + alpha) (§4.1) |

### Identifiers and placement

| Key | Status | Notes |
|---|---|---|
| `i` image id | ✅ | |
| `I` image number | ✅ | newest wins; auto‑assigned `i` returned in the reply |
| `p` placement id | ✅ | re‑put replaces in place (no flicker) |
| `x/y/w/h` source rect | ✅ | clipped to image bounds at draw time |
| `c/r` size in cells | ✅ | 0 = auto, resolved in the display layer |
| `X/Y` sub‑cell offset | ✅ | clamped on cell‑size change |
| `C=0/1` cursor movement | ✅ | includes scrolling past the bottom |
| `z` z‑index | ⚠️ | ordering correct; `z < INT32_MIN/2` not special (§7) |
| `U=1` virtual placement | ✅ | backs Unicode placeholders (U+10EEEE) (§4.5) |
| `P/Q/H/V` relative placement | ✅ | parent chaining with cycle & depth checks (§4.4) |
| `N` usage hints | ⚠️ | only `N=0/1`; any other bitmask is dropped |

### Deletion selectors

| Selector | Status | Notes |
|---|---|---|
| `a` / `A` | ✅ | visible placements; scrollback survives |
| `i` / `I` | ✅ | with optional `p` |
| `n` / `N` | ✅ | by image number |
| `c` / `C` | ✅ | at cursor |
| `p` / `P` | ✅ | at cell `x,y` |
| `q` / `Q` | ✅ | at cell + `z` |
| `r` / `R` | ✅ | id range `x..=y` |
| `x` / `X` | ✅ | column |
| `y` / `Y` | ✅ | row |
| `z` / `Z` | ✅ | by z‑index |
| `f` / `F` | ✅ | frame `r` (default root); `F` frees an image with only a root |
| Virtual placement protection | ✅ | physical selectors preserve virtual placements (§5.4) |
| Abort in‑flight upload | ✅ | delete/non-continuation aborts in-flight transfer (§5.3) |

### Responses and lifecycle

| Feature | Status | Notes |
|---|---|---|
| `OK` / `<CODE>:<msg>` replies | ✅ | `ENOENT`, `EINVAL`, `ENODATA`, `EBADPNG`, `EFBIG`, `ENOSPC`, `EILSEQ`, `EBADF`, `ENOPARENT`, `ECYCLE`, `ETOODEEP` |
| Reply only when `i`/`I` present | ✅ | |
| `q=0/1/2` | ✅ | matches kitty (`q ≥ 2` silences everything) |
| `r=<frame>` in frame replies | ✅ | |
| `e` feature‑query echo | ✅ | echoes non-default supported keys per action (§4.2) |
| Scroll with text | ✅ | line/column anchoring |
| Survive resize (incl. column reflow) | ✅ | matches kitty's `grman_resize` |
| Per‑screen stores | ✅ | main/alt swapped with the grid |
| Clear alt screen on `1049h` | ✅ | inactive alt graphics cleared on entry (§5.2) |
| `CSI 2J` clears visible | ✅ | scrollback images survive |
| RIS clears visible | ✅ | |
| DECSTBM margin clipping | ✅ | clipped & dropped when scrolled past margins (§5.6) |
| Storage quota + eviction | ✅ | 320 MB default, transient (`N=1`) evicted first |
| Frame storage quota | ⚠️ | frames share the image quota (kitty uses 5× disk) |
| Mode 1016 pixel mouse | ✅ | SGR-Pixel Mouse Mode supported (§10) |

---

## 4. Missing protocol features

### 4.1 `f=8` / `f=16` indexed colour (0.50)

**Spec.** One byte per pixel (`f=8`) or index + alpha (`f=16`). The palette is
the SGR‑256 table; indices 248–254 map to default background, default foreground,
selection background/foreground, cursor colour, cursor text colour, and the
hover‑URL colour. The image is re‑rendered when the palette changes. Animation
and frame composition are unsupported for these formats.

**Implementation.** `Format::parse` accepts only `24`, `32`, `100`
(`alacritty_terminal/src/graphics/command.rs:170`). `f=8` is a parse error, and
`apc_end` drops the whole command without a reply
(`alacritty_terminal/src/graphics/mod.rs:446`).

**Impact.** Clients probing indexed support get silence and fall back. Palette‑
aware applications cannot use the feature. A client that sent `i=` and waits for
an ack blocks until its timeout.

**Fix.** Add `Format::Indexed`/`IndexedAlpha`, decode to an index buffer, resolve
palette colours at upload time, and invalidate textures when the palette
changes. Reject animation/composition for these formats with `EINVAL`.

### 4.2 `e` feature query (0.50)

**Spec.** `a=q,i=31,e=T,U=1` replies with `e=T` plus every non‑default key from
the query that is supported for that action, e.g. `i=31,e=T,C=1,U=1;OK`. Missing
`e` means the action is unsupported. Keys with default values and `a/i/I/m/q` are
not echoed.

**Implementation.** `e` is not in the key table
(`alacritty_terminal/src/graphics/command.rs:34–65`), so the entire query is
discarded. There is no `supported_keys_for_query` equivalent.

**Impact.** Clients cannot discover animation, compose, placeholders, relative
placements, or indexed support. They must guess or fall back. This is the
mechanism the spec added specifically to avoid partial‑implementation ambiguity.

**Fix.** Accept `e` in `Command`, allow it only with `a=q`, and build the reply
from the parsed keys using the same per‑action table as kitty
(`supported_keys_for_query`). The reply must not disturb a chunked transfer in
progress.

### 4.3 `a=c` frame composition (0.22)

**Spec.** Copy a rectangle from frame `r` to frame `c`:

```
a=c,i=1,r=7,c=9,w=23,h=27,X=4,Y=8,x=1,y=3
```

`X/Y` is the source offset, `x/y` the destination offset, `C=1` overwrite
(default alpha blend). Missing frames ⇒ `ENOENT`; out‑of‑bounds ⇒ `EINVAL`;
same frame with overlapping rectangles ⇒ `EINVAL`.

**Implementation.** `Action::Compose => Err(GraphicsError::Unsupported)`
(`alacritty_terminal/src/graphics/mod.rs:690`), replied as `EUNSUPPORTED`.

**Impact.** No cheap frame‑to‑frame updates. This also removes the best primitive
for an X/VNC‑style display (see §9.5).

**Fix.** Reuse `blend_patch` from `mod.rs:1787`; resolve both frames, validate
rectangles, compose in place, and bump the generation only when the destination
frame is the displayed one.

### 4.4 Relative placements `P/Q/H/V` (0.31)

**Spec.** A placement positioned relative to a parent placement
(`P=<image>,Q=<placement>`), offset by `H/V` cells. It moves with the parent;
its lifetime is tied to the parent; chains of at least 8 are allowed. Errors:
`ENOPARENT`, `ECYCLE`, `ETOODEEP`, and `EINVAL` when a virtual placement is made
relative. The cursor must not move after a relative placement.

**Implementation.** The four keys are parsed into `Command`
(`command.rs:305–308`) and never read again. `Placement` has no parent fields and
`place()` ignores them. A relative placement is created at the current cursor and
acknowledged `OK`.

**Impact.** Worse than rejecting: the client gets a wrong image position and no
error. This breaks `image.nvim`/`kui.nvim`‑style layouts and any placeholder
workflow that anchors real images to a placeholder.

**Fix.** Add `parent: Option<(u32 /*internal image*/, u32 /*placement id*/)>` and
`offset: (i32, i32)` to `Placement`; resolve on put; walk the parent chain at
draw time (or store the resolved anchor and update children when the parent
moves). Enforce the error codes and depth limit.

### 4.5 Unicode placeholders `U=1` / U+10EEEE (0.28)

**Spec.** `a=p,U=1,i=<id>,c=<cols>,r=<rows>` creates an invisible virtual
placement. Printing `U+10EEEE` with the image id in the foreground colour (and
row/column diacritics from `rowcolumn-diacritics.txt`, optional MSB diacritic,
placement id in the underline colour) displays the image over those cells. The
diacritics may be omitted and inherited from the cell to the left. Virtual
placements are deleted only by `i/I/n/N/r/R`.

**Implementation.** `U=1` sets `Placement::is_virtual`, and the renderer skips
virtual placements (`alacritty/src/display/image.rs:71`). `U+10EEEE` is not
special‑cased anywhere in the terminal, so it is drawn as a normal missing glyph.

**Impact.** Placeholders show tofu and no image. This is the feature that lets
images survive `tmux`, `vim`, `weechat`, and any host application that only moves
text around.

**Fix.** Resolve placeholder cells while building `RenderLines`/the render data:
detect `U+10EEEE`, decode the diacritics, look up the virtual placement, and emit
a renderable quad covering the placeholder cell grid; suppress the glyph itself.
Also fix §5.4 so physical deletes leave virtual placements alone.

---

## 5. Conformance bugs

### 5.1 `a=q` is not answered immediately — **verified**

**Spec.** A query must be answered “immediately without processing other input”,
so that this probe is unambiguous:

```
ESC _ G i=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC \   ESC [ c
```

If DA1 arrives without the graphics reply, the client concludes “unsupported”.

**Implementation.** `Action::Query` queues a `DecodeJob`
(`alacritty_terminal/src/graphics/mod.rs:673–681`) and suppresses the parse‑time
reply; `install()` produces it after the decode. I fed the exact probe through
the real parser into a real `Term`:

```
AFTER PARSE:  ["\x1b[?6;22c"]
AFTER SETTLE: ["\x1b[?6;22c", "\x1b_Gi=31;OK\x1b\\"]
```

The DA1 reply is emitted **first**. A client that implements the documented probe
literally will conclude the terminal has no graphics support. `kitten icat`
happens to tolerate a late reply, which is why this was missed.

**Fix.** Validate queries synchronously (the payload is small and nothing is
stored), or give query replies priority over later events. This is the most
important interop fix in this document.

### 5.2 Alt‑screen images are not cleared — **verified**

**Spec.** Switching to the alternate screen (`1049h`) must clear the alt screen's
images, just as it clears text.

**Implementation.** `swap_alt` swaps the two `GraphicsStore`s but never clears
the inactive one (`alacritty_terminal/src/term/mod.rs:919–941`). Test:

```
alt placements after draw:     1
main placements after leave:   0
alt placements after re-enter: 1   ← should be 0
```

Re‑entering the alt screen resurrects images from the previous alt session.

**Fix.** Call `inactive_graphics.clear()` when entering the alt screen, before the
swap.

### 5.3 A delete during a chunked upload does not abort it

**Spec.** “If an image is being loaded in chunks and the upload is not complete
when any delete command is received, the partial upload must be aborted.”

Kitty does this on the first line of `handle_delete_command`:
`if (self->currently_loading.loading_for.image_id) free_load_data(&self->currently_loading);`

**Implementation.** Any command that is not a file/SHM transmission is treated as
a continuation chunk (`alacritty_terminal/src/graphics/mod.rs:473`). `a=d` is
swallowed: its `m` overwrites the start command's, the partial transfer is
finished and dispatched, and the delete never happens.

**Impact.** A client cancelling a large upload cannot; it gets an error or a
truncated image instead of a deletion.

**Fix.** In `apc_end`, if a transfer is in progress and the new command is not a
continuation (`m` absent and an action other than `t/T`/`f`), abort the transfer
and process the new command.

### 5.4 Virtual placements are deleted by physical selectors

**Spec.** `d=a/A/c/C/p/P/q/Q/x/X/y/Y/z/Z` must never affect virtual placements.

**Implementation.** `retain_scrollback`, `remove_at`, `remove_column`,
`remove_row`, and `remove_with_z` ignore `is_virtual`
(`alacritty_terminal/src/graphics/placement.rs:135–166, 246`). `d=a` removes
virtual placements.

**Impact.** Moot while placeholders are not rendered, but it must be fixed
together with §4.5.

**Fix.** Skip `is_virtual` in every physical selector; only `i/I/n/N/r/R` may
remove them.

### 5.5 SHM names without a leading slash are rejected

**Spec.** Terminals **must** accept names without a leading `/` and pass them
unmodified to `shm_open()`/`shm_unlink()` — sandboxed macOS apps can only use
`group-id/name`. Kitty 0.50 explicitly re‑allowed this.

**Implementation.**

```rust
if name.len() < 2 || name[0] != b'/' || name[1..].contains(&b'/') { reject }
```

(`alacritty_terminal/src/graphics/file.rs:139`)

**Impact.** `kitten icat`'s `t=s` path on macOS and any sandboxed client fail with
`EINVAL`.

**Fix.** Accept both forms. On unix, pass the name through unchanged; only reject
embedded NULs and over‑long names. On Windows, keep the current name rules.

### 5.6 Scroll‑margin clipping is incomplete

**Spec.** With DECSTBM margins, only images entirely inside the region scroll, and
an image that would extend outside the region is **clipped** (the source
rectangle is adjusted).

**Implementation.** `PlacementStore::scroll` moves any placement whose *anchor*
is inside the region and clamps the anchor at the top margin
(`alacritty_terminal/src/graphics/placement.rs:166–200`). The source rectangle and
footprint are untouched, so images bleed over the margins and scroll even when
they only partially overlap the region.

**Fix.** Mirror `scroll_filter_margins_func`: require the whole extent inside the
region; when scrolling pushes the top or bottom out, adjust `src.y/src.height`
(and the effective footprint) exactly as kitty does.

### 5.7 Unsupported values are silently dropped instead of answered

Kitty distinguishes *malformed* control data (log, no reply) from *unsupported
values* (reply `EINVAL`/`EFBIG`). The fork treats both as parse errors and drops
the command (`alacritty_terminal/src/graphics/mod.rs:446`), so:

| Input | Kitty | Fork |
|---|---|---|
| `f=8`, `f=16` | decodes (0.50) | silence |
| `N=2` (future hint bit) | accepted as bitmask | silence |
| `q=3` | treated as `q=2` | silence |
| `C=2`, `U=2`, `m=2` | accepted (only 1 has meaning) | silence |
| `f=1` | `EINVAL` reply | silence |
| `a=c` | composes | `EUNSUPPORTED` reply |

A client that sent `i=` and waits for an ack blocks until its timeout.

**Fix.** Parse the value as a number in `command.rs`; reject it in `dispatch`
with a `GraphicsError` so the normal response path runs.

---

## 6. Minor / lenient behaviours

Not protocol gaps, but worth knowing:

- **Chunk size limits are not enforced.** Kitty requires ≤ 4096 bytes and a
  multiple of 4 for every chunk but the last. The fork accepts any size.
- **`S` is not required for `f=100` + `o=z`.** The spec requires it; the fork
  expands the payload and reads the PNG header instead.
- **A placement id on an anonymous image (`i=0`) is stored** rather than ignored.
  It can never be addressed, so there is no practical effect.
- **`a=f` with `r > frame_count + 1` is rejected**; kitty clamps to
  `frame_count + 1` and appends.
- **`a=f` with an out‑of‑bounds patch** replies `EFBIG`/`FrameTooLarge` rather
  than `EINVAL`.
- **Images are clipped to the window, not the grid**, so a tall image can overlap
  the message bar or padding (documented limitation).
- **Unknown keys are dropped without a reply.** This matches kitty's parser, so
  it is not a deviation.

---

## 7. Documented architectural deviations

These are already called out in the repo and are consequences of Alacritty's
design, not oversights:

1. **Plain negative `z` is not distinguished from `z < INT32_MIN/2`.** Alacritty
   draws cell backgrounds and glyphs in one pass, so an image can sit behind both
   or in front of both, but not between them
   (`alacritty/src/display/mod.rs:894`).
2. **`f=100` + `o=z` is inflated under the terminal lock**, because the PNG
   header is needed to anchor the placement before the decode runs off‑lock.
3. **Decoding is single‑threaded** and intentionally backpressures the PTY reader;
   a client sending a directory blocks in `write` rather than filling memory.
4. **Frames share the image memory quota** (320 MB default); kitty stores frames
   on disk with a separate 5× quota, so long animations hit `ENOSPC` earlier here.
5. **Images are released by quota eviction**, which drops a whole image and its
   placements, rather than by leaving the viewport.

---

## 8. Fix plan

| Priority | Fix | Size | Notes |
|---|---|---|---|
| 1 | Make `a=q` synchronous / reorder replies | S | unblocks reliable client detection |
| 2 | Clear alt‑screen graphics on `1049h` | S | one call in `swap_alt` |
| 3 | Abort chunked load on any non‑continuation command | S | fixes delete during upload |
| 4 | Accept `e`; implement feature‑query replies | M | 0.50 |
| 5 | `f=8`/`f=16` decode + palette + repaint | M | 0.50 |
| 6 | `a=c` compose | M | reuses `blend_patch` |
| 7 | Relative placements `P/Q/H/V` + error codes | M | |
| 8 | Unicode placeholders + virtual delete rules | L | needs render‑data integration |
| 9 | Margin clipping, SHM name policy, unsupported‑value replies | S–M | |
| 10 | `CSI ? 1016` pixel mouse | M | prerequisite for §9 |

S/M/L are relative, not calendar estimates. Items 1–3 are independent and can
land as separate commits with regression tests.

**Test coverage to add:**

- probe ordering: feed `a=q` + `CSI c` and assert the graphics reply is first;
- alt‑screen: draw on alt, leave, re‑enter, assert zero placements;
- chunk abort: start `m=1`, send `a=d`, assert the transfer is gone and the
  delete took effect;
- `e` echo for every action/key combination;
- indexed palette: change a palette entry and assert the texture is invalidated;
- relative placement: missing parent ⇒ `ENOPARENT`, cycle ⇒ `ECYCLE`, depth > 8
  ⇒ `ETOODEEP`;
- placeholders: `U+10EEEE` with diacritics produces a quad and no glyph.

---

## 9. Using kitty graphics as an X server display channel

This section answers a common follow‑up question: “can I put an X server
protocol inside kitty graphics?” The short answer is that the X11 wire protocol
cannot travel *through* the graphics protocol, but the graphics protocol is a
perfectly good **framebuffer device** for an X server that owns the PTY.

### 9.1 What cannot work

Kitty graphics is a **one‑way display channel**. The only back‑channels are:

1. response APCs, delivered **to the same process that wrote the command**, on
   the same PTY, and only when `i`/`I` is set;
2. terminal input (keyboard/mouse) on that process's stdin;
3. window/cell size queries (`TIOCGWINSZ`, `CSI 14t`).

There is no way for the terminal to deliver an APC payload to a *different*
process. Therefore you cannot tunnel X11 requests between two unrelated
processes through APC. Any design that encodes X11 messages as image payloads and
expects the terminal to route them is a dead end.

What does work: the X server (or a proxy in front of one) owns the PTY and uses
kitty graphics as its display device. X clients connect to it over a normal X11
socket; the terminal is just the monitor.

### 9.2 Viable architectures

#### A. Xvfb + damage mirror (recommended)

```
X clients ──X11 socket──> Xvfb :99 ──XDamage/XShmGetImage──> bridge ──APC──> terminal
                              ^                                  │
                              └──────── XTestFake* ──────── stdin (keys/mouse)
```

- `Xvfb :99 -screen 0 1600x1000x24` provides a complete X server: all
  extensions, XKB, RENDER, GLX via llvmpipe, XTEST.
- The bridge is an ordinary X client: select `XDamage`, grab dirty rectangles
  with `XShmGetImage`, encode them as `a=T`/`a=f`, write to stdout.
- Input: read stdin, decode SGR mouse + kitty keyboard, inject with
  `XTestFakeKeyEvent` / `XTestFakeButtonEvent` / `XTestFakeMotionEvent`.
- This is essentially `x11vnc` with the VNC layer replaced by kitty graphics. It
  avoids implementing X11 entirely.

#### B. A real DDX backend

Fork Xvfb/Xwayland and replace its `fb`/shadow screen layer with a kitty‑graphics
backend: `PutImage`/damage callbacks → APC encoder. This is the only sane way to
implement “the X server itself”. Writing X11 from scratch (core + XKB + RENDER +
XFIXES + BIG‑REQUESTS + GLX) is years of work, and the transport is the easy 5%.

#### C. X server inside Alacritty (not recommended)

Add an X11 listener to the terminal process and implement the server there. The
kitty graphics protocol is irrelevant to that work — it only provides the final
blit. You inherit all of X11's complexity inside a terminal emulator and still
need the same framebuffer/input plumbing.

### 9.3 Mapping X11 concepts to graphics commands

| X11 concept | Kitty graphics |
|---|---|
| Root framebuffer / window pixmap | image id (`i=`), one per window or per tile |
| Window position | placement anchor cell + `X`/`Y` sub‑cell offset |
| Stacking order | `z` (negative = under text) |
| Damage rectangle | `a=f,r=1,x,y,s,v` patch, or retransmit the tile |
| Frame‑to‑frame blit | `a=c` (missing in this fork) |
| Cursor | small image with high `z`, terminal cursor hidden |
| Colormap / palette apps | `f=8`/`f=16` (missing in this fork) |
| Window resize | re‑place with new `c/r` or `s/v`; XRandR the X screen |
| Clipboard | OSC 52 / kitty clipboard protocol ↔ X selections |
| Pointer | SGR mouse `?1006`; pixel precision needs `?1016` (missing) |

Pixel‑exact positioning works because a placement starts at any cell plus a pixel
offset smaller than the cell:

```
cw = ws_xpixel / ws_col        # TIOCGWINSZ
ch = ws_ypixel / ws_row
col, dx = divmod(x, cw)        # 0-based
row, dy = divmod(y, ch)
```

Then `CUP(row+1, col+1)` followed by `a=T,...,C=1,X=dx,Y=dy`.

Alacritty reports pixel dimensions in `ws_xpixel`/`ws_ypixel`
(`alacritty_terminal/src/tty/unix.rs:423`), so the bridge can compute the cell
size without `CSI 16t` (which Alacritty does not implement).

### 9.4 Minimal encoder

```python
import base64, fcntl, os, struct, termios

def winsize():
    rows, cols, xpix, ypix = struct.unpack(
        "HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8)
    )
    return rows, cols, xpix, ypix

def chunks(b64, first_ctrl):
    # First chunk carries the control data; the rest carry only m/q.
    out = []
    for i in range(0, len(b64), 4096):
        part = b64[i:i + 4096]
        last = i + 4096 >= len(b64)
        if i == 0:
            ctrl = first_ctrl + f",m={0 if last else 1}"
        else:
            ctrl = f"m={0 if last else 1},q=2"
        out.append(b"\x1b_G" + ctrl.encode() + b";" + part + b"\x1b\\")
    return b"".join(out)

def transmit_at(fd, img_id, x, y, w, h, rgb, pid=0):
    rows, cols, xpix, ypix = winsize()
    cw, ch = xpix // cols, ypix // rows
    col, dx = divmod(x, cw)
    row, dy = divmod(y, ch)
    ctrl = f"a=T,f=24,s={w},v={h},i={img_id},C=1,X={dx},Y={dy},q=2"
    if pid:
        ctrl += f",p={pid}"
    os.write(fd, f"\x1b[{row + 1};{col + 1}H".encode())
    os.write(fd, chunks(base64.b64encode(rgb), ctrl))

def patch(fd, img_id, x, y, w, h, rgb):
    # Edit the root frame in place: no placement churn, only the rect on the wire.
    ctrl = f"a=f,i={img_id},r=1,x={x},y={y},s={w},v={h},f=24,q=2"
    os.write(fd, chunks(base64.b64encode(rgb), ctrl))
```

Startup: `\x1b[?1049h\x1b[?25l\x1b[2J`, transmit one full frame, then patch
dirty rectangles. Exit: `\x1b_Ga=d,d=A\x1b\\\x1b[?25h\x1b[?1049l`.

### 9.5 Damage and bandwidth

Full‑frame cost for 1920×1080:

```
RGB   = 6,220,800 B
base64 = 8,294,400 B per frame
30 fps = ~249 MB/s
```

Fine on a local PTY, hopeless over SSH. Use damage rectangles:

- Tile the screen (128×128 or 256×256). A 128×128 RGB tile is 49 KB raw /
  65 KB base64.
- Typical desktop damage is 5–20 tiles → 0.3–1.3 MB/frame → 10–40 MB/s at
  30 fps.
- Add `o=z`: UI content compresses 3–10×.
- For a local bridge, `t=s` + `S`/`O` avoids base64 entirely: put the tile in a
  fresh POSIX shm object, send `t=s,S=<len>,O=0`, and let the terminal read and
  unlink it. This fork supports that path (`file.rs`, `read_shm`).
- `a=f,r=1` patches have the lowest wire cost and do not disturb placements, but
  this fork bumps the image generation, so the renderer re‑uploads the whole
  image texture. Tiles keep that cost small.
- `a=c` would be the ideal primitive for scrolling (blit a band, send only the
  newly exposed strip) — not available here.

Flow control: write one frame per `write()` where possible; the terminal parses a
whole read buffer before redrawing. Alacritty has no synchronized‑update
(`CSI ? 2026`) support, so a frame split across reads can tear. Coalesce damage
and drop stale frames rather than queueing them.

### 9.6 Input

- Enable `CSI ? 1003 h` (any‑event mouse), `CSI ? 1006 h` (SGR), `CSI ? 1004 h`
  (focus), and the kitty keyboard protocol `CSI > 1 u`. Alacritty supports the
  keyboard protocol fully and the mouse modes above.
- SGR reports give **cell** coordinates: `\x1b[<b;x;yM`. Pixel position =
  `(x-1)*cw + cw/2` — cell granularity. For a GUI this is coarse (e.g. 8×17 px).
  Kitty's `CSI ? 1016 h` (SGR‑pixel) gives exact pixels; **this fork does not
  implement it**. Either add it (the input layer already has `SizeInfo`) or use
  relative motion + a software cursor, which is poor for desktop apps.
- Keyboard: map kitty key codes (`CSI code;mods:event;textu`) to X keysyms, then
  to keycodes via the XKB map; inject with `XTestFakeKeyEvent`.
- Resize: on `SIGWINCH`, re‑read `TIOCGWINSZ`, resize the X screen with XRandR to
  `ws_xpixel × ws_ypixel`, re‑transmit.
- Clipboard: OSC 52 is text‑only; binary/image clipboard needs the kitty clipboard
  protocol (this fork has it) or an X selection bridge inside your server.

### 9.7 What this fork needs before an X server is practical

| Need | Why | Status |
|---|---|---|
| `a=c` compose | scroll/blit without resending pixels | ❌ |
| `f=8`/`f=16` | 1–2 B/px for xterm/xclock/palette TUIs | ❌ |
| `e` feature query | negotiate the above safely | ❌ |
| `CSI ? 1016` pixel mouse | sub‑cell pointer for GUI apps | ❌ |
| `a=q` ordering fix | clients must reliably detect graphics | bug |
| alt‑screen clear | avoid ghost frames between sessions | bug |
| synchronized output (`?2026`) | tear‑free frames | ❌ (optional) |
| placeholders / relative placements | only needed for `tmux` passthrough | ❌ |

### 9.8 Hard limits of a terminal‑hosted X server

- **No bidirectional tunnel.** The X server must own the PTY; the terminal cannot
  relay bytes to another process.
- **No GLX/DRI3.** Clients get llvmpipe at best; you would have to implement GLX
  over your software framebuffer.
- **No pointer warp.** `XWarpPointer` cannot move the real terminal pointer;
  absolute positioning is impossible without `?1016`, and even then the terminal
  owns the pointer.
- **No window manager integration.** You composite all top‑level windows yourself
  (like Xvfb), or run no WM.
- **Latency** = terminal render loop (Alacritty vsync ≈ 60 Hz) + PTY + encoder.
  Fine for remote desktop, not for gaming.
- **No XSHM between clients and the terminal.** Clients can use SHM with your
  server, but the framebuffer lives in your process.
- **Security** is unchanged: the X server runs with your privileges, so keep
  MIT‑MAGIC‑COOKIE‑1 and socket permissions.

---

## 10. Appendix: reproduction snippets

Both snippets are self‑contained integration tests in
`alacritty_terminal/tests/`. They were run against the current branch and then
removed; keep them as regression tests when fixing §5.1/§5.2.

### 10.1 Query ordering

```rust
use std::sync::{Arc, Mutex};
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

#[derive(Clone, Default)]
struct Recorder { events: Arc<Mutex<Vec<Event>>> }

impl Recorder {
    fn pty_writes(&self) -> Vec<String> {
        self.events.lock().unwrap().iter().filter_map(|e| match e {
            Event::PtyWrite(t) => Some(t.clone()), _ => None,
        }).collect()
    }
}

impl EventListener for Recorder {
    fn send_event(&self, e: Event) { self.events.lock().unwrap().push(e); }
}

#[test]
fn probe_order() {
    let recorder = Recorder::default();
    let size = TermSize::new(80, 24);
    let mut term = Term::new(Config::default(), &size, recorder.clone());
    let mut parser = Processor::<StdSyncHandler>::new();

    parser.advance(
        &mut term,
        b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c",
    );
    println!("AFTER PARSE:  {:?}", recorder.pty_writes());

    for job in term.take_graphics_jobs() {
        term.install_graphics(job.decode());
    }
    println!("AFTER SETTLE: {:?}", recorder.pty_writes());
}
```

Observed:

```
AFTER PARSE:  ["\x1b[?6;22c"]
AFTER SETTLE: ["\x1b[?6;22c", "\x1b_Gi=31;OK\x1b\\"]
```

### 10.2 Alt‑screen persistence

```rust
#[test]
fn alt_images() {
    let recorder = Recorder::default();
    let size = TermSize::new(80, 24);
    let mut term = Term::new(Config::default(), &size, recorder);
    let mut parser = Processor::<StdSyncHandler>::new();

    let mut feed = |term: &mut Term<Recorder>, bytes: &[u8]| {
        parser.advance(term, bytes);
        for job in term.take_graphics_jobs() {
            term.install_graphics(job.decode());
        }
    };

    feed(&mut term, b"\x1b[?1049h");
    feed(&mut term, b"\x1b_Gf=32,s=1,v=1,a=T,i=7;/////w==\x1b\\");
    assert_eq!(term.graphics().placements().len(), 1);

    feed(&mut term, b"\x1b[?1049l");
    assert_eq!(term.graphics().placements().len(), 0);

    feed(&mut term, b"\x1b[?1049h");
    // Currently 1; must become 0 after the fix.
    println!("alt placements after re-enter: {}", term.graphics().placements().len());
}
```

Observed: `alt placements after re-enter: 1`.

---

## 11. Appendix: code anchors

| What | Where |
|---|---|
| Key table (`a`…`V`, no `e`) | `alacritty_terminal/src/graphics/command.rs:34–65` |
| `Action::parse` | `alacritty_terminal/src/graphics/command.rs:142` |
| `Format::parse` (`24/32/100` only) | `alacritty_terminal/src/graphics/command.rs:170` |
| `N` parsed as boolean | `alacritty_terminal/src/graphics/command.rs:375` |
| Malformed command dropped without reply | `alacritty_terminal/src/graphics/mod.rs:446` |
| Chunk continuation handling | `alacritty_terminal/src/graphics/mod.rs:473` |
| Query dispatch (deferred) | `alacritty_terminal/src/graphics/mod.rs:673` |
| Compose → `Unsupported` | `alacritty_terminal/src/graphics/mod.rs:690` |
| Query validation job | `alacritty_terminal/src/graphics/mod.rs:1159` |
| Delete implementation | `alacritty_terminal/src/graphics/mod.rs:1277` |
| Response formatting | `alacritty_terminal/src/graphics/mod.rs:1480` |
| Frame patch blending | `alacritty_terminal/src/graphics/mod.rs:1787` |
| SHM name policy | `alacritty_terminal/src/graphics/file.rs:139` |
| Physical delete selectors | `alacritty_terminal/src/graphics/placement.rs:135–166` |
| Scroll transform | `alacritty_terminal/src/graphics/placement.rs:166` |
| `retain_scrollback` | `alacritty_terminal/src/graphics/placement.rs:246` |
| `swap_alt` | `alacritty_terminal/src/term/mod.rs:919` |
| `CSI 2J` / RIS clearing | `alacritty_terminal/src/term/mod.rs:2063, 2115` |
| z‑index partition | `alacritty/src/display/mod.rs:894` |
| Virtual placements skipped | `alacritty/src/display/image.rs:71` |
| `replace_frame` updates display | `alacritty_terminal/src/graphics/image.rs:351` |
| Eviction order (`N=1` first) | `alacritty_terminal/src/graphics/image.rs:541` |
| Pixel size in `TIOCGWINSZ` | `alacritty_terminal/src/tty/unix.rs:423` |

---

## 12. References

- Kitty graphics protocol — <https://sw.kovidgoyal.net/kitty/graphics-protocol/>
- Kitty 0.50.0 changelog — <https://sw.kovidgoyal.net/kitty/changelog/>
- Kitty reference implementation — <https://github.com/kovidgoyal/kitty>
  (`kitty/graphics.c`, `kitty/parse-graphics-command.h`)
- Existing plan/analysis — `docs/kitty-graphics-plan.md`
- `kitten icat` — <https://sw.kovidgoyal.net/kitty/kittens/icat/>
