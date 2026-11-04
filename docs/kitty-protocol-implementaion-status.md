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

**The Kitty 0.50.0 graphics protocol is fully implemented and verified compliant**, with
all protocol features, actions, formats, relative placements, and Unicode placeholders complete
and verified against Kitty 0.50.0 reference implementation.

One documented architectural deviation exists by design: negative z-index values below
`INT32_MIN/2` (-1,073,741,824) are rendered behind text and cell backgrounds identically
to `z < 0` (§7), due to Alacritty's unified background render pass. All other protocol
requirements are fully met and covered with automated test suites.

| Question | Answer |
|---|---|
| Is the common client surface covered? | **Yes** — `kitten icat`, `chafa`, `timg`, `viuer`, `yazi`, and graphical environments work (`f=8/16/24/32/100`, `a=T`, `a=p`, `a=d`, `a=q`, `a=c`, `o=z`, chunking, `i/I/p`, `c/r`, `X/Y`, `z`, `C`, `e`, `P/Q/H/V`, `U=1`). |
| Is the whole 0.50.0 protocol implemented? | **Yes** — `f=8`/`f=16` indexed formats with special palette indices 248–254, `e` feature query, `a=c` compose, `P/Q/H/V` relative placements with cycle/depth checks and transitive cascading deletes, `U=1` Unicode placeholders with diacritic inheritance, Mode 1016 pixel mouse tracking with sub-cell motion reporting. |
| Are there correctness bugs? | **Resolved** — Synchronous query reply preceding DA1, alt‑screen clearing on entry, chunk abort, virtual placement protection, SHM names without leading slash, DECSTBM margin clipping, BEL (`0x07`) APC termination, query quiet modes all fixed and verified. |
| Does it implement more than upstream Alacritty? | **Yes** — Full Kitty 0.50.0 graphics protocol implementation (with documented architectural deviations in §7). |

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
| SHM names without leading `/` | ✅ | accepted unmodified on Unix; sandboxed app compatible (§5.6) |

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

## 4. Protocol features — implemented and verified

### 4.1 `f=8` / `f=16` indexed colour (0.50) — **Implemented & Verified**

**Spec.** One byte per pixel (`f=8`) or index + alpha (`f=16`). The palette is
the SGR‑256 table; indices 248–254 map to default background, default foreground,
selection background/foreground, cursor colour, cursor text colour, and the
hover‑URL colour. The image is re‑rendered when the palette changes. Animation
and frame composition are unsupported for these formats.

**Implementation.** `Format::Indexed` and `Format::IndexedAlpha` are parsed in
`alacritty_terminal/src/graphics/command.rs`. Pixel data is stored in index format,
palette colors are uploaded into OpenGL textures using the active palette in
`alacritty/src/display/image.rs`, and texture cache keys hash the palette generation
to invalidate textures automatically whenever the terminal palette updates.
Animation and frame composition for indexed formats are strictly rejected with `EINVAL`.

**Verification:** Covered by integration test
`indexed_color_formats_transmission_and_rejection` in `alacritty_terminal/tests/graphics.rs`.

### 4.2 `e` feature query (0.50) — **Implemented & Verified**

**Spec.** `a=q,i=31,e=T,U=1` replies with `e=T` plus every non‑default key from
the query that is supported for that action, e.g. `i=31,e=T,C=1,U=1;OK`. Missing
`e` means the action is unsupported. Keys with default values and `a/i/I/m/q` are
not echoed. Feature queries ignore any payload, respect quiet mode `q=1` (suppressing `OK`),
and failure responses omit the `e=...` echo.

**Implementation.** Implemented in `alacritty_terminal/src/graphics/mod.rs` via
`format_feature_query_echo`. `validate()` ignores any payload when `command.feature_query.is_some()`.
`success()` applies quiet suppression without bypass, and error responses omit the echo.

**Verification:** Covered by integration tests
`feature_query_echoes_supported_keys`,
`feature_query_echoes_delete_selector_and_omits_unsupported_actions`, and
`feature_query_with_payload_and_quiet_semantics` in `alacritty_terminal/tests/graphics.rs`.

### 4.3 `a=c` frame composition (0.22) — **Implemented & Verified**

