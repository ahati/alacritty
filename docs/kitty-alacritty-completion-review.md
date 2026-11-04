# Kitty graphics — completion review

> **Branch:** `kitty-graphics` (HEAD `83fa99d5`, 32 commits ahead of
> `origin/kitty-graphics`)
>
> **Reviewed state:** the **uncommitted working tree** on top of that commit —
> 14 files, +1,311/−104 — snapshot hash
> `d57b78cb012d33f1b711830b665f41964deb532241c5a53c45507e2fe6b240ad`.
>
> **Reference:** kitty **0.50.0** (2026‑11‑03), kitty's `graphics.c` /
> `parse-graphics-command.h`, and the protocol specification.
>
> **Method:** source review of every changed path, comparison against the
> specification and the reference implementation, and the repository's test
> suites. No source files were modified for this review. `cargo test` and
> `cargo check` were run; only build artifacts were written.
>
> **Snapshot note:** the working tree was edited during the review (the diff
> grew from +1,078 to +1,311 lines). Findings below are pinned to the snapshot
> hash above. Re-check the hash before acting on any line number.

---

## 1. Verdict

**Not complete.** The merged commits are in good shape and the working tree
implements the five previously missing protocol features, but the
"100% fully implemented and verified compliant" claim in
`docs/kitty-protocol-implementaion-status.md` is not supported by the current
code.

| Area | Verdict |
|---|---|
| Committed `kitty-graphics` history (graphics, animation, clipboard, ligatures) | **Good** — tests green |
| Indexed colour `f=8`/`f=16` | **Implemented**, palette path looks correct |
| `e` feature query | **Partial** — basic echo works; payload/quiet/error semantics wrong |
| `a=c` frame composition | **Partial** — rectangular blits work; spec defaults rejected |
| Relative placements `P/Q/H/V` | **Partial** — creation works; lifetime/scroll/virtual-parent semantics wrong |
| Unicode placeholders `U=1` / `U+10EEEE` | **Broken for the spec's own examples** |
| Mode 1016 pixel mouse | **Implemented**, but the vte change is uncommitted |
| Build reproducibility | **Broken** — path dependency on an uncommitted vte checkout |
| Status document | **Contradicts itself** |

The implementation is close, but six blocking defects remain. They are
concentrated in the placeholder path, `a=c`, feature queries, and relative
placement lifetime. All of them are fixable without architectural change.

---

## 2. Verification evidence

| Check | Result |
|---|---|
| `cargo test --locked -p alacritty_terminal --test graphics` | **81 passed**, 1 ignored |
| `cargo test --locked -p alacritty_terminal --test kitty_animation` | **8 passed** |
| `cargo test --locked -p alacritty_terminal --test kitty_clipboard` | **12 passed** |
| `cargo test --locked` in `vte-forked` (with uncommitted changes) | **40 passed** |
| `cargo check --locked -p alacritty` | **OK** |

Interpretation: the code compiles and its own regression tests pass. The tests
do **not** cover the failing paths below — in particular there is no test that
emits `U+10EEEE` at all (`grep -c 10EEEE alacritty_terminal/tests/*.rs` = 0).

The `xkitty_end_to_end_integration` test passed because
`/usr/local/bin/Xkitty` exists in this container. It returns early when the
binary is absent, so it is a local smoke test, not a portable conformance test.

---

## 3. Completion criteria

| # | Criterion | Status |
|---|---|---|
| 1 | Every 0.50.0 action, format, medium, selector and key is parsed | **Yes** |
| 2 | `f=8`/`f=16` transmit, display, palette refresh | **Yes** |
| 3 | `e` feature query reports exactly the supported keys | **No** — P0‑4, P1‑10 |
| 4 | `a=c` composes with all documented defaults | **No** — P0‑3 |
| 5 | `P/Q/H/V` positions, cascades, and lifetime are correct | **No** — P0‑5, P1‑7, P1‑8 |
| 6 | `U=1` placeholders display the image for the documented encodings | **No** — P0‑1, P0‑2 |
| 7 | `a=q` replies before subsequent input | **Yes** |
| 8 | Alt screen, margin clipping, chunk abort, virtual delete protection | **Mostly** — chunk abort is incomplete (P1‑9) |
| 9 | Build reproducible from a clean checkout | **No** — P1‑12 |
| 10 | Status document states the truth with evidence | **No** — P0‑6 |

