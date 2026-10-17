# Kitty Graphics Protocol in Alacritty — Feasibility Analysis & Implementation Plan

> **Status: implemented.** Phases 0–3 of the plan below are complete and verified
> by rendering into an Xvfb display: a 96×96 PNG and a 64×64 PNG placed by
> `a=T` came out pixel-exact (9216 and 4096 px) alongside text. `cargo test
> --workspace` is green (317 tests). See the container `README.md` for the
> supported command set, configuration, and known limitations. Phase 4 (client
> conformance against `kitten icat`, `timg`, `chafa`) and phase 5 (hardening,
> `t=f`/`t=s`, zlib, placeholders, animation) remain outstanding.
>
> Design corrections found during implementation, which supersede the text below:
> 1. **There is no DCS variant of the kitty graphics protocol.** Every command is
>    APC (`ESC _ G … ESC \`); DCS is sixel. Only the APC parser change was needed.
> 2. **DCS was a red herring for this feature**, though it is indeed unusable in
>    Alacritty (see §2.3) — irrelevant to kitty graphics.
> 3. **A third blocker was discovered that the plan did not anticipate**: the text
>    renderer caches the bound `GL_TEXTURE_2D` name and skips redundant binds, so
>    uploading image textures behind its back makes it sample an incomplete
>    texture and all text disappears permanently. Fixed by adding
>    `TextRenderer::invalidate_texture_binding`. Any future code that touches
>    texture bindings from outside the text renderer must do the same.
> 4. **The core cannot resolve `c`/`r` auto-sizing**, because `Term` has no font
>    metrics (only rows/columns). Placements therefore store the requested cell
>    counts and the display layer resolves `0 = auto` at draw time. This is
>    arguably better than kitty's placement-time resolution, since images re-flow
>    when the font size changes.
> 5. The payload of `t=f`/`t=t`/`t=s` is a **path, not base64**, so unsupported
>    mediums must be refused *before* base64 decoding.
> 6. **`o=z` (zlib) is not optional**, contrary to the ranking in §1. `kitten icat`
>    compresses every transmission with it, so without zlib support the primary
>    client cannot display anything.
> 7. **Alacritty's primary device attributes reply blocks kitty clients.** In
>    kitty's `escape-code-parser.go` the CSI buffer *includes* the final byte, so
>    `ESC [ ? 6 c` yields a three-byte payload; `kitten icat` only quits its
>    capability probe on `len(payload) > 3`, so it waits out its timeout on any
>    terminal reporting a bare `6`. Corrected to `ESC [ ? 6 ; 22 c` (VT102 plus
>    ANSI colour — no capability is claimed that the terminal lacks). This is
>    independent of graphics support and would equally affect a stock Alacritty.

Status: analysis / proposal. Nothing in this document is implemented yet.
Base: `/workspaces/alacritty-fork` @ `54214706` (`alacritty 0.18.0-dev`, `alacritty_terminal 0.26.1-dev`).

---

## 0. Verdict

| Question | Answer |
|---|---|
| Is the kitty graphics protocol usable in Alacritty today? | **No.** Not a single protocol byte is visible to the terminal. |
| Is it technically feasible to add? | **Yes**, but it requires changing the VT parser dependency first. |
| Is it an Alacritty-only patch? | **No.** Two layers below Alacritty must change or be bypassed. |
| Will upstream Alacritty merge it? | **No.** The maintainer has declined graphics/sixel repeatedly and explicitly. |
| Estimated size | **~2.5–3.5k lines** of new/changed Rust across 2–3 crates (comparable to the rejected sixel PR). |
| Recommended target | This downstream fork, behind a config flag, with the diff kept isolated so rebasing stays cheap. |

The single most important finding: **the parser pinned by Alacritty (`vte 0.15.0`) discards APC strings before any Alacritty code can observe them**, and it *also* discards DCS payloads. Kitty graphics is APC-only (`ESC _ G … ESC \`). Therefore step one is not "parse graphics"; it is "make APC bytes reachable at all".

---

## 1. What the protocol actually requires

Kitty graphics has no DCS variant. Every command is:

```
ESC _ G <control-data> ; <base64-payload> ESC \
```

`control-data` is a comma-separated list of `key=value` pairs (~30 keys). Transport details that constrain the design:

* **Chunking.** Payloads are base64, split into chunks of ≤4096 bytes; all but the last must be a multiple of 4. `m=1` means more follows; `m=0` (default) is final. Only the first chunk carries the full control data; continuation chunks carry at most `m`, `q`, and (for animation) `a=f`. The cursor position that counts is the one **when the final chunk arrives**, and nothing may be displayed until the transfer is complete and validated. *Consequence: the implementation needs a resumable, multi-`apc_dispatch` state machine — the parser cannot hand us "one APC = one command".*
* **No `ESC` inside a payload.** Payload is base64 and control data is printable ASCII, so `ESC` is unambiguously the terminator. This is what makes a byte-scanning pre-parser (Option B below) tractable.
* **Responses.** `<ESC>_Gi=<id>[,I=<n>][,p=<p>];OK<ESC>\` on success, `<ESC>_Gi=<id>...;<CODE>:<message><ESC>\` on failure. Responses are sent **only when the command carried `i` or `I`**. `q=0` sends everything, `q=1` suppresses `OK`, `q=2` suppresses everything.
* **`a=q` must be answered immediately**, before other input is processed. The canonical capability probe is `<ESC>_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA<ESC>\<ESC>[c`; a client concludes "unsupported" if DA1 arrives with no graphics reply.
* **Placements are anchored to cells**, not to pixels: kitty records `start_row`/`start_column` at put time plus effective rows/cols; scrolling is a transform on `start_row`; placements scrolling entirely past the limit are destroyed; with DECSTBM margins only refs entirely inside the region scroll and partially-exiting refs are clipped.
* **Z-order.** `z` is signed. `z < 0` draws under text; `z ≥ 0` over text; `z < INT32_MIN/2` draws under cells with a non-default background. Ties break by lower image id.
* **Lifecycle.** Re-transmitting the same `i` deletes the old image and all its placements. `(i,p)` re-put replaces in place without flicker. Graphics are cleared on RIS, on 1049 alt-screen switch, and on `CSI 2J`. Kitty keeps **separate stores for main and alt screens**.

### Minimum viable subset

Ranked by what real clients actually exercise (`kitten icat`, `timg`, `chafa --format=kitty`, `yazi`, `viuer`, `image.nvim`):

**MVP (must have)**
1. APC `G` parsing with correct key/value grammar and strict chunk rules.
2. `a=T` + `f=100` (PNG), chunked, over `t=d` (direct). This alone covers icat/timg/chafa/yazi/viuer.
3. `i=` addressing, `a=q` probe, `q=0/1/2`, correct `OK` / `<CODE>:<msg>` responses.
4. `a=p` placements, `p=`, `c`/`r` sizing, `z` layering, `C=1` (do not move cursor).
5. `a=d` for at least `d=a/A`, `i/I` (with `p`), `c/C`, `p/P`, `z/Z`, plus implicit clear on RIS / `CSI 2J` / alt-screen switch / scroll-off.
6. Scroll-follow of placements, a storage quota with eviction, and `U+10EEEE` rendered as *something benign* rather than a tofu box.

**Important (phase 5+)**
`f=24`/`f=32` raw with `s`/`v`; source rects `x/y/w/h`; intra-cell `X/Y`; `I=` image numbers; `t=f`/`t=s`; `o=z` (zlib); `U=1` Unicode placeholders.

**Optional / expensive**
All animation (`a=f`, `a=a`, `a=c`, `d=f/F`, frame quotas), relative placements (`P/Q/H/V`), indexed `f=8/16`, `N=1`.

---

## 2. Current state of Alacritty: evidence

### 2.1 What exists

| Layer | State |
|---|---|
| Kitty **keyboard** protocol | Fully supported. `TermMode::KITTY_KEYBOARD_PROTOCOL` (`alacritty_terminal/src/term/mod.rs:81`), handlers gated on `TermConfig.kitty_keyboard` (`:350`), hardcoded `true` in `UiConfig::term_options()` (`alacritty/src/config/ui_config.rs:126`). |
| Kitty **graphics** protocol | **Absent.** Repo-wide search for `sixel`, `graphics protocol`, `inline image` in `alacritty/src` + `alacritty_terminal/src` returns zero hits. |
| `extra/man/alacritty-escapes.7.scd` | Documents DCS as **Rejected** (`extra/man/alacritty-escapes.7.scd:258`). No APC entry. |
| Changelogs | Zero graphics/image/APC entries. "kitty" appears only for the keyboard protocol. |

### 2.2 Blocker 1 — APC is unobservable

Alacritty does **not** implement `vte::Perform`. It implements the higher-level `vte::ansi::Handler` (`alacritty_terminal/src/term/mod.rs:1059`), and vte's internal `Performer` maps `Perform` → `Handler`.

In `vte 0.15.0` (the exact pin: `alacritty_terminal/Cargo.toml:28`, `Cargo.lock:2124`):

* `State::SosPmApcString` is entered for `0x58`, `0x5E..=0x5F` and dispatched through `anywhere()`, which reacts only to `CAN`/`SUB`/`ESC` and **silently drops every other byte** (`vte-0.15.0/src/lib.rs:182, 438`).
* Neither `Perform` nor `Handler` has any `apc*` method. Verified directly:

```
$ grep -rn "fn apc" ~/.cargo/registry/src/*/vte-0.15.0/src/
NONE — no apc callback exists
```

`Perform` exposes exactly: `print`, `execute`, `hook`, `put`, `unhook`, `osc_dispatch`, `csi_dispatch`, `esc_dispatch`, `terminated`.

**Reproduced empirically** with a standalone `vte 0.15.0` probe (recording `Perform` impl):

```
--- APC kitty graphics ---
input: "\x1b_G f=32,s=1,v=1,a=T,i=1;AQIDBA==\x1b\\"
  esc(interm=[], byte=0x5c '\')
  >>> NO payload delivered for APC

--- APC then text ---
input: "\x1b_G a=q,i=1\x1b\\HELLO"
  esc(interm=[], byte=0x5c '\')
  print('H') print('E') print('L') print('L') print('O')
  >>> NO payload delivered for APC
```

Only the terminating `ESC \` surfaces, as `esc_dispatch(0x5C)`. The parser recovers cleanly, but every protocol byte is gone.

### 2.3 Blocker 2 — DCS is also unobservable (relevant to sixel, not to kitty graphics)

DCS bytes *do* reach `Perform::hook`/`put`/`unhook`, but the `vte::ansi::Performer` adapter implements those as `debug!`-only no-ops (`vte-0.15.0/src/ansi.rs:1311-1327`), and `Handler` has no `hook`/`put`/`unhook` at all. So DCS payloads never reach `Term` either.

This does not block kitty graphics (APC-only), but it means:
* sixel remains impossible without a second parser change, and
* if a later phase wants DCS support, the same patch shape applies with ~10 additional lines.

### 2.4 Upstream position

| Ref | Status | Notes |
|---|---|---|
| [alacritty#910](https://github.com/alacritty/alacritty/issues/910) "Add support for libsixel" | **open** since 2017-11-21, 238 comments, 684 reactions | Canonical thread; no dedicated kitty-graphics issue exists. |
| [alacritty#4763](https://github.com/alacritty/alacritty/pull/4763) "Support for graphics in the terminal" | **open, deliberately unmerged** since 2021-02-05, +2882/−100, 33 files | Sixel; maintained by author but never merged. |
| [vte#110](https://github.com/alacritty/vte/pull/110), [vte#115](https://github.com/alacritty/vte/pull/115) | **closed unmerged** (2024-09-21; 2026-07-12 "Closing due to inactivity") | APC support attempts, including a perf-reviewed `opaque_start/opaque_put/opaque_end` design. |
| Latest published `vte` | **still 0.15.0** | `alacritty/vte` master still drops APC. |

Maintainer (chrisduerr), verbatim: *"First of all the protocol is garbage. … I also couldn't care less about images in the terminal… ~3k lines is a significant portion of Alacritty's code, so I would consider this 'not simple'… I don't see how one could justify upstreaming this."* (2024-05-29), and *"Opting out of performance is not something that aligns with Alacritty's goals"* — rejecting even a disabled-by-default toggle.

**Implication for this fork:** build it here, isolate it, and do not spend effort on an upstream PR. Two project-wide requirements do carry over from that stance and should be honoured as design constraints: (a) **zero cost when disabled**, and (b) **no regressions when enabled** in the common no-graphics path.

Every terminal that supports kitty graphics modified its own parser to expose APC: kitty (own parser), WezTerm (`termwiz`), Ghostty (own parser with explicit `apc_start`/`apc_put`/`apc_end` states). None of them bolted on an out-of-band scanner.

---

## 3. Solving the blocker: three options

### Option A — Fork `vte`, add a streaming APC API (recommended)

Land the change reviewers asked for in vte#115: **streaming** callbacks, not whole-payload buffering, so an arbitrarily large payload costs O(1) parser memory.

```rust
// vte/src/lib.rs — Perform additions (all defaulted, non-breaking)
fn apc_start(&mut self) {}
fn apc_put(&mut self, _byte: u8) {}
fn apc_end(&mut self) {}

// state machine: split SosPmApcString so APC is forwarded, SOS/PM still discarded
0x58 | 0x5E => self.state = State::SosPmString,       // ESC X, ESC ^  (unchanged behaviour)
0x5F       => { performer.apc_start(); self.state = State::ApcString },

fn advance_apc<P: Perform>(&mut self, performer: &mut P, byte: u8) {
    match byte {
        0x18 | 0x1A => { performer.apc_end(); performer.execute(byte); self.state = State::Ground }
        0x9C        => { performer.apc_end(); self.state = State::Ground }
        0x1B        => { performer.apc_end(); self.reset_params(); self.state = State::Escape }
        _           => performer.apc_put(byte),
    }
}
```

Mirror the same three methods onto `vte::ansi::Handler` and forward them in `impl Perform for Performer`. Because `Handler` methods are all defaulted, this is source-compatible for every existing implementor — which is exactly the shape that makes such a patch reviewable.

* **Pros:** correct by construction at every buffer boundary; no second pass over terminal output; ~40 lines in vte; unlocks DCS/sixel later with ~10 more; matches every other terminal's architecture.
* **Cons:** a second repository to maintain (mitigated: `[patch.crates-io]` is already used for `x11-clipboard` in this workspace's `Cargo.toml`).
* **Note:** `alacritty_terminal/src/lib.rs:20` already does `pub use vte;`, so a patched vte slots in with no import churn.

### Option B — In-repo pre-parser at the PTY boundary

Intercept bytes in `pty_read` (`alacritty_terminal/src/event_loop.rs:154`) before `state.parser.advance(...)`, extract `ESC _ G … ESC \`, handle it, and forward the remainder to vte. Carry partial sequences in `EventLoop::State` (next to `parser: ansi::Processor`, `:404`).

Sound because `ESC` always terminates any VT string, and `ESC` cannot appear inside a kitty payload — a naive scan cannot be fooled by a false start inside OSC/DCS.

* **Pros:** single repository; no dependency fork.
* **Cons:** a second scanning pass over **every** byte of terminal output (alacritty is performance-sensitive; `READ_BUFFER_SIZE` is 1 MiB); must re-implement boundary/partial-sequence handling and interact correctly with `sync_bytes_count()` and the `processed`/`unprocessed` accounting; divergent VT semantics risk (e.g. C1 `0x9C` ST, `CAN`/`SUB`).
* **Perf mitigation if chosen:** `memchr` for `0x1B`, fast-path the "no ESC in buffer" case, and copy only when a sequence is actually found. Never worth it if Option A is available.

### Option C — Replace the parser wholesale

Adopt `termwiz`/`wezterm-escape-parser` or write a new parser. Rejected: enormous blast radius, and Alacritty's `Handler` surface would have to be re-mapped.

### Decision matrix

| | A: fork vte | B: pre-parser |
|---|---|---|
| Correctness at boundaries | by construction | manual, error-prone |
| Bytes touched per read | 1× | 2× |
| Repos to maintain | 2 | 1 |
| Unlocks DCS/sixel later | yes (~10 lines) | yes (second scanner) |
| Upstreamable shape | yes | no |
| Risk of divergence from vte semantics | none | moderate |

**Recommendation: Option A.** Fall back to B only if a hard single-repo constraint exists.

---

## 4. Target architecture

Five layers. Only L1 and L5 are cross-cutting; L2–L4 are additive modules.

```
L1 Parser      vte patch: apc_start/apc_put/apc_end  →  Term
L2 Protocol    alacritty_terminal/src/graphics/  : command grammar, chunk assembly,
               base64/zlib/PNG decode, responses, quota  →  Event::PtyWrite
L3 Core state  GraphicsStore on Term (per screen): images + placements,
               scroll/resize anchoring, damage
L4 Render      alacritty/src/renderer/images.rs : texture cache + z-partitioned quad pass
L5 Config      [terminal.graphics] : enabled, limits
```

### 4.1 Data model (L2/L3)

```rust
// alacritty_terminal/src/graphics/mod.rs
pub struct GraphicsStore {
    images: HashMap<u32, Arc<ImageData>>,   // client id -> image
    images_by_number: HashMap<u32, u32>,    // I= number -> newest client id
    placements: Vec<Placement>,             // anchored to the grid
    loading: Option<LoadState>,             // in-flight chunked transfer
    quota: usize,
    used: usize,
    next_internal_id: u32,
}

pub struct ImageData {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<[u8]>,   // RGBA8, premultiplied on CPU at ingest
    pub generation: u64,     // bumped on re-transmit, invalidates GPU textures
}

pub struct Placement {
    pub image_id: u32,
    pub placement_id: u32,
    pub anchor: Point<Line, Column>, // absolute grid line/column at put time
    pub offset_x: u16,               // intra-cell pixels (X/Y)
    pub offset_y: u16,
    pub cols: u16,
    pub rows: u16,
    pub src: SrcRect,                // x/y/w/h source rectangle
    pub z: i32,
    pub is_virtual: bool,            // U=1 placeholder prototype
}
```

**Why not `CellExtra`?** There *is* a per-cell attachment mechanism — `Cell.extra: Option<Arc<CellExtra>>` (`alacritty_terminal/src/term/cell.rs:132-146`), used for zero-width chars, underline colour and OSC 8 hyperlinks. It is the wrong home for placements:

* `Cell::is_empty` (`cell.rs:233`) ignores `extra`, so an image-carrying cell is still "empty" and can be dropped by `clear_viewport`/`LineLength`/`Row::shrink`.
* `Cell::reset` (`cell.rs:259`) drops `extra` outright.
* Column reflow (`grid/resize.rs`) splits and merges rows cell-by-cell, silently relocating any cell-anchored id.

Keep placements in a `Term`-level store with explicit transforms, mirroring how `selection` and `vi_mode_cursor` are already handled — they are *not* stored in cells either.

### 4.2 Scroll / resize anchoring (L3)

The precedent is already in the codebase: `Term::scroll_up_relative`/`scroll_down_relative` (`term/mod.rs:770`/`:742`) call `self.selection.rotate(&self, &region, lines)` before `grid.scroll_up`, and `Term::resize` (`:655`) **invalidates the selection on column change** (`:684-687`) and rotates it on row-only change (`:689-692`).

Mirror exactly that:

* `placements.scroll(&region, lines)` — move anchors inside the region by the delta; drop anything pushed past the history limit.
* `placements.on_resize(...)` — on **row-only** change, rotate like the selection; on **column** change, **drop placements** (same policy the selection already uses, because reflow breaks cell anchors). This is a defensible v1 and matches existing project behaviour for the closest analogous feature.
* `Term::swap_alt` (`:707`) and `reset_state` (`:1835`) must clear/handle graphics. Kitty keeps separate main/alt stores; the simplest faithful v1 is one store per screen, swapped alongside `grid`/`inactive_grid`.

### 4.3 Damage (L3)

`TermDamage` is line-based only — `LineDamageBounds { line, left, right }` (`term/mod.rs:136`) — with `Full` as the escape hatch. For v1, **any graphics mutation calls `mark_fully_damaged()`**. That is correct, and scrolling already does exactly this (`:789`). This matters beyond debug overlays: the Wayland path derives swap damage from grid lines plus `FrameDamage::add_viewport_rect` (`alacritty/src/display/damage.rs:176`), so under-damaging would leave stale image pixels on screen. Per-region optimization is a later phase.

### 4.4 Rendering (L4)

Model the new pass on `RectRenderer` (`alacritty/src/renderer/rects.rs:248`), which is the closest analogue: it resets the viewport to the full window, uses absolute pixels, and overrides/restores blend state (`renderer/mod.rs:243-265`).

Additions over `RectRenderer`: a `uv: [f32; 2]` vertex attribute, a `sampler2D` uniform, and a per-texture bind or batch keyed by texture id — mirroring `TextRenderBatch::tex()` batching (`renderer/text/mod.rs:94-133`).

Texture upload can reuse the exact pattern already in the glyph atlas (`renderer/text/atlas.rs:73-101` for `GenTextures`/`TexImage2D`/`TexParameteri`, `:147-196` for `TexSubImage2D`). For images, one texture per image via `TexImage2D` avoids the atlas's shelf-packing and avoids `UNPACK_ROW_LENGTH` (unavailable on GLES2 without `EXT_unpack_subimage`).

The renderer must also add a **texture cache** (`HashMap<u32, TextureEntry { gl_id, w, h, generation, last_used }>`), evicted on `ImageData::generation` change and by LRU/quota, with `DeleteTextures` on drop. Ownership follows the existing split: `Term`/core owns the raster, `Renderer` owns GPU resources (as with `GlyphCache` in `Display` and atlases in the renderer).

**Draw ordering — a genuinely nice fit.** Alacritty paints cell backgrounds and glyphs in a *single* pass (`renderer.draw_cells`, `display/mod.rs:878`), so there is no natural seam between "above background" and "below glyph" … except that the default background emits `bg_alpha == 0.0` (`display/content.rs:388-390`) and the text shader **discards** those fragments (`res/glsl3/text.f.glsl:41-44`, `res/gles2/text.f.glsl:42-45`). Therefore:

* `z < 0` → draw **after** `renderer.clear` (`display/mod.rs:838`), **before** `draw_cells`.
* `z >= 0` → draw **after** `draw_cells`.

Negative-z images then show through default-background cells and are occluded only by glyphs and explicitly-coloured cells — which is the intended semantics.

*Known deviation:* kitty renders plain-negative-z images *above* non-default cell backgrounds but *below* glyphs. Alacritty cannot express that without splitting the background and glyph passes. Document it; fixing it is a much larger change to the text renderer.

**Blending.** Rects use straight alpha (`SRC_ALPHA, ONE_MINUS_SRC_ALPHA`); the glyph atlas uses premultiplied RGBA. Premultiply image pixels once at ingest (matching the atlas) and draw with `BlendFunc(ONE, ONE_MINUS_SRC_ALPHA)` — this also avoids halos from any non-opaque PNG. Set and restore viewport/blend/program around the pass, exactly as `draw_rects` does.

**Shaders.** Both variants are needed, since the codebase ships GLES2 and GLSL3 paths for text and rects (`res/gles2/`, `res/glsl3/`, selected by `#if defined(GLES2_RENDERER)`). NPOT textures are fine on both paths with `CLAMP_TO_EDGE` and no mipmaps.

**Unicode placeholders (`U+10EEEE`)** are out of MVP scope, but the MVP requirement is that the codepoint is **not** drawn as an ordinary glyph — render it blank/`U+FFFD` until placeholders are implemented.

### 4.5 Decoding and where it happens

* `base64` is already a dependency of `alacritty_terminal`.
* `png` **is already in the tree** but as `optional = true`, enabled only by the `x11` feature, and *not declared at all on macOS* (`alacritty/Cargo.toml:58, 90-96`); it is currently used only to decode the window icon (`display/window.rs:298-305`). Portable `f=100` requires promoting it to an unconditional dependency. `default-features = false` is already proven sufficient for decoding.
* `o=z` (zlib) needs `flate2`/`miniz_oxide`; treat as phase 5.
* **Threading.** `EventLoop` is per-window, but a multi-megabyte PNG decode on the reader thread stalls *that window's* output processing. v1 decodes inline (simple); phase 5 moves decode off the parse path by handing `Arc<[u8]>` compressed bytes to the display thread and decoding on first upload, with `ImageData::generation` guarding cache invalidation.

### 4.6 Responses

Emit via `Event::PtyWrite` (`alacritty_terminal/src/event.rs:40`) from `Term`, exactly like `identify_terminal` (`term/mod.rs:1257-1271`), handled at `alacritty/src/event.rs:1926`.

Ordering is what makes the capability probe work: `PtyWrite` events are queued in stream order, so an `a=q` reply lands before the DA1 reply for the `ESC [c` that follows it. Immediacy is therefore achieved by ordering, not by synchronous PTY writes.

> **Critical trap.** A terminal that *partially* implements graphics is worse than one that ignores it: clients that gate on the `a=q` ack will **block indefinitely** if the reply never comes. Either implement responses correctly, or do not consume the sequence at all.

### 4.7 Config (L5)

```toml
[terminal.graphics]
enabled = true
max_image_dimension = 10000   # px, matching kitty's MAX_IMAGE_DIMENSION
memory_limit = 134217728      # bytes, ~128 MiB
```

Plumb through `TermConfig` beside `kitty_keyboard` (`alacritty_terminal/src/term/mod.rs:350`, `alacritty/src/config/ui_config.rs:118-127`). Must be checked on live config reload, as `kitty_keyboard` is (`term/mod.rs:518-521`).

### 4.8 Security

`t=f` (file), `t=t` (temp file) and `t=s` (shared memory) let a client ask the terminal to read files with the terminal's privileges, with an error-message oracle. Kitty's mitigations: pre-check paths (refuse `/proc`, `/sys`, `/dev` *before* opening), `O_NONBLOCK`, `fstat` regular-file check, refuse non-regular files, unlink `t=t` files only when the path contains `tty-graphics-protocol` and lives in a known temp dir, and return **one identical error** (`EBADF:Failed to read image file`) for every read failure.

**Recommendation: MVP supports `t=d` only** and returns an error for other mediums. Add file/SHM later with the full checklist, ideally behind a config flag defaulting off.

---

## 5. Phased implementation plan

### Phase 0 — Fork scaffolding (0.5d)

* Decide Option A vs B (§3). Create the parser branch/commit and wire `[patch.crates-io] vte = { git = …, branch = "apc" }` alongside the existing `x11-clipboard` patch in the workspace `Cargo.toml`.
* Add the `graphics` config skeleton (default **off**) so every later phase is opt-in and measurable.
* **Exit:** workspace builds; `cargo test -p alacritty_terminal` green; no behaviour change with the flag off.

### Phase 1 — Make APC reachable (1–2d) — *the gate*

* vte: add `apc_start`/`apc_put`/`apc_end` to `Perform` and `ansi::Handler`; add `State::ApcString`; forward in `Performer`. Keep SOS/PM behaviour unchanged so nothing else regresses.
* `impl Handler for Term` (`term/mod.rs:1059`): forward into a stub `graphics::ApcReceiver` that logs the payload.
* Tests: APC split across arbitrary byte boundaries (including 1-byte-at-a-time), APC with no terminator, APC followed immediately by text, `CAN`/`SUB` abort, `ESC` + non-`\` malformed cases, and a regression assertion that SOS/PM are still discarded.
* **Exit:** feeding `<ESC>_G f=32,s=1,v=1,a=T,i=1;AQIDBA==<ESC>\` produces the exact payload `G f=32,s=1,v=1,a=T,i=1;AQIDBA==` in the log; existing vte/Alacritty tests unchanged.

### Phase 2 — Protocol core (5–8d)

* `alacritty_terminal/src/graphics/{mod.rs, command.rs, store.rs}`: key/value grammar with kitty's strict state machine (`KEY → EQUAL → VALUE → AFTER_VALUE`), 30-key table, `m=` chunk assembly, base64 decode, `t=d` only, `f=24/32/100`, `a=t|T|p|d|q`, `i/I/p`, `q=0/1/2`, responses, `C=1`, `c/r/X/Y/z`, quota + eviction.
* Promote `png` to an unconditional dependency.
* Attach `GraphicsStore` to `Term`; `mark_fully_damaged()` on every mutation; clear on `reset_state`/`swap_alt`/`CSI 2J`.
* Tests: table-driven grammar tests (valid + every malformed class), chunk-boundary matrix, incomplete-transfer-never-displays, response strings byte-for-byte, delete semantics per `d` value, quota/eviction, and `q=2` silence.
* **Exit:** all MVP commands correct at the store level, verified by unit tests with no renderer involved.

### Phase 3 — Renderer (4–6d)

* `alacritty/src/renderer/images.rs`: `ImageRenderer` (VAO/VBO/shaders for both GLES2 and GLSL3), texture cache with generation-based invalidation, LRU + quota eviction, `DeleteTextures` on drop.
* `Renderer` gains an `image_renderer` field and a `draw_images(...)` method mirroring `draw_rects`'s viewport/blend save-restore.
* `Display::draw`: two insertion points around `draw_cells` (§4.4); cell→pixel conversion via `padding_x + col*cell_width` (`display/cursor.rs:18-21`), rows viewport-relative via `term::point_to_viewport(display_offset, anchor)`.
* `RenderableContent`/`Term` gain a `graphics()` accessor so `Display` can enumerate visible placements.
* **Exit:** `kitten icat image.png` renders a correct image that scrolls with the text, images blur/restore correctly, and mixed z-order is correct.

### Phase 4 — Client conformance (3–5d)

Test against real clients and fix what breaks: `kitten icat`, `timg`, `chafa --format=kitty`, `yazi`/`ranger` previews, `viuer`, and an `image.nvim`/`neovim` case (exercise `I=` python and placeholders). Prioritise anything that hangs over anything that renders oddly.
* **Exit:** every listed client renders and terminates cleanly; no client hangs.

### Phase 5 — Hardening (5–10d)

* `t=f`/`t=t`/`t=s` with the full security checklist (§4.8), config-gated.
* `o=z` zlib; off-parse-thread decode; per-region damage instead of full damage; texture cache tuning.
* `I=` image numbers; `U=1` Unicode placeholders incl. the `rowcolumn-diacritics` table.
* Animation (`a=f`/`a=a`/`a=c`) if desired.
* Docs: `extra/man/alacritty-escapes.7.scd` APC entry, CHANGELOG, `docs/features.md`.

---

## 6. Testing strategy

1. **Unit tests in the parser and store** — table-driven grammar, chunk boundaries, delete matrix, quota. Fast, deterministic, no GPU.
2. **Recording-based regression tests** — reuse the existing `tests/ref/` harness (`alacritty_terminal/tests/ref.rs:112` builds a `Term` and calls `ansi::Processor::advance` over `alacritty.recording`). The `hyperlinks` case is the template. Note the ref-test writer captures **raw bytes before parsing** (`event_loop.rs:150`), so APC traffic is recorded faithfully. The golden `grid.json` does not serialize placements, so either extend the serializer or assert on the store directly in an adjacent test.
3. **Cross-implementation oracles** — WezTerm and Ghostty both implement this protocol; when behaviour is ambiguous (e.g. the spec's inverted `c`/`r` description for `a=c`; `q=2` semantics), treat them as secondary references and prefer kitty's C implementation.
4. **Manual visual checklist** — CI here has no GPU, so rendering correctness is verified by hand for each phase-3/4 client. Document the checklist in the PR.

---

## 7. Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Wrong/absent responses hang clients | **High** | Implement responses fully in phase 2, or consume nothing; test `a=q` first. |
| Upstream rejection ⇒ perpetual rebase | High | Isolate: one vte patch, one new module + one field on `Term`, one renderer module, one config section. Keep the flag off by default. |
| Reflow moves text under images | Medium | Mirror the existing selection policy: drop placements on column change (§4.2). Document as a limitation. |
| Decode stalls terminal output | Medium | Phase 5: decode off the parse path; v1 documented. |
| Wayland under-damage leaves stale pixels | Medium | Full damage on every graphics mutation in v1. |
| GLES2 path breakage (shaders, NPOT, `UNPACK_ROW_LENGTH`) | Medium | Both shader variants; one texture per image; validate on a GLES2/EGL device. |
| File/SHM transmission is a security hole | **High** | Defer past MVP; then path pre-checks, `O_NONBLOCK`, regular-file check, identical error strings, config-gated. |
| Memory blowup from large/many images | Medium | Quota + eviction from phase 2; `MAX_IMAGE_DIMENSION` validation before allocation. |
| Plain-negative-`z` renders under non-default cell backgrounds | Low | Known deviation (§4.4); requires splitting the text pass to fix. |
| Perf regression when disabled | Medium | Flag checked before touching the parser; benchmark the no-APC read path. |

---

## 8. Effort estimate

| Phase | Deliverable | New/changed LOC (approx.) |
|---|---|---|
| 0 | Scaffolding, config skeleton | ~50 |
| 1 | vte APC patch + Handler wiring + tests | ~200 (vte) + ~150 |
| 2 | Command grammar, chunking, store, responses, quota | ~900–1200 |
| 3 | Texture cache, image renderer, shaders, draw order | ~500–700 |
| 4 | Client conformance fixes | ~200–400 |
| 5 | Security, zlib, placeholders, animation, damage tuning | ~600–1200 |
| — | Tests throughout | ~500–800 |
| **Total** | | **~3.0–4.5k lines** over 2–3 crates |

For calibration: the rejected sixel PR (#4763) was +2882/−100 across 33 files. This estimate is in the same range, which is precisely why it belongs in a fork rather than an upstream PR.

---

## 9. Appendices

### A. Reproducing the blocker

```sh
mkdir -p /tmp/vte-probe/src && cd /tmp/vte-probe
# Cargo.toml: vte = { version = "0.15.0", default-features = false, features = ["std","ansi"] }
# main.rs: a recording `Perform` impl; feed "\x1b_G f=32,s=1,v=1,a=T,i=1;AQIDBA==\x1b\\"
cargo run --offline
# => prints only esc(byte=0x5c); no payload callbacks fire
```

### B. Key line anchors

| What | Where |
|---|---|
| vte pin | `alacritty_terminal/Cargo.toml:28`, `Cargo.lock:2124` |
| vte re-export | `alacritty_terminal/src/lib.rs:20` |
| APC dropped in state machine | `vte-0.15.0/src/lib.rs:182, 438` |
| DCS dropped in Performer | `vte-0.15.0/src/ansi.rs:1311-1327` |
| `impl Handler for Term` | `alacritty_terminal/src/term/mod.rs:1059` |
| `Term` struct / `Config` | `alacritty_terminal/src/term/mod.rs:268, 332` |
| `Term::resize` / `swap_alt` / `reset_state` | `term/mod.rs:655, 707, 1835` |
| scroll + `selection.rotate` hook | `term/mod.rs:770, 742` |
| damage API | `term/mod.rs:136, 178, 458` |
| `CellExtra` | `alacritty_terminal/src/term/cell.rs:132-146, 233, 259` |
| `Event::PtyWrite` | `alacritty_terminal/src/event.rs:40` |
| parser instance + parse call | `alacritty_terminal/src/event_loop.rs:404, 154` |
| ref-test writer (raw bytes) | `alacritty_terminal/src/event_loop.rs:150` |
| ref-test harness | `alacritty_terminal/tests/ref.rs:112` |
| `Display::draw` order | `alacritty/src/display/mod.rs:775, 838, 878, 990` |
| default bg alpha 0 / shader discard | `alacritty/src/display/content.rs:388-390`, `res/glsl3/text.f.glsl:41-44`, `res/gles2/text.f.glsl:42-45` |
| `Renderer` fields / `draw_rects` | `alacritty/src/renderer/mod.rs:89, 243` |
| `RectRenderer` template | `alacritty/src/renderer/rects.rs:248, 235, 320` |
| texture upload pattern | `alacritty/src/renderer/text/atlas.rs:73, 147` |
| cell→pixel conversion | `alacritty/src/display/cursor.rs:18-21` |
| viewport damage rect | `alacritty/src/display/damage.rs:176` |
| `png` optionality | `alacritty/Cargo.toml:58, 90-96` |
| config plumbing precedent | `alacritty/src/config/ui_config.rs:118-127` |
| `[patch.crates-io]` precedent | workspace `Cargo.toml` |

### C. References

* Kitty graphics protocol — <https://sw.kovidgoyal.net/kitty/graphics-protocol/>
* kitty implementation — <https://github.com/kovidgoyal/kitty> (`kitty/graphics.c`, `parse-graphics-command.h`, `base64.h`)
* Alacritty issue #910 — <https://github.com/alacritty/alacritty/issues/910>
* Alacritty PR #4763 (sixel) — <https://github.com/alacritty/alacritty/pull/4763>
* vte PR #115 (APC) — <https://github.com/alacritty/vte/pull/115>
* Ghostty graphics — <https://github.com/ghostty-org/ghostty> (`src/terminal/kitty/`)
* WezTerm graphics — <https://github.com/wezterm/wezterm> (`term/src/terminalstate/kitty.rs`)