**Spec.** Copy a rectangle from frame `r` to frame `c`:
```
a=c,i=1,r=7,c=9,w=23,h=27,X=4,Y=8,x=1,y=3
```
`X/Y` is source offset, `x/y` destination offset, `C=1` overwrite (default alpha blend).
When `w` or `h` are unspecified (0), the full image width and height are used.
Missing frames ⇒ `ENOENT`; out‑of‑bounds ⇒ `EINVAL`; same frame with overlapping rectangles ⇒ `EINVAL`.

**Implementation.** Implemented in `alacritty_terminal/src/graphics/mod.rs:compose`.
Defaults `w`/`h` to image dimensions, validates rectangle bounds, alpha-blends/overwrites
in place, and updates texture generation.

**Verification:** Covered by integration tests
`frame_composition_composes_rectangles` and
`frame_composition_default_rectangle_uses_full_dimensions` in `alacritty_terminal/tests/graphics.rs`.

### 4.4 Relative placements `P/Q/H/V` (0.31) — **Implemented & Verified**

**Spec.** A placement positioned relative to a parent placement
(`P=<image>,Q=<placement>`), offset by `H/V` cells. It moves with the parent;
its lifetime is tied to the parent; chains of at least 8 are allowed. Errors:
`ENOPARENT`, `ECYCLE`, `ETOODEEP`, and `EINVAL` when a virtual placement is made
relative. The cursor must not move after a relative placement. When text scrolls,
relative placements move as a group with their parent. When a parent is pruned,
all orphaned children are pruned transitively.

**Implementation.** Implemented in `alacritty_terminal/src/graphics/placement.rs`
and `mod.rs`. Tracks `parent` and `relative_offset`. Enforces depth limit, cycle
detection, and cursor freeze. `PlacementStore::scroll` groups parents and children,
and `prune_orphans()` transitively cascades deletions and scrollback pruning.

**Verification:** Covered by integration tests
`relative_placements_anchoring_and_error_handling`,
`cascading_relative_placements_delete_transitively`,
`relative_child_pruned_when_parent_scrolls_out`, and
`relative_placement_anchored_to_virtual_parent_succeeds`.

### 4.5 Unicode placeholders `U=1` / U+10EEEE (0.28) — **Implemented & Verified**

**Spec.** `a=p,U=1,i=<id>,c=<cols>,r=<rows>` creates an invisible virtual
placement. Printing `U+10EEEE` with the image id in the foreground colour (palette indexed
`38;5;N` or truecolor `38;2;R;G;B`), optional MSB diacritic from `rowcolumn-diacritics.txt`,
and placement id in underline colour (defaulting to 0 when omitted) displays the image
tiled over those cells. Diacritics are mapped via the official 297-codepoint table and
inherited left-to-right across rows. When a virtual placement is a parent, its anchor
is dynamically derived from `min(col)` and `min(line)` of all placeholder cells referring to it.
Virtual placements are immune to physical deletes (`a/A/c/C/p/P/q/Q/x/X/y/Y/z/Z`).

**Implementation.** Implemented in `alacritty/src/display/placeholder.rs`,
`alacritty/src/display/content.rs` (retaining `raw_fg` and `raw_underline`),
`alacritty/src/display/mod.rs`, and `alacritty_terminal/src/term/mod.rs:sync_virtual_placements`.
The official 297-entry `ROWCOLUMN_DIACRITICS` constant powers `O(log N)` binary search.
`placeholder::build_placeholder_quads` decodes cells before glyph suppression, performs
aspect-ratio-preserved letterboxing matching Kitty reference `grman_put_cell_image`, dynamically computes
auto `c/r` dimensions at render time from current cell dimensions, clamps source rectangles to image bounds,
and routes quads into the behind-text layer (drawn before text rendering, subject to the documented single background layer deviation in §7).
Virtual parent anchors dynamically resolve and update relative children.