---

## 4. Blocking findings (P0)

### 4.1 Unicode placeholders do not work for the spec's examples

**Where:** `alacritty/src/display/mod.rs:943–1010`.

The renderer detects `U+10EEEE`, replaces the glyph with a space, decodes the
image id from the foreground colour and the placement id from the underline
colour, then looks up a virtual placement. Two independent defects make the
lookup fail for the canonical encoding.

#### (a) The image id is decoded as RGB, not as the colour index

The specification's example is:

```
printf "\e[38;5;42m\U10EEEE\U0305\U0305\e[39m\n"
```

`42` is a **palette index**. Alacritty resolves cell colours to RGB before the
display layer sees them (`RenderableCell::compute_fg_rgb`,
`alacritty/src/display/content.rs:211`, implemented at `:326`), so the code computes:

```rust
image_id = (fg.r << 16) | (fg.g << 8) | fg.b
```

For palette entry 42 this is the RGB triplet of that entry (for example
`35071`), not `42`. The virtual placement was created with `i=42`, so
`img.client_id == image_id` never matches. Only truecolour
(`\e[38;2;R;G;Bm`) can encode a 24‑bit id here; the 8‑bit form the spec
demonstrates cannot work.

**Fix:** keep the colour index in the renderable cell (or carry the raw
`Color` alongside the resolved RGB) and use the index when the cell's colour is
a palette colour. Add a test for the `38;5;42m` form.

#### (b) The placement id defaults to the foreground, not zero

`RenderableCell.underline` is:

```rust
let underline = cell.underline_color().map_or(fg, |c| …);
```

(`alacritty/src/display/content.rs:284–286`)

When the client omits an underline colour — as the spec example does — the
placement id becomes the **foreground RGB**, not `0`. The lookup requires:

```rust
placement_id == 0 || p.placement_id == placement_id
```

A virtual placement created with `U=1` and no `p` has `placement_id == 0`, so
it is rejected. Even a truecolour image id (`38;2;0;0;42`) fails for the same
reason.

**Fix:** distinguish "no underline colour" from "underline colour equals the
foreground" and treat the former as placement id `0`.

#### Minimal reproducer to add

```rust
// after implementing, this must produce a 2x2 quad and no glyph
b"\x1b[38;5;42m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}"
b"\x1b[38;5;42m\u{10EEEE}\u{030D}\u{0305}\u{10EEEE}\u{030D}\u{030D}"
```

### 4.2 Diacritic decoding assumes a contiguous table

**Where:** `alacritty/src/display/mod.rs:954–960`.

```rust
explicit_row = Some((zerowidth[0] as u32).saturating_sub(0x0305));
explicit_col = Some((zerowidth[1] as u32).saturating_sub(0x0305));
```

Kitty's `rowcolumn-diacritics.txt` is **not** a contiguous block:

| index | codepoint |
|---|---|
| 0 | U+0305 |
| 1 | U+030D |
| 2 | U+030E |
| 3 | U+0310 |
| 4 | U+0312 |
| 5 | U+033D |
| … | … |

So `U+030D` (column 1) decodes as **8**, `U+030E` as **9**, `U+0310` as **11**.
Every placeholder cell other than `(0,0)` maps to the wrong tile, including the
spec's own 2×2 example.

**Fix:** embed a lookup table generated from `rowcolumn-diacritics.txt` (or the
Unicode 6.0 Mn/230 list it is derived from), and add a test for rows/columns
`0,1,2,15,255`.

### 4.3 `a=c` rejects the documented default rectangle

**Where:** `alacritty_terminal/src/graphics/mod.rs:1020–1022`.

```rust
let w = command.src_width;
let h = command.src_height;
if w == 0 || h == 0 {
    return Err(GraphicsError::Invalid);
}
```

The specification says: *"The size of the rectangle is specified as
`w=width,h=height` pixels. If unspecified, the full image width and height are
used."*

So the common full-frame form, `a=c,i=1,r=7,c=9`, returns
`EINVAL:invalid arguments`. The test only exercises the explicit
`w=1,h=1` form (`tests/graphics.rs:2060`), so this is missed.

**Fix:** when `w`/`h` are zero, substitute the image width/height before the
bounds checks; add a full-frame test.

### 4.4 Feature queries decode their payload

**Where:** `alacritty_terminal/src/graphics/mod.rs:1310–1332`.

```rust
if !data.is_empty() || command.source_width > 0 || command.source_height > 0 {
    decode_payload(command, data, limit, max_dimension)?;
}
```

The specification says: *"A feature query transmits no image data, any payload
is ignored, and it has no effect on images or on any chunked transmission in
progress."*

A feature query with a payload but no `s`/`v` reaches
`decode_raw(…, 0, 0)` and replies `EINVAL`. The canonical examples have no
payload, so the tests pass; a client that reuses a normal query shape (or a
multiplexer that appends bytes) fails.

**Fix:** in `validate`, return `Ok(())` immediately when
`command.feature_query.is_some()`; add a test with a non-empty payload.

### 4.5 Relative placement to a virtual parent uses the wrong anchor

**Where:** `alacritty_terminal/src/graphics/mod.rs:1441–1444`.

```rust
let anchor_line = Line(parent_p.anchor.line.0 + command.parent_offset_y);
```

For a virtual placement, `parent_p.anchor` is the cursor position when
`a=p,U=1` was issued. The specification requires the virtual parent's position
to be derived from **all actual placeholder images** that refer to it:

> When a virtual placement is the parent, its position is derived from all the
> actual Unicode placeholder images that refer to it. The x position is the
> minimum of all the placeholder x positions and the y position is the minimum
> of all the placeholder y positions.

The current test (`tests/graphics.rs:2368`) only asserts the stored
`parent`/`relative_offset`; it does not check the resulting `anchor`, so the
bug is invisible.

**Fix:** resolve virtual parents at draw time (or when placeholder cells
change) from the minimum placeholder cell, not from the creation-time cursor.
Add an anchor assertion.

### 4.6 The status document contradicts itself and overclaims

**Where:** `docs/kitty-protocol-implementaion-status.md`.

- Line 19: *"The protocol is 100% fully implemented and verified compliant with
  Kitty 0.50.0."*
- Line 156: **"## 4. Missing protocol features"** — still describes
  `f=8/16`, `e`, `a=c`, `P/Q/H/V`, `U=1` as missing.
- Line 267: **"## 5. Conformance bugs"** — still documents the bugs the header
  table now calls resolved.
- §9.7 still lists `a=c`, `f=8/16`, `e` and `1016` as missing.

The header table was updated; the body was not. "Verified" is also unsupported:
there is no placeholder test, and the relative-virtual-parent test does not
assert the anchor.

**Fix:** rewrite the document around evidence. Either keep the audit structure
and mark each item fixed with a test name, or replace it with a short
conformance statement that only claims what the tests prove.

---

## 5. High-severity findings (P1)

### 5.1 Relative children survive a pruned parent

`GraphicsStore::prune` (`graphics/mod.rs:546`) and `clear_visible` (`:592`)
call `PlacementStore::retain_within` / `retain_scrollback` but **not**
`prune_orphans`.