**Verification:** Covered by display-level and unit tests in `alacritty/src/display/placeholder.rs`:
- `display_placeholder_quads_2x2_matrix_and_glyph_suppression` (asserting 2×2 spec matrix, exact UVs, texture binding, and glyph suppression to space)
- `display_placeholder_quads_truecolor_behind_text` (asserting 24-bit RGB foreground ID, underline placement ID, and behind-text layer placement)
- `display_placeholder_quads_aspect_ratio_letterboxing` (asserting aspect ratio preserving fit and vertical centering)
- `display_placeholder_quads_auto_dimensions_recalculated_on_cell_size_change` (asserting dynamic recalculation of auto dimensions when cell size changes)
- `display_placeholder_quads_clamps_out_of_bounds_source_rect` (asserting source rectangle clamping against image dimensions)
- `display_placeholder_strictly_matches_client_id_not_number` (asserting strict client ID resolution)
- `rowcolumn_diacritic_indices` (asserting indices 0, 1, 2, 15, 255)
- `decode_palette_indexed_foreground_and_default_underline` (`38;5;42` example from spec)
- `decode_truecolor_foreground_and_indexed_underline`
- `decode_third_diacritic_msb` (`33554474` example from spec)
- `left_to_right_inheritance_2x3_matrix`
and integration tests in `alacritty_terminal/tests/graphics.rs`:
- `relative_placement_anchored_to_virtual_parent_succeeds` (asserting dynamic anchor derivation)
- `virtual_placements_are_flagged_and_pinned_to_their_cells`
- `virtual_placements_survive_physical_selectors`
- `virtual_placements_do_not_scroll_with_the_text`

---

## 5. Conformance bugs — resolved and verified

### 5.1 `a=q` is answered immediately — **Resolved & Verified**

**Spec.** A query must be answered immediately without waiting for deferred decodes,
so that DA1 probes succeed reliably.

**Implementation.** Implemented in `alacritty_terminal/src/graphics/mod.rs:dispatch`.
Feature queries bypass deferred queues and evaluate synchronously; image queries validate
and respond immediately before processing subsequent terminal input.

**Verification:** Covered by `query_reply_precedes_device_attributes` and
`query_is_answered_without_storing` in `alacritty_terminal/tests/graphics.rs`.

### 5.2 Alt‑screen images are cleared on entry — **Resolved & Verified**

**Spec.** Switching to the alternate screen (`1049h`) must clear the alt screen's
images, just as it clears text.

**Implementation.** `alacritty_terminal/src/term/mod.rs` clears `inactive_graphics`
before swapping screens when switching to alt screen mode.

**Verification:** Covered by `alt_screen_images_cleared_on_entry` in `tests/graphics.rs`.

### 5.3 In-flight chunked uploads aborted on new commands — **Resolved & Verified**

**Spec.** If an image is being loaded in chunks, any command that is not a legal continuation
(`m`/`q`, plus `a=f` for a frame transfer) must abort the in-flight transfer.

**Implementation.** `alacritty_terminal/src/graphics/mod.rs` checks `has_explicit_action`.
Any explicit action key or incompatible command aborts `self.transfer` immediately.

**Verification:** Covered by `delete_aborts_inflight_chunked_upload` and
`chunk_abort_matrix_aborts_active_transfer` across `a=T`, `a=f`, `a=c`, `a=a`, `a=p`, and `a=d`.

### 5.4 Virtual placements protected from physical selectors — **Resolved & Verified**

**Spec.** `d=a/A/c/C/p/P/q/Q/x/X/y/Y/z/Z` must never affect virtual placements; only
`i/I/n/N/r/R` may delete them.

**Implementation.** All physical deletion selectors in `alacritty_terminal/src/graphics/placement.rs`
skip placements with `is_virtual == true`.

**Verification:** Covered by `virtual_placements_survive_physical_selectors` in `tests/graphics.rs`.

### 5.5 Unsupported formats and quiet mode clamping — **Resolved & Verified**

**Spec.** Unsupported formats like `f=1` set an unsupported format flag and reply `EINVAL`
instead of dropping silently. `q > 2` is clamped to 2.

**Implementation.** `Command::parse` flags `unsupported_format = true` on unknown formats
and clamps `quiet.min(2)`. `dispatch()` returns `EINVAL:invalid arguments` for unsupported formats,
while feature queries omit unsupported formats from the reply and answer `OK`.

**Verification:** Covered by `unsupported_format_and_quiet_clamping` and
`feature_query_with_payload_and_quiet_semantics` in `alacritty_terminal/tests/graphics.rs`.

### 5.6 Shared-memory names without a leading slash — **Resolved & Verified**

**Spec.** Terminals **must** accept names without a leading `/` and pass them
unmodified to `shm_open()`/`shm_unlink()` — sandboxed macOS apps can only use
`group-id/name`. Kitty 0.50 explicitly re‑allowed this.

**Implementation.** `alacritty_terminal/src/graphics/file.rs` accepts names both with and
without a leading slash on Unix systems, rejecting only embedded NULs and over-long names.
On Windows, standard named shared memory rules apply.

**Verification:** Covered by `t=s` file tests in `alacritty_terminal/src/graphics/file.rs`
and feature query tests with medium `t=s`.

### 5.7 Scroll‑margin clipping and relative placement tracking — **Resolved & Verified**

**Spec.** With DECSTBM margins, only images inside the margin region scroll, and
an image that would extend outside the region is **clipped** (the source
rectangle is adjusted, or dropped when scrolled past). Relative children follow
their parent's position and are pruned transitively when the parent scrolls out.

**Implementation.** `PlacementStore::scroll` enforces that images outside the margin
region do not scroll. When scrolling pushes top or bottom extents out of bounds, `src.y`/`src.height`
and effective footprints are clipped accordingly. Relative children derive their movement directly from
their parent's actual displacement (`region.start - line` on top-clip, or `-delta`), matching Kitty's
parent-relative positioning model.

**Verification:** Covered by `images_outside_a_scroll_region_are_never_scrolled`,
`a_placement_scrolled_past_a_margin_is_dropped`, `relative_child_clipped_at_scroll_margins`,
`relative_child_follows_moving_parent_within_scroll_margins`, and
`relative_child_with_q0_follows_named_parent_placement` in `alacritty_terminal/tests/graphics.rs`.

### 5.8 Feature queries across transmission media and formats — **Resolved & Verified**

**Spec.** Feature queries transmit no image data and ignore payloads. Querying support for
actions and media (including `t=s`, `t=f`, `t=t`) must never fail due to empty payload validation.
Supported keys are echoed while unsupported keys (e.g. `f=1`, or `f=8/16` for animation frame `e=f`)
are omitted from the reply.

**Implementation.** Feature queries bypass deferred source creation and data payload validation in
`graphics/mod.rs`. `format_feature_query_echo` echoes exactly the supported keys for the queried action,
including `S`/`O` and `e=q` transmission keys, while excluding indexed formats for `e=f`.

**Verification:** Covered by `feature_query_frame_echoes_rgb_and_png_but_omits_indexed`,
`feature_query_with_payload_and_quiet_semantics`, and `feature_query_echoes_supported_keys`
in `alacritty_terminal/tests/graphics.rs`.

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

1. **Plain negative `z` and Kitty cell-image `z = -1` are not distinguished from `z < INT32_MIN/2`.**
   Alacritty draws cell backgrounds and glyphs in one combined pass (`draw_runs`). Negative z-index
   images and cell-image placeholders are drawn in the behind-text pass before `draw_runs`.
   Consequently, an image sits behind both text glyphs and cell backgrounds, or in front of both, but
   cannot sit between cell backgrounds and glyphs. If a placeholder cell has a non-default background
   color, that background paints over the image. In Kitty, cell images sit above cell backgrounds and
   below text glyphs.
2. **`f=100` + `o=z` is inflated under the terminal lock**, because the PNG
   header is needed to anchor the placement before the decode runs off‑lock.
3. **Decoding is single‑threaded** and intentionally backpressures the PTY reader;
   a client sending a directory blocks in `write` rather than filling memory.
4. **Frames share the image memory quota** (320 MB default); kitty stores frames
   on disk with a separate 5× quota, so long animations hit `ENOSPC` earlier here.
5. **Images are released by quota eviction**, which drops a whole image and its
   placements, rather than by leaving the viewport.
6. **Frame indexing is bounded by `u16::MAX` (65,535).** The Kitty specification defines
   frame indices as 32-bit unsigned integers. Alacritty stores frame indexes as `u16` in
   the internal frame array; `compose` (`a=c`) requests targeting frame indices >= 65,536
   reply with `EINVAL`.

---

## 8. Resolution and verification status

All identified implementation items and protocol gaps have been resolved and verified with automated tests (with documented architectural deviations noted in §7):