When a parent scrolls out of the buffer or the visible area, the parent is
removed and its children remain in the store with a dangling parent. They are
still drawn at their last anchor and can no longer be reached by
`shift_children`.

**Fix:** call `placements.prune_orphans()` after `retain_within` and
`retain_scrollback` (or fold it into `drop_orphaned_images`). Add a test that
scrolls a parent out of history and asserts the child is gone.

### 5.2 Relative children do not follow a parent through margin scrolling

`PlacementStore::scroll` (`placement.rs:240–310`) iterates every placement and
moves each one independently if its own anchor intersects the region.

A child whose `H`/`V` offset puts it outside the scrolling region stays where
it is while the parent moves, so the relative offset breaks. The new clipping
also mutates `src` and `footprint` destructively, so a later reverse scroll
cannot restore the original rectangle.

**Fix:** scroll a placement and its descendants as a group: move the parent by
the delta and apply the same delta to `shift_children`, or recompute children
from the parent after the parent's final anchor is known. Kitty's
`scroll_filter_margins_func` is the reference.

### 5.3 Chunk-abort heuristic misclassifies commands

`graphics/mod.rs:486–497`:

```rust
let is_continuation = command.delete.is_none()
    && command.action != Action::Delete
    && command.action != Action::Put
    && (command.image_id == 0 || command.image_id == existing.command.image_id);
```

A new `a=T`, `a=f`, `a=c` or `a=a` command with `i=0` (or the same `i` as the
in-flight transfer) is classified as a continuation and appended to the old
payload instead of aborting it. Only `a=d` is reliably fixed.

The specification requires an abort for any command that is not a legal
continuation (`m`/`q`, plus `a=f` for a frame transfer). The test
(`tests/graphics.rs:2003`) only covers `a=d`.

**Fix:** a continuation is a command with **no action key** (or `a=f` when the
start command was `a=f`) and no other control data except `m`/`q`. Everything
else aborts. Add a matrix test (`a=T`, `a=f`, `a=c`, `a=a`, `a=p`, `a=d`).

### 5.4 Feature-query quiet and error behaviour is wrong

`success()` (`graphics/mod.rs:1666`) forces a reply whenever
`feature_query.is_some()`, so `q=1` does not suppress the `OK`. Kitty applies
`q` to feature queries like any other response.

`failure()` goes through `response()`, which appends the `e=…` echo even on
errors. Kitty only appends keys when the response is `OK`.

**Fix:** remove the `feature_query` exception in `success`; pass a flag to
`response` so the echo is emitted only for successful queries. Add tests for
`a=q,i=31,e=T,U=1,q=1` and for an erroring feature query.

### 5.5 Dead `unsupported_format` flag; silent drops remain

`Command.unsupported_format` (`command.rs:344`) is declared and read
(`mod.rs:669`) but **never assigned**. The `KEY_FORMAT` arm still propagates
`Format::parse` errors (`command.rs:408`), so `f=1` is a parse error and the
whole command is dropped silently — the exact behaviour the flag was meant to
replace.

Similarly, `q=3` is still rejected by `command.rs:447` and dropped silently;
kitty accepts it as `q=2`.

**Fix:** either delete the dead field or wire it up as intended: parse unknown
format numbers into a flag, then reply `EINVAL` from `dispatch`. Clamp
`quiet` to `2` instead of rejecting. Add tests for `f=1` and `q=3`.

### 5.6 Build is not reproducible

- `Cargo.toml:31`: `vte = { path = "../vte-forked" }`
- `Cargo.lock` no longer pins the vte git source.
- `vte-forked` has **uncommitted** changes: `src/ansi.rs` (`1016` →
  `SgrPixelMouse`), `src/lib.rs` (BEL terminates APC).

This builds only in this container. A clean checkout, CI, or another machine
fails. The `Mode 1016` and BEL features are therefore not actually delivered.

**Fix:** commit and push the vte changes, then restore the git dependency (a
branch or a pinned rev) and regenerate `Cargo.lock`. Add the vte tests to the
branch that CI builds.