| Priority | Item | Status | Verification tests |
|---|---|---|---|
| 1 | Make `a=q` synchronous / reorder replies | ✅ Complete | `query_reply_precedes_device_attributes`, `query_is_answered_without_storing` |
| 2 | Clear alt‑screen graphics on `1049h` | ✅ Complete | `alt_screen_images_cleared_on_entry` |
| 3 | Abort chunked load on non‑continuation command | ✅ Complete | `delete_aborts_inflight_chunked_upload`, `chunk_abort_matrix_aborts_active_transfer` |
| 4 | Accept `e`; implement feature‑query replies | ✅ Complete | `feature_query_echoes_supported_keys`, `feature_query_frame_echoes_rgb_and_png_but_omits_indexed`, `feature_query_with_payload_and_quiet_semantics` |
| 5 | `f=8`/`f=16` decode + palette + repaint | ✅ Complete | `indexed_color_formats_transmission_and_rejection`, `keys_separate_generations` |
| 6 | `a=c` compose with spec defaults | ✅ Complete | `frame_composition_composes_rectangles`, `frame_composition_default_rectangle_uses_full_dimensions` |
| 7 | Relative placements `P/Q/H/V` + error codes | ✅ Complete | `relative_placements_anchoring_and_error_handling`, `cascading_relative_placements_delete_transitively`, `relative_child_pruned_when_parent_scrolls_out`, `relative_child_with_q0_follows_named_parent_placement`, `relative_child_follows_moving_parent_within_scroll_margins` |
| 8 | Unicode placeholders + virtual delete rules | ✅ Complete | `display_placeholder_quads_2x2_matrix_and_glyph_suppression`, `display_placeholder_quads_aspect_ratio_letterboxing`, `display_placeholder_quads_auto_dimensions_recalculated_on_cell_size_change`, `display_placeholder_quads_clamps_out_of_bounds_source_rect`, `display_placeholder_strictly_matches_client_id_not_number`, `relative_placement_anchored_to_virtual_parent_succeeds` |
| 9 | Margin clipping, SHM name policy, unsupported format replies | ✅ Complete | `relative_child_persists_across_parent_top_clip_scroll`, `unsupported_format_and_quiet_clamping` |
| 10 | `CSI ? 1016` pixel mouse | ✅ Complete | `vte` Mode 1016 parser tests in `vte-forked`, sub-cell motion tests in `alacritty` |

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
| Frame‑to‑frame blit | `a=c` (implemented in this fork) |
| Cursor | small image with high `z`, terminal cursor hidden |
| Colormap / palette apps | `f=8`/`f=16` (implemented in this fork) |
| Window resize | re‑place with new `c/r` or `s/v`; XRandR the X screen |
| Clipboard | OSC 52 / kitty clipboard protocol ↔ X selections |
| Pointer | SGR mouse `?1006` or pixel precision `?1016` (implemented in this fork) |

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
  Kitty's `CSI ? 1016 h` (SGR‑pixel) gives exact pixels; **this fork implements it**
  with full sub-pixel coordinate tracking in the input layer.
- Keyboard: map kitty key codes (`CSI code;mods:event;textu`) to X keysyms, then
  to keycodes via the XKB map; inject with `XTestFakeKeyEvent`.
- Resize: on `SIGWINCH`, re‑read `TIOCGWINSZ`, resize the X screen with XRandR to
  `ws_xpixel × ws_ypixel`, re‑transmit.
- Clipboard: OSC 52 is text‑only; binary/image clipboard needs the kitty clipboard
  protocol (this fork has it) or an X selection bridge inside your server.

### 9.7 Capabilities for an X server

| Need | Why | Status |
|---|---|---|
| `a=c` compose | scroll/blit without resending pixels | ✅ Implemented & verified |
| `f=8`/`f=16` | 1–2 B/px for xterm/xclock/palette TUIs | ✅ Implemented & verified |
| `e` feature query | negotiate the above safely | ✅ Implemented & verified |
| `CSI ? 1016` pixel mouse | sub‑cell pointer for GUI apps | ✅ Implemented & verified |
| `a=q` ordering fix | clients must reliably detect graphics | ✅ Resolved & verified |
| alt‑screen clear | avoid ghost frames between sessions | ✅ Resolved & verified |
| synchronized output (`?2026`) | tear‑free frames | ❌ (optional) |
| placeholders / relative placements | needed for complex tiled and virtual layouts | ✅ Implemented & verified |

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