### 5.7 Deferred decode can drop the reply

`install` (`graphics/mod.rs:637–650`) returns `GraphicsUpdate::default()` when
`images.complete()` returns false because the image was evicted or deleted
while the decode ran. A client that sent `i=` waits for a reply that never
comes.

**Fix:** send `ENOENT` (or `EIO`) through the normal failure path; keep the
image's pending state consistent.

---

## 6. Medium findings (P2)

| # | Finding | Where |
|---|---|---|
| 1 | Placeholder image is stretched to the `c/r` grid instead of being letterboxed at its aspect ratio | `alacritty/src/display/mod.rs:1042–1048` |
| 2 | Placeholder quads ignore `z_index` and the virtual placement's `X/Y` offsets | `alacritty/src/display/mod.rs:1050–1060` |
| 3 | `p.internal_id == image_id` compares an internal id with a client id; can match the wrong image | `alacritty/src/display/mod.rs:1009` |
| 4 | MSB diacritic inheritance compares the partial `image_id`, so ids above 2²⁴ reset the inherited row/column | `alacritty/src/display/mod.rs:943–1000` |
| 5 | Palette index `255` (reserved → default foreground) maps to the last palette entry | `alacritty/src/display/mod.rs:826–835` |
| 6 | Indexed texture key is `generation ^ palette_hash`; XOR can collide across palettes | `alacritty/src/display/mod.rs:880, 1015` |
| 7 | `compose` casts the destination frame index to `u16` without a bound check, and copies the frame twice | `graphics/mod.rs:1073` |
| 8 | `shift_children` matches `Q=0` against every placement of the parent image, so an ambiguous parent can move unrelated children | `graphics/placement.rs:134–149` |
| 9 | `xkitty_end_to_end_integration` silently skips when the binary is absent and does not exercise placeholders/compose/relative placements | `tests/graphics.rs:2187` |
| 10 | Clipboard `walias` is parsed but a no-op; non-text MIME types are never stored; unfocused read/store requests get no reply | `alacritty_terminal/src/clipboard/mod.rs`, `alacritty/src/event.rs:1944` |
| 11 | Status-doc `N` row says only `N=0/1` is accepted; the code now accepts a bitmask and uses bit 0 | `docs/kitty-protocol-implementaion-status.md` |
| 12 | `a=q` with an invalid `e` action silently becomes a normal query (`Action::parse(...).ok()`) | `graphics/command.rs:444` |

---

## 7. Test coverage gaps

The new tests are a meaningful improvement — they cover the query ordering,
alt‑screen clear, delete abort, virtual placement protection, basic feature
echo, rectangular compose, relative placement creation/errors, indexed formats,
BEL termination, cascading deletes, and query interleaving. Missing:

| Gap | Why it matters |
|---|---|
| No `U+10EEEE` test at all | The entire placeholder path is unverified; P0‑1/P0‑2 would pass CI today |
| No `a=c` with omitted `w`/`h` | P0‑3 |
| No feature query with a payload | P0‑4 |
| No assertion of a relative placement's anchor when the parent is virtual | P0‑5 |
| No parent-pruned-by-buffer test | P1‑5.1 |
| No margin-scroll test for relative children | P1‑5.2 |
| Chunk abort tested only with `a=d` | P1‑5.3 |
| No `q=1` feature query test | P1‑5.4 |
| No `f=1` / `q=3` test | P1‑5.5 |
| `xkitty` test is environment-dependent | Gives false confidence elsewhere |

Suggested placement of the new tests:

- `alacritty_terminal/tests/graphics.rs` for compose, query, chunk, relative
  lifetime.
- A new `alacritty/tests/placeholder.rs` (or a unit test around the placeholder
  decoder extracted from `Display::draw`) for `U+10EEEE`, the diacritic table,
  the 256-colour id, and the underline default. The decoder should be a pure
  function to make this testable without a GPU.

---

## 8. What is complete and should be kept

- **Indexed colour path.** `Format::Indexed`/`IndexedAlpha`, raw decode,
  `bytes_per_pixel`, palette construction, and RGBA expansion at upload are
  coherent. The palette-hash invalidation is the right idea (fix the key
  composition in P2‑6).
- **Synchronous query path.** `Action::Query` is no longer deferred; the probe
  now replies before DA1, and the regression test proves it.
- **Alt-screen clearing.** `swap_alt` clears `inactive_graphics`; the test
  proves re-entry is clean.
- **Virtual placement protection.** Physical selectors skip virtual placements;
  `d=i` still removes them; `prune_orphans` cascades deletes.
- **Mode 1016.** The vte mode, mutual exclusion with SGR/UTF‑8 mouse, and the
  pixel-change motion path in `input/mod.rs` are a real improvement over
  cell-granular reporting.
- **Clipboard core.** Packet grammar, id filtering, size limits, permission
  model, chunked reads, and write outcomes are solid. `walias` and non-text
  MIME are the only protocol gaps.
- **Animation.** The merged `a=f`/`a=a`/`d=f/F` implementation passes its
  dedicated suite and integrates with the frame store.

---

## 9. Definition of done

The work can be called complete when all of the following are true:

1. `U+10EEEE` renders the spec's 2×2 example correctly from both
   `38;5;N` and `38;2;R;G;B`, including a `p=0` virtual placement and
   inherited row/column/MSB diacritics.
2. The diacritic table is generated from `rowcolumn-diacritics.txt` and tested
   at indices 0, 1, 2, 15, 255.
3. `a=c,i=1,r=N,c=M` composes full frames; explicit rectangles still work.
4. A feature query with any payload is answered `OK` and does not disturb a
   chunked transfer.
5. A relative placement whose parent is virtual uses the minimum placeholder
   cell as its anchor.
6. `prune`/`clear_visible` remove orphaned relative placements; margin scrolling
   moves parent and children together.
7. Any non-continuation command aborts an in-flight transfer; `a=f` continuation
   still works.
8. `q=1` suppresses a feature-query `OK`; errors do not echo keys.
9. `f=1` and `q=3` are answered rather than silently dropped (or the dead flag
   is removed and the behaviour documented).
10. The vte changes are committed and pushed, `Cargo.toml` points at a
    reproducible git rev/branch, and `Cargo.lock` is regenerated.
11. The status document's claims match the tests; the stale §4/§5 sections are
    removed or marked resolved with evidence.
12. A clean checkout builds and the full test suite passes without
    `/usr/local/bin/Xkitty`.

---

## 10. Appendix: snapshot and commands

```
branch:   kitty-graphics
HEAD:     83fa99d5 Document the kitty graphics protocol implementation status
dirty:    14 files, +1311/-104
sha256:   d57b78cb012d33f1b711830b665f41964deb532241c5a53c45507e2fe6b240ad
```

Commands used:

```sh
git status --short --branch
git diff --stat
git diff | sha256sum
cargo test --locked -p alacritty_terminal --test graphics
cargo test --locked -p alacritty_terminal --test kitty_animation
cargo test --locked -p alacritty_terminal --test kitty_clipboard
cargo check --locked -p alacritty
cd ../vte-forked && cargo test --locked
```

Reference material:

- Kitty graphics protocol — <https://sw.kovidgoyal.net/kitty/graphics-protocol/>
- Kitty 0.50.0 changelog — <https://sw.kovidgoyal.net/kitty/changelog/>
- `rowcolumn-diacritics.txt` —
  <https://sw.kovidgoyal.net/kitty/_downloads/f0a0de9ec8d9ff4456206db8e0814937/rowcolumn-diacritics.txt>
- Kitty reference implementation — `kitty/graphics.c`,
  `kitty/parse-graphics-command.h`
